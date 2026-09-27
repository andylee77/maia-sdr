#
# Fishball hardware validation (hwval) - Configuration
#
# See doc/HW_VALIDATION_SUITE.md (sections 6.1, 6.2 and 6.4).
#
# SPDX-License-Identifier: MIT
#


class HwvalConfig:
    """hwval core configuration

    Defines the identity, the AXI-Lite register block layout, the DDR
    windows used by the AXI masters and the FIFO depths of the
    validation core. The register table (``hwval_hdl.regmap``) and the
    top level (``hwval_hdl.hwval_top``) are both generated from this
    object, so everything that must agree between the gateware, the
    JSON register map and the documentation lives here.
    """
    def __init__(self):
        # ── Identity ─────────────────────────────────────────────
        # "hwv1"
        self.id = 0x6877_7631
        self.version = (0, 1, 0)
        # Platform identifier (0 = Fishball Z7020), kept for parity
        # with P25Config.
        self.platform = 0

        # ── AXI-Lite subordinate ─────────────────────────────────
        # 4 KiB window at 0x7C46_0000 (12-bit byte address).
        self.axi_lite_base = 0x7C46_0000
        self.axi_lite_size = 4096
        self.axi_lite_address_bits = 12

        # Register block bases (byte offsets from axi_lite_base).
        # Each block is 256 bytes (64 words).
        self.block_size = 0x100
        self.block_bases = {
            'id': 0x000,
            'census': 0x100,
            'ingest': 0x200,
            'legacy': 0x400,
            'ringv2': 0x500,
            'mt0': 0x600,
            'mt1': 0x700,
            'evt': 0x800,
        }

        # Snapshot domain bits (SNAP_REQ / SNAP_ACK). The 'mem' snapshot
        # domain is the clk2x (125 MHz) clock domain.
        self.snapshot_domains = {
            'sync': 1,
            'mem': 2,
            'sampling': 4,
        }
        self.snapshot_clock_domains = {
            'sync': 'sync',
            'mem': 'clk2x',
            'sampling': 'sampling',
        }

        # ── DDR windows (device tree, static no-map) ─────────────
        # ring v2: runtime base/size inside this window.
        self.ringv2_window_base = 0x2000_0000
        self.ringv2_window_size = 16 << 20
        # legacy ring: fixed base, same geometry as the P25 wideband
        # ring (16 x 1 MiB).
        self.legacy_base = 0x2200_0000
        self.legacy_num_buffers_log2 = 4
        self.legacy_buffer_size = 1 << 20
        # memory testers mt0/mt1 share one 64 MiB window; by default mt0
        # tests the lower half and mt1 the upper half.
        self.memtest_window_base = 0x2400_0000
        self.memtest_window_size = 64 << 20

        # ── FIFO depths / AXI parameters ─────────────────────────
        self.ringv2_fifo_depth = 2048   # 64-bit words = 16 KiB
        self.ringv2_max_outstanding = 8
        self.memtest_max_outstanding = 8
        self.evt_heartbeat_log2 = 26

        # ── Clocks (nominal, for documentation and defaults) ─────
        self.s_axi_lite_hz = 100_000_000
        self.sync_hz = 62_500_000
        self.clk2x_hz = 125_000_000
        self.clk3x_hz = 187_500_000

        # Burst geometry of ring v2 (16 beats x 8 bytes).
        self.ringv2_burst_bytes = 128

    # ── Derived values ──────────────────────────────────────────
    @property
    def version_str(self):
        return '.'.join(str(v) for v in self.version)

    @property
    def version_word(self):
        major, minor, patch = self.version
        return (major << 16) | (minor << 8) | patch

    @property
    def legacy_num_buffers(self):
        return 1 << self.legacy_num_buffers_log2

    @property
    def legacy_size(self):
        return self.legacy_num_buffers * self.legacy_buffer_size

    @property
    def ringv2_window_bursts(self):
        return self.ringv2_window_size // self.ringv2_burst_bytes

    @property
    def guard_lo_default(self):
        """Lowest address any hwval master may touch by default."""
        return min(self.ringv2_window_base, self.legacy_base,
                   self.memtest_window_base)

    @property
    def guard_hi_default(self):
        """One past the highest address any hwval master may touch."""
        return max(self.ringv2_window_base + self.ringv2_window_size,
                   self.legacy_base + self.legacy_size,
                   self.memtest_window_base + self.memtest_window_size)

    @property
    def mt0_default_base(self):
        return self.memtest_window_base

    @property
    def mt1_default_base(self):
        return self.memtest_window_base + self.memtest_window_size // 2

    @property
    def mt_default_size(self):
        return self.memtest_window_size // 2

    def validate(self):
        assert 0 <= self.platform < 256
        assert all(0 <= v < 256 for v in self.version)
        assert self.axi_lite_size == 1 << self.axi_lite_address_bits
        for name, base in self.block_bases.items():
            assert base % self.block_size == 0, \
                f'block {name} base {base:#x} not block aligned'
            assert base + self.block_size <= self.axi_lite_size, \
                f'block {name} outside the AXI-Lite window'
        assert len(set(self.block_bases.values())) == len(self.block_bases)
        assert set(self.snapshot_domains) == set(
            self.snapshot_clock_domains)
        # DDR windows must be 4 KiB aligned and must not overlap
        windows = [
            (self.ringv2_window_base, self.ringv2_window_size),
            (self.legacy_base, self.legacy_size),
            (self.memtest_window_base, self.memtest_window_size),
        ]
        for base, size in windows:
            assert base % 4096 == 0 and size % 4096 == 0
            assert base >= 0x2000_0000, 'DDR window below 0x2000_0000'
        windows.sort()
        for (b0, s0), (b1, _) in zip(windows, windows[1:]):
            assert b0 + s0 <= b1, 'DDR windows overlap'
        # The legacy ring (DmaStreamRingWrite) needs its base aligned to
        # the total ring size.
        assert self.legacy_base & (self.legacy_size - 1) == 0, \
            f'legacy_base {self.legacy_base:#x} not aligned to ring ' \
            f'size {self.legacy_size:#x}'
        assert self.ringv2_fifo_depth & (self.ringv2_fifo_depth - 1) == 0
        assert 1 <= self.ringv2_max_outstanding <= 15
        assert 1 <= self.memtest_max_outstanding <= 15


def default():
    """Default hwval configuration for Fishball Z7020"""
    return HwvalConfig()


# Named build configurations (``--config NAME`` in hwval_top.main()).
configs = {
    'default': default,
}
