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

    def validate(self):
        assert self.platform >= 0 and self.platform < 256
        # Ring base addresses must be aligned to total ring size
        assert self.dibit_dma_address & (self.dibit_dma_total_size - 1) == 0, \
            f'dibit_dma_address {self.dibit_dma_address:#x} not aligned to ' \
            f'ring size {self.dibit_dma_total_size:#x}'
        assert self.traffic_dma_address & (self.traffic_dma_total_size - 1) == 0, \
            f'traffic_dma_address {self.traffic_dma_address:#x} not aligned to ' \
            f'ring size {self.traffic_dma_total_size:#x}'
