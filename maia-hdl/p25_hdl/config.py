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
