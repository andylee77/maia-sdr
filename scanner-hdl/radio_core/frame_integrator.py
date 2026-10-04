#
# Fishball radio core - maia_hdl's spectrum integrator with its mode and integrations taken per
# frame (doc/changes/082, item 5), and what the frame header needs to know about each frame.
#
# Upstream's integrator reads `peak_detect` live at two stages: the power stage (CpwrPeak, whose
# DSP48 sequence differs between the modes) and the write-back three samples later. A mode
# that changes between them spoils the samples in flight: from average to peak, the DSP48's
# average sequence leaves `is_greater` low and the peak write-back then skips those bins. Here
# a frame's mode is taken when the frame's first transform arrives and travels with each sample,
# so every stage applies the mode of the sample it holds. `nint` is taken there as upstream does.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

from maia_hdl.spectrum_integrator import SpectrumIntegrator


class FrameIntegrator(SpectrumIntegrator):
    """SpectrumIntegrator with a mode per frame.

    The parameters, ``model`` and the other attributes are ``SpectrumIntegrator``'s.

    Attributes
    ----------
    peak_detect : Signal(), in
        Peak hold (else average power) for the frames that start from now on.
    alternate : Signal(), in
        Each frame takes the other mode from the frame before it (``peak_detect`` is ignored).
    frame_start : Signal(), out
        High (with ``clken``) while the last sample of a frame is at the input: the next sample
        is the first of the next frame.
    closed_peak, closed_transforms, closed_aborted : out
        The frame that the last ``frame_start`` closed: its mode, the transforms it integrated,
        and whether an abort ended it before its integrations were done. They hold until the
        next ``frame_start``, so they describe the frame that ``done`` announces.
    """
    def __init__(self, domain_3x, input_width, input_fp_width, nint_width, fft_order_log2):
        super().__init__(domain_3x, input_width, input_fp_width, nint_width, fft_order_log2)
        self.alternate = Signal()
        self.frame_start = Signal()
        self.closed_peak = Signal()
        self.closed_transforms = Signal(nint_width)
        self.closed_aborted = Signal()

    def elaborate(self, platform):
        m = Module()
        m.submodules.to_fp = to_fp = self.to_fp
        m.submodules.common_exp = common_exp = self.common_exp
        m.submodules.cpwr = cpwr = self.cpwr

        mems = [Memory(shape=self.sumw+self.ew, depth=2**self.order_log2,
                       init=[])
                for _ in range(2)]
        m.submodules.mem0 = mems[0]
        m.submodules.mem1 = mems[1]
        rdports = [mem.read_port() for mem in mems]
        # BRAM output register
        rdports_reg = [Signal(self.sumw+self.ew, name=f'rdport{j}_reg',
                              reset_less=True)
                       for j in range(2)]
        for j in range(2):
            with m.If(rdports[j].en):
                m.d.sync += rdports_reg[j].eq(rdports[j].data)
        wrports = [mem.write_port() for mem in mems]

        # The BRAM's read (with its output register) and the conversion to floating point
        # take the same time, so the accumulator and the input meet at the common exponent.
        mem_delay = 2
        assert mem_delay == to_fp.delay
        # From the input to the power stage's input, and to the write-back.
        power_delay = mem_delay + common_exp.delay
        processing_delay = power_delay + cpwr.delay

        read_counter_rst = 0
        read_counter = Signal(self.order_log2, init=read_counter_rst)
        write_counter_rst = (
            (read_counter_rst - processing_delay) % 2**self.order_log2)
        write_counter = Signal(self.order_log2, init=write_counter_rst)
        sum_counter = Signal(self.nw)
        not_first_sum = Signal()
        not_first_sum_delay = Signal(mem_delay)
        pingpong = Signal()
        pingpong_delay = Signal(processing_delay, reset_less=False)
        pingpong_q = Signal(reset_less=False)
        do_abort = Signal()

        # The mode of the frame at the input, and of the sample at each stage after it:
        # peak_delay[j] holds the mode of the sample that was at the input j + 1 samples ago.
        frame_peak = Signal()
        peak_delay = Signal(processing_delay)
        transforms = Signal(self.nw)
        natural_end = Signal()
        m.d.comb += [
            natural_end.eq((sum_counter == 1) | (sum_counter == 0)),
            self.frame_start.eq(self.clken & self.input_last & (natural_end | do_abort)),
        ]

        with m.If(self.clken):
            m.d.sync += [
                read_counter.eq(read_counter + 1),
                write_counter.eq(write_counter + 1),
                pingpong_delay.eq(Cat(pingpong, pingpong_delay[:-1])),
                peak_delay.eq(Cat(frame_peak, peak_delay[:-1])),
                not_first_sum_delay.eq(
                    Cat(not_first_sum, not_first_sum_delay[:-1])),
            ]

            with m.If(self.input_last):
                # An FFT vector ends
                m.d.sync += [
                    read_counter.eq(read_counter_rst),
                    write_counter.eq(write_counter_rst),
                    not_first_sum.eq(1),
                    sum_counter.eq(sum_counter - 1),
                    transforms.eq(transforms + 1),
                    self.nearly_end.eq(0),
                ]
                with m.If(natural_end | do_abort):
                    # A new sum starts
                    m.d.sync += [
                        sum_counter.eq(self.nint),
                        not_first_sum.eq(0),
                        pingpong.eq(~pingpong),
                        do_abort.eq(0),
                        frame_peak.eq(Mux(self.alternate, ~frame_peak, self.peak_detect)),
                        transforms.eq(0),
                        self.closed_peak.eq(frame_peak),
                        self.closed_transforms.eq(transforms + 1),
                        self.closed_aborted.eq(do_abort & ~natural_end),
                    ]
                with m.If((sum_counter == 2)):
                    m.d.sync += [
                        self.nearly_end.eq(1),
                    ]

        with m.If(self.abort):
            m.d.sync += do_abort.eq(1)

        m.d.sync += pingpong_q.eq(pingpong_delay[-1])

        # The read and write counters are reversed to perform bit order
        # inversion in the FFT indices. Moreover, the MSB is negated to perform
        # fftshift.
        read_counter_rev = read_counter[::-1]
        write_counter_rev = write_counter[::-1]
        read_counter_shift = Cat(read_counter_rev[:-1],
                                 ~read_counter_rev[-1])
        write_counter_shift = Cat(write_counter_rev[:-1],
                                  ~write_counter_rev[-1])

        exp_delay = [Signal(self.ew, name=f'exp_q_{j}', reset_less=True)
                     for j in range(cpwr.delay)]
        with m.If(self.clken):
            m.d.sync += exp_delay[0].eq(common_exp.exponent_out)
            m.d.sync += [exp_delay[j].eq(exp_delay[j - 1])
                         for j in range(1, len(exp_delay))]

        # The power stage works on two samples at once: the second half of the sample that entered
        # it one cycle before (the add or the comparison), and the first product of the sample
        # entering. It takes the earlier sample's mode; the two differ only across a frame's
        # start, where the entering sample is its first transform's and adds nothing (the
        # accumulator it meets reads 0), so both modes give it the same value.
        power_peak = peak_delay[power_delay]
        write_peak = peak_delay[-1]

        writeback = Signal()
        read_data = Signal(self.sumw + self.ew)
        rdata = Mux(pingpong, rdports_reg[0], rdports_reg[1])
        m.d.comb += [
            to_fp.clken.eq(self.clken),
            to_fp.re_in.eq(self.re_in),
            to_fp.im_in.eq(self.im_in),

            common_exp.clken.eq(self.clken),
            common_exp.re_a_in.eq(to_fp.re_out),
            common_exp.im_a_in.eq(to_fp.im_out),
            common_exp.exponent_a_in.eq(to_fp.exponent_out),
            read_data.eq(
                Mux(not_first_sum_delay[-1],
                    Mux(pingpong_delay[mem_delay - 1],
                        rdports_reg[1], rdports_reg[0]),
                    0)),
            common_exp.b_in.eq(read_data[:self.sumw]),
            common_exp.exponent_b_in.eq(read_data[-self.ew:]),

            cpwr.clken.eq(self.clken),
            cpwr.common_edge.eq(self.common_edge),
            cpwr.peak_detect.eq(power_peak),
            cpwr.re_in.eq(common_exp.re_a_out),
            cpwr.im_in.eq(common_exp.im_a_out),
            cpwr.real_in.eq(common_exp.b_out),

            self.done.eq(pingpong_delay[-1] ^ pingpong_q),
            # We need to include pingpong_delay[1] here because otherwise the
            # rden would be active immediately after toggling pingpong, and we
            # would lose the contents of the ram output register.
            rdports[0].en.eq(Mux(pingpong & pingpong_delay[mem_delay - 1],
                                 self.rden, self.clken)),
            rdports[1].en.eq(Mux(pingpong | pingpong_delay[mem_delay - 1],
                                 self.clken, self.rden)),
            rdports[0].addr.eq(Mux(pingpong, self.rdaddr, read_counter_shift)),
            rdports[1].addr.eq(Mux(pingpong, read_counter_shift, self.rdaddr)),
            # In average mode, always write back to the BRAM. In peak detect
            # mode, only write back when the cpwr says that the new power is
            # greater than the one in the BRAM.
            writeback.eq(~write_peak | cpwr.is_greater),
            wrports[0].en.eq((~pingpong_delay[-1]) & self.clken & writeback),
            wrports[1].en.eq(pingpong_delay[-1] & self.clken & writeback),
            self.rdata_value.eq(rdata[:self.sumw]),
            self.rdata_exponent.eq(rdata[-self.ew:]),
        ]
        for wr in wrports:
            m.d.comb += [
                wr.addr.eq(write_counter_shift),
                wr.data.eq(Cat(cpwr.out[:self.sumw], exp_delay[-1])),
                ]
        return m
