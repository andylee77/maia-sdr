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

        # Control channel dibit DMA buffer
        # Small buffer is fine: 4800 sym/sec = ~1.2 KB/sec
        self.dibit_dma_address = 0x1700_0000
        self.dibit_dma_size = 0x0010_0000  # 1 MB

        # Traffic channel dibit DMA buffer
        self.traffic_dma_address = 0x1800_0000
        self.traffic_dma_size = 0x0010_0000  # 1 MB

    def validate(self):
        assert self.platform >= 0 and self.platform < 256
