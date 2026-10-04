#
# Fishball P25 - Configuration: the platform and the DDR rings the core writes. Each ring has a
# device-tree carve-out and rxbuffer node of the same geometry (tezuka_fw fishball-p25.dtsi).
#
# SPDX-License-Identifier: MIT
#


class P25Config:
    """P25 core configuration: memory layout and platform."""
    def __init__(self):
        # Platform identifier (0 = Fishball Z7020), in `version[31:24]`.
        self.platform = 0

        # Lanes: one DDC each, lane 0 the control channel.
        self.lanes = 3

        # The lane ring: every lane's tagged IQ packets (lane_packetizer.py), 4 KB each, four to a
        # sub-buffer. Three lanes at 50 kSPS write ~600 KB/s, so 2 MB holds 3.4 s.
        # Device `p25-lanes`.
        self.lanes_dma_address = 0x1900_0000
        self.lanes_dma_num_buffers_log2 = 7       # 128 sub-buffers
        self.lanes_dma_buffer_size = 0x4000       # 16 KB

        # The wideband spectrometer: one 4096-bin integration (8 B a bin) per sub-buffer.
        # Device `p25-wideband-spec`.
        self.wideband_spec_dma_address = 0x2100_0000
        self.wideband_spec_dma_num_buffers_log2 = 1   # 2 sub-buffers
        self.wideband_spec_dma_buffer_size = (1 << 12) * 8

        # The raw IQ capture: the AD9361's samples before any DDC, two a 64-bit word, 32 MB/s
        # at 8 MSPS (0.5 s in 16 MB). Device `p25-wideband-iq`.
        self.wideband_iq_dma_address = 0x2200_0000
        self.wideband_iq_dma_num_buffers_log2 = 4     # 16 sub-buffers
        self.wideband_iq_dma_buffer_size = 0x10_0000  # 1 MB

    RING_NAMES = ('lanes_dma', 'wideband_spec_dma', 'wideband_iq_dma')

    def num_buffers(self, ring):
        return 1 << getattr(self, f'{ring}_num_buffers_log2')

    def total_size(self, ring):
        return self.num_buffers(ring) * getattr(self, f'{ring}_buffer_size')

    def rings(self):
        """Every ring as (name, base address, total size)."""
        return [(name, getattr(self, f'{name}_address'), self.total_size(name))
                for name in self.RING_NAMES]

    def validate(self):
        assert 0 <= self.platform < 256
        assert 1 <= self.lanes <= 15
        for name, base, size in self.rings():
            # The DMA engines need the base aligned to the whole ring.
            assert base & (size - 1) == 0, \
                f'{name} base {base:#x} not aligned to its size {size:#x}'
            # U-Boot keeps the initrd and the device tree below 0x1800_0000.
            assert base >= 0x1800_0000, f'{name} below 0x1800_0000'
        rings = sorted(self.rings(), key=lambda r: r[1])
        for (name_a, base_a, size_a), (name_b, base_b, _) in zip(rings, rings[1:]):
            assert base_a + size_a <= base_b, \
                f'{name_a} [{base_a:#x}, {base_a + size_a:#x}) overlaps {name_b} at {base_b:#x}'
