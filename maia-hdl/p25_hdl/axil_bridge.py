#
# Fishball P25 - AXI4-Lite to register bus bridge that answers every access.
#
# Maia's `Axi4LiteRegisterBridge` waits for the register bus to say done, so an address no bank
# claims, a bank held in reset, or a write without byte strobes stalls the bus, and the A9 with
# it. This bridge answers those itself: reads return 0, writes are dropped. A bank that has not
# answered after `timeout` cycles is answered the same way, and its late answer is ignored.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from maia_hdl import axi


class AnsweringRegisterBridge(Elaboratable):
    """AXI4-Lite to register bus bridge (the bus of maia_hdl/register.py).

    Parameters
    ----------
    address_width : int
        Word address width of the register bus (the AXI address has two more bits).
    timeout : int
        Cycles a claimed access may wait for its bank.
    name : str
        Prefix of the AXI4-Lite pins.

    Attributes
    ----------
    axi : AxiInterface
    ren : Signal(), out
        Read enable for the banks (only for a claimed address).
    wstrobe : Signal(4), out
        Write strobe for the banks (only for a claimed address).
    address : Signal(address_width), out
    wdata : Signal(32), out
    claimed : Signal(), in
        A live bank decodes ``address`` (combinational from ``address``).
    rdone, wdone : Signal(), in
    rdata : Signal(32), in
    """
    def __init__(self, address_width: int, *, timeout: int = 4096, name: str = None):
        self.aw = address_width
        self.timeout = timeout
        self.axi = axi.AxiInterface(
            axi.AxiDevice.SUBORDINATE,
            [axi.AxiChannel(axi.AxiDirection.READ, self.aw + 2, 32),
             axi.AxiChannel(axi.AxiDirection.WRITE, self.aw + 2, 32)],
            axi.AxiVersion.AXI4LITE,
            name=name)
        self.ren = Signal()
        self.rdone = Signal()
        self.wstrobe = Signal(4)
        self.wdone = Signal()
        self.address = Signal(self.aw, reset_less=True)
        self.rdata = Signal(32)
        self.wdata = Signal(32)
        self.claimed = Signal()

    def elaborate(self, platform):
        m = Module()
        busy = Signal()
        write_preference = Signal()
        start_write = Signal()
        start_write_q = Signal()
        start_read = Signal()
        start_read_q = Signal()
        strobe_q = Signal(4)
        m.d.comb += [
            self.axi.awready.eq(start_write_q),
            self.axi.wready.eq(start_write_q),
            self.axi.arready.eq(start_read_q),
            self.wdata.eq(self.axi.wdata),
            start_write.eq(
                ~busy & self.axi.awvalid & self.axi.wvalid
                & (write_preference | ~self.axi.arvalid)),
            start_read.eq(
                ~busy & self.axi.arvalid
                & (~write_preference | ~self.axi.awvalid | ~self.axi.wvalid)),
        ]
        m.d.sync += [
            start_write_q.eq(start_write),
            start_read_q.eq(start_read),
            strobe_q.eq(Mux(start_write, self.axi.wstrb, 0)),
            self.address.eq(Mux(start_write, self.axi.awaddr >> 2, self.axi.araddr >> 2)),
        ]

        # The access goes to a bank only when one claims the address (and a write has strobes);
        # otherwise the bridge answers it in the cycle the bank would have seen it.
        to_bank_read = start_read_q & self.claimed
        to_bank_write = start_write_q & self.claimed & strobe_q.any()
        m.d.comb += [
            self.ren.eq(to_bank_read),
            self.wstrobe.eq(Mux(to_bank_write, strobe_q, 0)),
        ]
        self_read = start_read_q & ~self.claimed
        self_write = start_write_q & ~(self.claimed & strobe_q.any())

        pending_read = Signal()
        pending_write = Signal()
        waited = Signal(range(self.timeout + 1))
        with m.If(to_bank_read):
            m.d.sync += [pending_read.eq(1), waited.eq(0)]
        with m.If(to_bank_write):
            m.d.sync += [pending_write.eq(1), waited.eq(0)]
        with m.If(pending_read | pending_write):
            m.d.sync += waited.eq(waited + 1)
        expired = (pending_read | pending_write) & (waited == self.timeout)

        read_answer = Signal()
        read_value = Signal(32)
        write_answer = Signal()
        m.d.comb += [
            read_answer.eq((pending_read & (self.rdone | expired)) | self_read),
            read_value.eq(Mux(pending_read & self.rdone, self.rdata, 0)),
            write_answer.eq((pending_write & (self.wdone | expired)) | self_write),
        ]
        with m.If(read_answer):
            m.d.sync += [self.axi.rdata.eq(read_value), self.axi.rvalid.eq(1),
                         pending_read.eq(0)]
        with m.If(write_answer):
            m.d.sync += [self.axi.bvalid.eq(1), pending_write.eq(0)]

        with m.If(self.axi.b_handshake() | self.axi.r_handshake()):
            m.d.sync += busy.eq(0)
        with m.If(self.axi.b_handshake()):
            m.d.sync += self.axi.bvalid.eq(0)
        with m.If(self.axi.r_handshake()):
            m.d.sync += self.axi.rvalid.eq(0)
        with m.If(start_write | start_read):
            m.d.sync += [busy.eq(1), write_preference.eq(~write_preference)]
        m.d.comb += [
            self.axi.bresp.eq(axi.AxiResp.OKAY),
            self.axi.rresp.eq(axi.AxiResp.OKAY),
        ]
        return m
