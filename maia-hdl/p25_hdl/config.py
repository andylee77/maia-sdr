#
# Fishball P25 - Configuration
#
# SPDX-License-Identifier: MIT
#


class P25Config:
    """P25 core configuration

    Defines memory layout and platform parameters for the P25 IP core.
    """
    def __init__(self):
        # Platform identifier (0 = Fishball Z7020)
        self.platform = 0

        # ── Control channel dibit ring DMA ────────────────────────
        # 4800 sym/sec -> ~1.28 KB/sec packed dibits
        # 8 sub-buffers x 4 KB = 32 KB total ring
        # ~3.2 sec per sub-buffer (one interrupt per ~3.2 sec)
        # Ring base must be aligned to total_size (32 KB)
        self.dibit_dma_address = 0x1700_0000
        self.dibit_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.dibit_dma_buffer_size = 0x1000   # 4 KB per sub-buffer

        # ── Traffic channel dibit ring DMA ────────────────────────
        self.traffic_dma_address = 0x1800_0000
        self.traffic_dma_num_buffers_log2 = 3
        self.traffic_dma_buffer_size = 0x1000

        # ── Control channel post-DDC IQ ring DMA (Phase 6C) ───────
        # Streams the control DDC's post-decimation IQ output to DDR
        # so the PS can run validated demod prototypes (Python in 6C,
        # Rust in 6D) directly on the live antenna feed without
        # disturbing the existing dibit DMA path.
        #
        # Source: control DDC re_out/im_out (16-bit signed I + 16-bit
        # signed Q) at 62.5 kSPS (8 MSPS ADC / 128x decimation,
        # matches the SDRTrunk-captured reference wavs and the
        # tools/p25_lsm_demod.py validated reference).
        #
        # IQ packing (per 64-bit DMA word):
        #     bit 63                                                bit 0
        #     +-----------------+-----------------+-----------------+-----------------+
        #     |    im[1] s16    |    re[1] s16    |    im[0] s16    |    re[0] s16    |
        #     +-----------------+-----------------+-----------------+-----------------+
        #            63..48            47..32            31..16            15..0
        # Sample 0 is in the low half. The PS-side reader treats each
        # 64-bit word as four little-endian int16s in the order
        # re0, im0, re1, im1.
        #
        # Bandwidth math:
        #     62.5 kSPS x 4 B/sample          = 250 KB/s
        #     250 KB/s / 32 KB sub-buffer     = ~7.8 IRQ/s = ~128 ms per sub-buffer
        #     8 sub-buffers x 32 KB           = 256 KB ring = ~1.0 s of IQ in flight
        #
        # 256 KB is much larger than the dibit/traffic rings (32 KB)
        # because IQ runs ~200x faster — at the smaller size we'd
        # IRQ-flood the PS. ~1 s ring depth gives the Phase 6D Rust
        # demod (and the Phase 6C bring-up Python script) plenty of
        # slack between polls.
        #
        # Ring base must be aligned to total ring size (256 KB) — the
        # AW counter inside DmaStreamRingWrite simply increments and
        # masks, so unaligned bases would alias outside the ring.
        # Asserted in validate() below.
        #
        # See doc/P25_ADDRESS_MAP.md for the full picture (DDR
        # carve-outs, register banks, IRQ assignments).
        self.iq_dma_address = 0x1900_0000
        self.iq_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.iq_dma_buffer_size = 0x8000   # 32 KB per sub-buffer

        # ── Control channel LSM dibit ring DMA (Phase 6E.9) ───────
        # Parallel to the existing C4FM `dibit_dma`. The LSM demod
        # chain (`LsmDecimator2 -> LsmFir(LPF) -> LsmFir(RRC) ->
        # LsmDemod`) sits beside the C4FM chain on the same control
        # DDC output and produces its own dibit stream from
        # `LsmDemod.dibit_out`/`symbol_strobe`. Giving it its own
        # ring DMA lets the PS drain both rings in parallel and
        # A/B C4FM vs. LSM on the same RF capture.
        #
        # Layout (8 sub-buffers x 4 KB = 32 KB) mirrors `dibit_dma`
        # exactly so the existing kernel-side DMA helper code carries
        # over without changes. Byte rate at 4800 sym/s is identical
        # to `dibit_dma` (~1.28 KB/s).
        #
        # Ring base must be aligned to total ring size (32 KB).
        # See doc/P25_ADDRESS_MAP.md for the full picture.
        self.lsm_dibit_dma_address = 0x1A00_0000
        self.lsm_dibit_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.lsm_dibit_dma_buffer_size = 0x1000   # 4 KB per sub-buffer

        # ── Traffic channel LSM dibit ring DMA (Phase 7A.2) ───────
        # Phase 7A.2 mirrors what Phase 6E.9 did on the control side:
        # an LSM demod chain (LsmDecimator2 -> LsmFir(LPF) ->
        # LsmFir(RRC) -> LsmDemod) sits beside the existing C4FM
        # traffic chain, both fed by the same `traffic_ddc.re_out`/
        # `im_out`. The LSM dibits exit via this dedicated ring; the
        # C4FM dibits stay on `traffic_dma_address` (0x1800_0000).
        # The PS drains both rings in parallel so the dashboard can
        # A/B C4FM vs. LSM on a followed voice channel and pick the
        # right modulation per call (Phase 7B will add automatic
        # selection based on which pipeline produces stable NIDs).
        #
        # Layout (8 sub-buffers x 4 KB = 32 KB) mirrors
        # `lsm_dibit_dma` exactly. Byte rate is the same 1.28 KB/s
        # as the other dibit rings.
        #
        # Address `0x1B00_0000` continues the 0x100_0000 spacing
        # pattern: 0x1700 (control C4FM dibit), 0x1800 (traffic
        # C4FM dibit), 0x1900 (control IQ), 0x1A00 (control LSM
        # dibit), 0x1B00 (traffic LSM dibit). 0x1C00 onwards is
        # reserved for the Phase 7G channelizer slot rings.
        #
        # Tezuka side: a new device-tree carve-out for
        # `p25_traffic_lsm_dibit_dma@1b000000` is required so the
        # rxbuffer kernel module exposes a `p25-traffic-lsm-dibit`
        # UIO device. PS-side `fpga.rs` opens that device.
        #
        # Ring base must be aligned to total ring size (32 KB).
        # See doc/P25_ADDRESS_MAP.md for the canonical map.
        self.traffic_lsm_dibit_dma_address = 0x1B00_0000
        self.traffic_lsm_dibit_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.traffic_lsm_dibit_dma_buffer_size = 0x1000   # 4 KB per sub-buffer

        # ── Traffic channel post-DDC IQ ring DMA (2026-04-16) ─────
        # Mirrors the control-side `iq_dma` (Phase 6C) for the
        # traffic chain: taps `traffic_ddc.re_out`/`im_out` at 62.5
        # kSPS and streams the packed 64-bit IQ words to DDR.
        #
        # Motivation: the control-vs-traffic chain audit (2026-04-16)
        # found that traffic had no post-DDC IQ tap, so software
        # features on the control side (/api/control_iq_capture,
        # offline software LSM pipeline cross-check, planned
        # constellation scatter for traffic-chain debug) had no
        # traffic-side equivalent. Adding full iq_dma parity closes
        # the asymmetry and unblocks the constellation dashboard
        # without a separate HDL BRAM-ring one-off.
        #
        # Address 0x1C00_0000 continues the 0x100_0000 spacing
        # pattern established in the earlier rings:
        #   0x1700 control C4FM dibit
        #   0x1800 traffic C4FM dibit
        #   0x1900 control IQ  (256 KB)
        #   0x1A00 control LSM dibit
        #   0x1B00 traffic LSM dibit
        #   0x1C00 traffic IQ  (256 KB) ← new
        # 0x1D00 onwards remains reserved for the Phase 7G
        # channelizer slot rings.
        #
        # Layout mirrors `iq_dma` exactly (256 KB, 8 × 32 KB
        # sub-buffers) — same bandwidth math (62.5 kSPS × 4 B =
        # 250 KB/s, ~7.8 IRQ/s) and the same PS-side reader logic
        # just swaps the DMA base address.
        #
        # Ring base must be aligned to total ring size (256 KB).
        self.traffic_iq_dma_address = 0x1C00_0000
        self.traffic_iq_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.traffic_iq_dma_buffer_size = 0x8000   # 32 KB per sub-buffer

        # ── Control channel post-LSM (matched-filter) IQ DMA (2026-04-18) ─
        # Phase 10.6: second IQ tap on the control chain, sourced from
        # the LSM chain's RRC matched-filter output (`lsm_rrc.re_out/
        # im_out/strobe_out`) rather than the raw post-DDC output. Both
        # rings run concurrently; the PS chooses which to expose via
        # the `/ws/iq?source=post_ddc|post_lsm` API.
        #
        # Rate: 31.25 kSPS (half of post-DDC after the LsmDecimator2
        # /2 stage). Packing format is identical to `iq_dma` — two
        # i16(re, im) pairs per 64-bit DMA word.
        #
        # Bandwidth:
        #     31.25 kSPS × 4 B/sample        = 125 KB/s
        #     125 KB/s / 32 KB sub-buffer    = ~3.9 IRQ/s = ~256 ms/sub-buffer
        #     8 × 32 KB                      = 256 KB ring = ~2 s of IQ
        #
        # Why it matters: raw post-DDC IQ is an unfiltered, non-
        # amplitude-normalised view — useful for baseband analysis but
        # not a matched-filter eye. Post-RRC is the canonical eye-plot
        # source: the signal has been matched-filtered against its own
        # pulse shape and ISI is minimised around decision points. Feeds
        # the browser-side eye-plot renderer (stage 8).
        #
        # Address 0x1D00_0000 continues the 0x100_0000 spacing (next
        # free slot after 0x1C00 traffic IQ). 256 KB aligned.
        self.lsm_iq_dma_address = 0x1D00_0000
        self.lsm_iq_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.lsm_iq_dma_buffer_size = 0x8000   # 32 KB per sub-buffer

        # ── Traffic channel post-LSM IQ DMA (2026-04-18) ──────────
        # Symmetric with `lsm_iq_dma`, tapped from `traffic_lsm_rrc`
        # output. Required for matched-filter eye on voice channels,
        # which is where the robotic-audio diagnostic work happens.
        #
        # Address 0x1E00_0000, 256 KB aligned.
        self.traffic_lsm_iq_dma_address = 0x1E00_0000
        self.traffic_lsm_iq_dma_num_buffers_log2 = 3
        self.traffic_lsm_iq_dma_buffer_size = 0x8000

        # ── Control channel post-PLL IQ DMA (Phase 10.7, 2026-04-22)
        # Tapped from `LsmPllRotate` outputs (mid + sym interleaved
        # onto a single strobe; 2 samples/symbol → 9.6 kSPS).
        # Samples are already carrier-derotated and AGC-scaled, so
        # this is the first dashboard tap that produces a clean
        # open-eye + tight constellation without any PS-side signal
        # processing. Feeds the new Plots tab (eye + constellation +
        # deviation) and `/api/deviation`. See doc/DASHBOARD_PLOTS.md
        # §9 and the `post_pll_iq` bank detail in P25_ADDRESS_MAP.md.
        #
        # Ring deliberately oversized for the rate (~38 KB/s × 256 KB
        # = ~7 s of IQ) so the PS can pull a 1-second deviation
        # window without cache pressure.
        # Address 0x1F00_0000, 256 KB aligned.
        self.post_pll_iq_dma_address = 0x1F00_0000
        self.post_pll_iq_dma_num_buffers_log2 = 3
        self.post_pll_iq_dma_buffer_size = 0x8000

        # ── Traffic channel post-PLL IQ DMA (Phase 10.7) ──────────
        # Traffic-side twin of `post_pll_iq_dma`. Same packing, same
        # geometry, different tap (traffic-chain LSM rotate).
        # Address 0x2000_0000, 256 KB aligned.
        self.traffic_post_pll_iq_dma_address = 0x2000_0000
        self.traffic_post_pll_iq_dma_num_buffers_log2 = 3
        self.traffic_post_pll_iq_dma_buffer_size = 0x8000

        # ── Wideband spectrometer DMA (Phase 10.7) ────────────────
        # Output of the `Spectrometer` sub-module tapped pre-DDC off
        # `rxiq_cdc`. Uses `DmaBRAMWrite` rather than the streaming
        # ring DMA, so the buffer size is fixed by the FFT geometry:
        # `fft_order_log2 = 12` → 4096 bins × 8 bytes = 32 KB per
        # integrated spectrum. Total carve-out = num_buffers × 32 KB.
        #
        # 4 buffers × 32 KB = 128 KB ring. Hardware integrator
        # averages at 5-10 Hz cadence; PS reads latest buffer via
        # `spec_status.last_buffer`. Span = AD9361 sample rate
        # (preset-dependent, 2-16 MHz). Feeds `/api/spectrum_wide`;
        # no PS FFT.
        # Address 0x2100_0000, 128 KB aligned.
        self.wideband_spec_dma_address = 0x2100_0000
        self.wideband_spec_dma_num_buffers_log2 = 2   # 4 sub-buffers
        # Spectrometer FFT is 4096 bins (order_log2=12) × 8 B/word.
        self.wideband_spec_dma_buffer_size = (1 << 12) * 8

    @property
    def dibit_dma_num_buffers(self):
        return 1 << self.dibit_dma_num_buffers_log2

    @property
    def dibit_dma_total_size(self):
        return self.dibit_dma_num_buffers * self.dibit_dma_buffer_size

    @property
    def traffic_dma_num_buffers(self):
        return 1 << self.traffic_dma_num_buffers_log2

    @property
    def traffic_dma_total_size(self):
        return self.traffic_dma_num_buffers * self.traffic_dma_buffer_size

    @property
    def iq_dma_num_buffers(self):
        return 1 << self.iq_dma_num_buffers_log2

    @property
    def iq_dma_total_size(self):
        return self.iq_dma_num_buffers * self.iq_dma_buffer_size

    @property
    def lsm_dibit_dma_num_buffers(self):
        return 1 << self.lsm_dibit_dma_num_buffers_log2

    @property
    def lsm_dibit_dma_total_size(self):
        return self.lsm_dibit_dma_num_buffers * self.lsm_dibit_dma_buffer_size

    @property
    def traffic_lsm_dibit_dma_num_buffers(self):
        return 1 << self.traffic_lsm_dibit_dma_num_buffers_log2

    @property
    def traffic_lsm_dibit_dma_total_size(self):
        return (self.traffic_lsm_dibit_dma_num_buffers
                * self.traffic_lsm_dibit_dma_buffer_size)

    @property
    def traffic_iq_dma_num_buffers(self):
        return 1 << self.traffic_iq_dma_num_buffers_log2

    @property
    def traffic_iq_dma_total_size(self):
        return (self.traffic_iq_dma_num_buffers
                * self.traffic_iq_dma_buffer_size)

    @property
    def lsm_iq_dma_num_buffers(self):
        return 1 << self.lsm_iq_dma_num_buffers_log2

    @property
    def lsm_iq_dma_total_size(self):
        return self.lsm_iq_dma_num_buffers * self.lsm_iq_dma_buffer_size

    @property
    def traffic_lsm_iq_dma_num_buffers(self):
        return 1 << self.traffic_lsm_iq_dma_num_buffers_log2

    @property
    def traffic_lsm_iq_dma_total_size(self):
        return (self.traffic_lsm_iq_dma_num_buffers
                * self.traffic_lsm_iq_dma_buffer_size)

    @property
    def post_pll_iq_dma_num_buffers(self):
        return 1 << self.post_pll_iq_dma_num_buffers_log2

    @property
    def post_pll_iq_dma_total_size(self):
        return (self.post_pll_iq_dma_num_buffers
                * self.post_pll_iq_dma_buffer_size)

    @property
    def traffic_post_pll_iq_dma_num_buffers(self):
        return 1 << self.traffic_post_pll_iq_dma_num_buffers_log2

    @property
    def traffic_post_pll_iq_dma_total_size(self):
        return (self.traffic_post_pll_iq_dma_num_buffers
                * self.traffic_post_pll_iq_dma_buffer_size)

    @property
    def wideband_spec_dma_num_buffers(self):
        return 1 << self.wideband_spec_dma_num_buffers_log2

    @property
    def wideband_spec_dma_total_size(self):
        return (self.wideband_spec_dma_num_buffers
                * self.wideband_spec_dma_buffer_size)

    def validate(self):
        assert self.platform >= 0 and self.platform < 256
        # Ring base addresses must be aligned to total ring size
        assert self.dibit_dma_address & (self.dibit_dma_total_size - 1) == 0, \
            f'dibit_dma_address {self.dibit_dma_address:#x} not aligned to ' \
            f'ring size {self.dibit_dma_total_size:#x}'
        assert self.traffic_dma_address & (self.traffic_dma_total_size - 1) == 0, \
            f'traffic_dma_address {self.traffic_dma_address:#x} not aligned to ' \
            f'ring size {self.traffic_dma_total_size:#x}'
        assert self.iq_dma_address & (self.iq_dma_total_size - 1) == 0, \
            f'iq_dma_address {self.iq_dma_address:#x} not aligned to ' \
            f'ring size {self.iq_dma_total_size:#x}'
        assert self.lsm_dibit_dma_address & (self.lsm_dibit_dma_total_size - 1) == 0, \
            f'lsm_dibit_dma_address {self.lsm_dibit_dma_address:#x} not ' \
            f'aligned to ring size {self.lsm_dibit_dma_total_size:#x}'
        assert self.traffic_lsm_dibit_dma_address & \
            (self.traffic_lsm_dibit_dma_total_size - 1) == 0, \
            f'traffic_lsm_dibit_dma_address ' \
            f'{self.traffic_lsm_dibit_dma_address:#x} not aligned to ' \
            f'ring size {self.traffic_lsm_dibit_dma_total_size:#x}'
        assert self.traffic_iq_dma_address & \
            (self.traffic_iq_dma_total_size - 1) == 0, \
            f'traffic_iq_dma_address ' \
            f'{self.traffic_iq_dma_address:#x} not aligned to ' \
            f'ring size {self.traffic_iq_dma_total_size:#x}'
        assert self.lsm_iq_dma_address & \
            (self.lsm_iq_dma_total_size - 1) == 0, \
            f'lsm_iq_dma_address ' \
            f'{self.lsm_iq_dma_address:#x} not aligned to ' \
            f'ring size {self.lsm_iq_dma_total_size:#x}'
        assert self.traffic_lsm_iq_dma_address & \
            (self.traffic_lsm_iq_dma_total_size - 1) == 0, \
            f'traffic_lsm_iq_dma_address ' \
            f'{self.traffic_lsm_iq_dma_address:#x} not aligned to ' \
            f'ring size {self.traffic_lsm_iq_dma_total_size:#x}'
        assert self.post_pll_iq_dma_address & \
            (self.post_pll_iq_dma_total_size - 1) == 0, \
            f'post_pll_iq_dma_address ' \
            f'{self.post_pll_iq_dma_address:#x} not aligned to ' \
            f'ring size {self.post_pll_iq_dma_total_size:#x}'
        assert self.traffic_post_pll_iq_dma_address & \
            (self.traffic_post_pll_iq_dma_total_size - 1) == 0, \
            f'traffic_post_pll_iq_dma_address ' \
            f'{self.traffic_post_pll_iq_dma_address:#x} not aligned to ' \
            f'ring size {self.traffic_post_pll_iq_dma_total_size:#x}'
        assert self.wideband_spec_dma_address & \
            (self.wideband_spec_dma_total_size - 1) == 0, \
            f'wideband_spec_dma_address ' \
            f'{self.wideband_spec_dma_address:#x} not aligned to ' \
            f'ring size {self.wideband_spec_dma_total_size:#x}'
