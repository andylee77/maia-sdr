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

        # ── Control channel post-DDC IQ ring DMA (Phase 6C) ───────
        # Streams the control DDC's post-decimation IQ output to DDR
        # at 62.5 kSPS (8 MSPS ADC / 128x decimation).
        #
        # IQ packing (per 64-bit DMA word):
        #     bit 63                                                bit 0
        #     +-----------------+-----------------+-----------------+-----------------+
        #     |    im[1] s16    |    re[1] s16    |    im[0] s16    |    re[0] s16    |
        #     +-----------------+-----------------+-----------------+-----------------+
        # Sample 0 is in the low half.
        #
        # Bandwidth math:
        #     62.5 kSPS x 4 B/sample          = 250 KB/s
        #     8 sub-buffers x 32 KB           = 256 KB ring = ~1.0 s
        #
        # Ring base must be aligned to total ring size (256 KB).
        self.iq_dma_address = 0x1900_0000
        self.iq_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.iq_dma_buffer_size = 0x8000   # 32 KB per sub-buffer

        # ── Control channel LSM dibit ring DMA (Phase 6E.9) ───────
        # Recovered dibits from the LSM demod chain. 4800 sym/s ->
        # ~1.28 KB/s packed. 8 x 4 KB = 32 KB total ring.
        #
        # Ring base must be aligned to total ring size (32 KB).
        self.lsm_dibit_dma_address = 0x1A00_0000
        self.lsm_dibit_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.lsm_dibit_dma_buffer_size = 0x1000   # 4 KB per sub-buffer

        # ── Traffic channel LSM dibit ring DMA (Phase 7A.2) ───────
        # Traffic-side twin of `lsm_dibit_dma`.
        #
        # Ring base must be aligned to total ring size (32 KB).
        self.traffic_lsm_dibit_dma_address = 0x1B00_0000
        self.traffic_lsm_dibit_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.traffic_lsm_dibit_dma_buffer_size = 0x1000   # 4 KB per sub-buffer

        # ── Traffic channel post-DDC IQ ring DMA (2026-04-16) ─────
        # Mirrors the control-side `iq_dma` for the traffic chain.
        # Same 62.5 kSPS, same 256 KB geometry, address 0x1C00_0000.
        #
        # Ring base must be aligned to total ring size (256 KB).
        self.traffic_iq_dma_address = 0x1C00_0000
        self.traffic_iq_dma_num_buffers_log2 = 3   # 8 sub-buffers
        self.traffic_iq_dma_buffer_size = 0x8000   # 32 KB per sub-buffer

        # ── Control channel pre-diff IQ ring DMA (Phase 10.8) ─────
        # Tap inside LsmDemodLoop after LsmPllRotate but BEFORE the
        # differential slicer. `lsm_demod.i_pre_diff_out` /
        # `q_pre_diff_out` / `pre_diff_strobe_out` deliver mid + sym
        # samples (2 samples/symbol, ~9.6 kSPS) of carrier-derotated
        # + AGC-scaled IQ. Samples are already pre-rotated / pre-
        # interleaved -- the PS renders a clean open-eye + tight
        # constellation with no additional signal processing.
        #
        # Same packing (64-bit words, 2 samples/word) as `iq_dma`.
        # Bandwidth ~38 KB/s, oversized ring (256 KB) gives ~7 s
        # of IQ in flight -- plenty of slack for 1 s deviation
        # windows.
        #
        # Address 0x1F00_0000 was freed by retiring the superseded
        # `post_pll_iq_dma` ring on 2026-04-22. 256 KB aligned.
        self.pre_diff_iq_dma_address = 0x1F00_0000
        self.pre_diff_iq_dma_num_buffers_log2 = 3
        self.pre_diff_iq_dma_buffer_size = 0x8000

        # ── Traffic channel pre-diff IQ ring DMA (Phase 10.8) ─────
        # Traffic-side twin of `pre_diff_iq_dma`. Same tap point in
        # `traffic_lsm_demod`, same packing, same geometry.
        # Address 0x2000_0000 was freed by retiring
        # `traffic_post_pll_iq_dma`. 256 KB aligned.
        self.traffic_pre_diff_iq_dma_address = 0x2000_0000
        self.traffic_pre_diff_iq_dma_num_buffers_log2 = 3
        self.traffic_pre_diff_iq_dma_buffer_size = 0x8000

        # ── Wideband spectrometer DMA (Phase 10.7) ────────────────
        # `DmaBRAMWrite`, fixed 4096-bin FFT geometry.
        # 4 buffers × 32 KB = 128 KB ring. 5-10 Hz integrator.
        # Address 0x2100_0000, 128 KB aligned.
        self.wideband_spec_dma_address = 0x2100_0000
        self.wideband_spec_dma_num_buffers_log2 = 2   # 4 sub-buffers
        # Spectrometer FFT is 4096 bins (order_log2=12) × 8 B/word.
        self.wideband_spec_dma_buffer_size = (1 << 12) * 8

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
    def pre_diff_iq_dma_num_buffers(self):
        return 1 << self.pre_diff_iq_dma_num_buffers_log2

    @property
    def pre_diff_iq_dma_total_size(self):
        return (self.pre_diff_iq_dma_num_buffers
                * self.pre_diff_iq_dma_buffer_size)

    @property
    def traffic_pre_diff_iq_dma_num_buffers(self):
        return 1 << self.traffic_pre_diff_iq_dma_num_buffers_log2

    @property
    def traffic_pre_diff_iq_dma_total_size(self):
        return (self.traffic_pre_diff_iq_dma_num_buffers
                * self.traffic_pre_diff_iq_dma_buffer_size)

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
        assert self.pre_diff_iq_dma_address & \
            (self.pre_diff_iq_dma_total_size - 1) == 0, \
            f'pre_diff_iq_dma_address ' \
            f'{self.pre_diff_iq_dma_address:#x} not aligned to ' \
            f'ring size {self.pre_diff_iq_dma_total_size:#x}'
        assert self.traffic_pre_diff_iq_dma_address & \
            (self.traffic_pre_diff_iq_dma_total_size - 1) == 0, \
            f'traffic_pre_diff_iq_dma_address ' \
            f'{self.traffic_pre_diff_iq_dma_address:#x} not aligned to ' \
            f'ring size {self.traffic_pre_diff_iq_dma_total_size:#x}'
        assert self.wideband_spec_dma_address & \
            (self.wideband_spec_dma_total_size - 1) == 0, \
            f'wideband_spec_dma_address ' \
            f'{self.wideband_spec_dma_address:#x} not aligned to ' \
            f'ring size {self.wideband_spec_dma_total_size:#x}'
