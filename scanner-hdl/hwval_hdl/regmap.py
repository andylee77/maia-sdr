#
# Fishball hardware validation (hwval) - Register map
#
# The register names are the contract of doc/HW_VALIDATION_SUITE.md
# section 6.4; offsets are assigned here (declaration order inside each
# 256-byte block). This module only depends on the register table
# classes, so the JSON map / SVD / Markdown can be generated without the
# gateware submodules:
#
#   python -m hwval_hdl.regmap --json ../bench/share/hwval_regs.json \
#       --md ../doc/hwval_register_map.md --svd hwval.svd
#
# SPDX-License-Identifier: MIT
#

import argparse

from .axil_regs import Reg, RegBlock, RegField, RegisterTable
from .config import HwvalConfig
from . import config as config_mod

F = RegField

# Features implemented by this core (FEATURES register).
FEATURE_BITS = [
    ('ringv2', 'Ring buffer v2 (m_axi_ringv2)'),
    ('legacy', 'Production-replica legacy ring (m_axi_legacy)'),
    ('mt0', 'AXI memory tester mt0 (m_axi_mt0, HP0)'),
    ('mt1', 'AXI memory tester mt1 (m_axi_mt1, HP3)'),
    ('census', 'Clock census'),
    ('ingest', 'ADC ingest monitor'),
    ('prbs', 'Per-sample PRBS checker in the ingest monitor'),
    ('evt', 'CTRL_OUT event recorder'),
    ('y1_clk', 'y1_clk (50 MHz oscillator) counted by the census'),
    ('dna', 'PL device DNA (DNA_LO / DNA_HI / DNA_STATUS)'),
]

IRQ_BITS = [
    ('ringv2', 'Ring v2 interrupt (IRQ_EVERY / IRQ_TIMEOUT coalesced)'),
    ('legacy', 'Legacy ring sub-buffer completed'),
    ('mt0', 'Memory tester mt0 done'),
    ('mt1', 'Memory tester mt1 done'),
    ('census', 'Clock census done'),
    ('evt', 'Event FIFO non-empty (level: follows EVT_LEVEL != 0, '
            'IRQ_CLEAR has no effect on it)'),
]

SNAP_BITS = [
    ('sync', 'sync domain (clk, 62.5 MHz)'),
    ('mem', 'mem domain (clk2x_clk, 125 MHz)'),
    ('sampling', 'sampling domain (sampling_clk)'),
]


def _bit_fields(bits):
    return [F(name, j, 1, desc) for j, (name, desc) in enumerate(bits)]


def _pair(name, desc, *, snapshot, access='ro'):
    """_LO / _HI register pair of a 64-bit value."""
    return [
        Reg(f'{name}_LO', access, desc=f'{desc} (bits 31:0)',
            snapshot=snapshot),
        Reg(f'{name}_HI', access, desc=f'{desc} (bits 63:32)',
            snapshot=snapshot),
    ]


def rate_inc(rate_hz, clk_hz):
    """Phase increment of a RateGen for a given strobe rate."""
    return round(rate_hz / clk_hz * 2**32)


def _id_block(c):
    major, minor, patch = c.version
    return RegBlock('id', c.block_bases['id'], [
        Reg('ID', 'ro', const=c.id,
            desc=f'Core identification, 0x{c.id:08X} ("hwv1")'),
        Reg('VERSION', 'ro', width=24, const=c.version_word,
            desc='Core version, major<<16 | minor<<8 | patch',
            fields=[F('patch', 0, 8, 'Patch version'),
                    F('minor', 8, 8, 'Minor version'),
                    F('major', 16, 8, 'Major version')]),
        Reg('FEATURES', 'ro', width=len(FEATURE_BITS),
            const=(1 << len(FEATURE_BITS)) - 1,
            desc='Implemented features bitmask',
            fields=_bit_fields(FEATURE_BITS)),
        Reg('SCRATCH', 'rw', desc='Scratch register (no side effects)'),
        Reg('DNA_LO', 'ro',
            desc='PL device DNA bits 31:0 (DNA_PORT, read once after '
                 'reset; valid when DNA_STATUS.valid)'),
        Reg('DNA_HI', 'ro', width=25,
            desc='PL device DNA bits 56:32 (in bits 24:0)'),
        Reg('DNA_STATUS', 'ro', width=1, desc='Device DNA status',
            fields=[F('valid', 0, 1, 'DNA_LO / DNA_HI hold the 57-bit '
                                     'device DNA')]),
        Reg('CORE_RESET', 'rw', width=1, reset=0,
            desc='While 1, holds all non-AXI-Lite domains (sync, clk2x, '
                 'clk3x, sampling, lclk, fclk1, y1) in reset; also '
                 'clears GUARD_LOCK',
            fields=[F('reset', 0, 1, 'Core reset (active high)')]),
        Reg('SNAP_REQ', 'wo', width=len(SNAP_BITS),
            desc='Write a domain mask to request a snapshot of the '
                 'status registers of those domains',
            fields=_bit_fields(SNAP_BITS)),
        Reg('SNAP_ACK', 'ro', width=len(SNAP_BITS),
            desc='Domain mask of the snapshots completed for the last '
                 'SNAP_REQ write (poll until equal to the mask; 10 ms '
                 'timeout = that domain clock is dead)',
            fields=_bit_fields(SNAP_BITS)),
        Reg('SNAP_SEQ', 'ro',
            desc='Number of SNAP_REQ writes since reset (wraps)'),
        Reg('TS_LO', 'ro', snapshot='sync',
            desc='sync-domain free-running 64-bit timestamp, 62.5 MHz '
                 'cycles (bits 31:0)'),
        Reg('TS_HI', 'ro', snapshot='sync',
            desc='sync-domain free-running 64-bit timestamp (bits 63:32)'),
        Reg('IRQ_PENDING', 'ro', width=len(IRQ_BITS),
            desc='Pending interrupt sources', fields=_bit_fields(IRQ_BITS)),
        Reg('IRQ_ENABLE', 'rw', width=len(IRQ_BITS), reset=0,
            desc='Interrupt enables; interrupt_out = '
                 'OR(IRQ_PENDING & IRQ_ENABLE)',
            fields=_bit_fields(IRQ_BITS)),
        Reg('IRQ_CLEAR', 'w1c', width=len(IRQ_BITS),
            desc='Write 1 to clear the corresponding IRQ_PENDING bit '
                 '(reads 0)',
            fields=_bit_fields(IRQ_BITS)),
        Reg('IRQ_COUNT', 'ro',
            desc='Number of rising edges of interrupt_out (saturates)'),
        Reg('GUARD_LO', 'rw', reset=c.guard_lo_default,
            desc='Address guard low bound (inclusive) for ringv2, mt0, '
                 'mt1. Writes ignored while GUARD_LOCK = 1'),
        Reg('GUARD_HI', 'rw', reset=c.guard_hi_default,
            desc='Address guard high bound (exclusive) for ringv2, mt0, '
                 'mt1. Writes ignored while GUARD_LOCK = 1'),
        Reg('GUARD_LOCK', 'rw', width=1, reset=0,
            desc='Set once: after writing 1 the guard registers and this '
                 'bit are read-only until CORE_RESET',
            fields=[F('lock', 0, 1, 'Guard lock')]),
    ], desc='Identification, snapshot control, interrupts and the DDR '
            'address guard (AXI-Lite domain).')


CENSUS_CLOCKS = [
    # (register suffix, census name, clock domain, description)
    ('SYNC', 'sync', 'sync', 'clk (clk_out1, nominal 62.5 MHz)'),
    ('MEM', 'mem', 'clk2x', 'clk2x_clk (clk_out2, nominal 125 MHz)'),
    ('CLK3X', 'clk3x', 'clk3x', 'clk3x_clk (clk_out3, nominal 187.5 MHz)'),
    ('SAMPLING', 'sampling', 'sampling',
     'sampling_clk (util_ad9361_divclk/clk_out)'),
    ('LCLK', 'lclk', 'lclk', 'lclk_clk (axi_ad9361 l_clk)'),
    ('FCLK1', 'fclk1', 'fclk1', 'fclk1_clk (FCLK1, nominal 200 MHz)'),
    ('Y1', 'y1', 'y1', 'y1_clk (50 MHz oscillator, pin N18)'),
]


def _census_block(c):
    regs = [
        Reg('CENSUS_CTRL', 'wo', width=1, desc='Census command',
            fields=[F('start', 0, 1, 'Start a measurement gate')]),
        Reg('CENSUS_GATE', 'rw', reset=c.s_axi_lite_hz,
            desc='Gate length in s_axi_lite (100 MHz) cycles'),
        Reg('CENSUS_STATUS', 'ro', width=2, desc='Census status',
            fields=[F('busy', 0, 1, 'Measurement in progress'),
                    F('done', 1, 1, 'Counts valid')]),
        Reg('CENSUS_GATE_ACTUAL', 'ro',
            desc='Actual gate length in s_axi_lite cycles; '
                 'f = count / GATE_ACTUAL x 100 MHz'),
    ]
    for suffix, _, _, desc in CENSUS_CLOCKS:
        regs.append(Reg(f'CENSUS_{suffix}', 'ro',
                        desc=f'Rising edges of {desc} during the gate'))
    regs.append(Reg('CENSUS_CLKOUT', 'ro',
                    desc='Rising edges of the AD9361 CLK_OUT (ad_clkout, '
                         'pin R16) sampled in clk3x during the gate'))
    return RegBlock('census', c.block_bases['census'], regs,
                    desc='Clock census: edges of every clock counted '
                         'against FCLK0 (AXI-Lite domain, no snapshot).')


def _ingest_block(c):
    s = 'sampling'
    regs = [
        Reg('INGEST_CTRL', 'rw', width=5, reset=0x01,
            desc='Ingest monitor control (quasi-static, sampling domain)',
            fields=[F('stats_enable', 0, 1, 'Enable statistics'),
                    F('prbs_enable', 1, 1, 'Enable the PRBS checker'),
                    F('prbs_mode', 2, 1,
                      '0 = AD9361 BIST PRBS (pn0fn), 1 = PN9/PN11'),
                    F('honor_valid', 3, 1,
                      'Honour valid_in (1) or take every sampling clock '
                      '(0, production behaviour) in the monitor and the '
                      'sampling->sync CDC'),
                    F('clear_on_snap', 4, 1,
                      'Clear the window statistics on every sampling '
                      'snapshot')]),
        Reg('INGEST_CMD', 'wo', width=1, desc='Ingest monitor command',
            fields=[F('clear', 0, 1, 'Clear all ingest counters')]),
        *_pair('SAMPLES', 'Samples seen', snapshot=s),
        Reg('VALID_GAP_CYCLES', 'ro', snapshot=s,
            desc='sampling cycles with valid_in = 0'),
        Reg('VALID_GAP_RUNS', 'ro', snapshot=s,
            desc='Runs of consecutive valid_in = 0 cycles'),
        Reg('CDC_WRERR', 'ro', snapshot=s,
            desc='sampling->sync CDC FIFO write errors (overflows)'),
        Reg('CDC_FULL_CYCLES', 'ro', snapshot=s,
            desc='sampling cycles with the sampling->sync CDC FIFO full '
                 '(extension)'),
        Reg('I_MIN', 'ro', snapshot=s,
            desc='Minimum I in the window (sign-extended 12-bit)'),
        Reg('I_MAX', 'ro', snapshot=s,
            desc='Maximum I in the window (sign-extended 12-bit)'),
        Reg('Q_MIN', 'ro', snapshot=s,
            desc='Minimum Q in the window (sign-extended 12-bit)'),
        Reg('Q_MAX', 'ro', snapshot=s,
            desc='Maximum Q in the window (sign-extended 12-bit)'),
        *_pair('I_SUM', 'Sum of I in the window (sign-extended 48-bit)',
               snapshot=s),
        *_pair('Q_SUM', 'Sum of Q in the window (sign-extended 48-bit)',
               snapshot=s),
        *_pair('I_SUMSQ', 'Sum of I^2 in the window', snapshot=s),
        *_pair('Q_SUMSQ', 'Sum of Q^2 in the window', snapshot=s),
        Reg('CLIP_COUNT', 'ro', snapshot=s,
            desc='Samples at full scale (-2048 or 2047) on I or Q'),
        Reg('I_OR_MASK', 'ro', width=12, snapshot=s,
            desc='OR of all I samples in the window (stuck-at-0 bits)'),
        Reg('I_AND_MASK', 'ro', width=12, snapshot=s,
            desc='AND of all I samples in the window (stuck-at-1 bits)'),
        Reg('Q_OR_MASK', 'ro', width=12, snapshot=s,
            desc='OR of all Q samples in the window (stuck-at-0 bits)'),
        Reg('Q_AND_MASK', 'ro', width=12, snapshot=s,
            desc='AND of all Q samples in the window (stuck-at-1 bits)'),
        *_pair('WIN_SAMPLES', 'Samples in the statistics window',
               snapshot=s),
        *_pair('PRBS_CHECKED', 'Samples checked by the PRBS checker',
               snapshot=s),
        Reg('PRBS_ERRORS', 'ro', snapshot=s,
            desc='PRBS sample errors'),
        Reg('PRBS_OOS_EVENTS', 'ro', snapshot=s,
            desc='PRBS loss-of-sync events'),
        Reg('PRBS_STATUS', 'ro', width=1, snapshot=s, desc='PRBS status',
            fields=[F('in_sync', 0, 1, 'PRBS checker in sync')]),
    ]
    return RegBlock('ingest', c.block_bases['ingest'], regs,
                    desc='ADC ingest monitor on re_in/im_in/valid_in '
                         '(sampling domain; status via the sampling '
                         'snapshot, which also freezes the statistics '
                         'window).')


def _legacy_block(c):
    s = 'sync'
    regs = [
        Reg('LEGACY_CTRL', 'rw', width=3, reset=0,
            desc='Legacy ring control (quasi-static, sync domain)',
            fields=[F('dma_enable', 0, 1, 'DmaStreamRingWrite enable'),
                    F('src', 1, 2,
                      'Source: 0 off, 1 sample ramp, 2 live rxiq')]),
        Reg('LEGACY_CMD', 'wo', width=1, desc='Legacy ring command',
            fields=[F('clear', 0, 1, 'Clear counters and generator')]),
        Reg('LEGACY_RATE_INC', 'rw',
            reset=rate_inc(8_000_000, c.sync_hz),
            desc='Sample strobe rate = inc / 2^32 x 62.5 MHz (default '
                 '8 MSPS)'),
        Reg('LEGACY_BASE', 'ro', const=c.legacy_base,
            desc='Ring base address (fixed)'),
        Reg('LEGACY_SIZE', 'ro', const=c.legacy_size,
            desc='Ring size in bytes (NUM_BUFFERS x sub-buffer size)'),
        Reg('LEGACY_NUM_BUFFERS', 'ro', const=c.legacy_num_buffers,
            desc='Number of sub-buffers'),
        Reg('LEGACY_LAST_BUFFER', 'ro', snapshot=s,
            desc='Last completed sub-buffer index'),
        Reg('LEGACY_NEXT_ADDRESS', 'ro', snapshot=s,
            desc='Next write address of the ring DMA'),
        Reg('LEGACY_PACKER_OVF', 'ro', snapshot=s,
            desc='IQPacker overflow pulses while the DMA is enabled'),
        Reg('LEGACY_PACKER_OVF_DISABLED', 'ro', snapshot=s,
            desc='IQPacker overflow pulses while the DMA is disabled'),
        Reg('LEGACY_WORDS_IN', 'ro', snapshot=s,
            desc='64-bit words produced by the packer'),
        Reg('LEGACY_WORDS_ACCEPTED', 'ro', snapshot=s,
            desc='64-bit words accepted by the ring DMA'),
        Reg('LEGACY_AW', 'ro', snapshot=s, desc='AW handshakes'),
        Reg('LEGACY_B', 'ro', snapshot=s, desc='B handshakes'),
        Reg('LEGACY_BRESP_ERR', 'ro', snapshot=s,
            desc='BRESP != OKAY count'),
        Reg('LEGACY_SUBBUF_DONE', 'ro', snapshot=s,
            desc='Completed sub-buffers'),
        Reg('LEGACY_STALL_CYCLES', 'ro', snapshot=s,
            desc='Cycles with packer data waiting on the DMA'),
        Reg('LEGACY_MAX_STALL', 'ro', snapshot=s,
            desc='Longest stall in sync cycles'),
        Reg('LEGACY_MAX_OUTSTANDING', 'ro', snapshot=s,
            desc='Maximum un-acknowledged bursts seen'),
        Reg('LEGACY_LAT_MAX', 'ro', snapshot=s,
            desc='Maximum AW-to-B write latency in sync cycles'),
        Reg('LEGACY_HIST_SEL', 'rw', width=4, reset=0,
            desc='Write-latency histogram bin selected for '
                 'LEGACY_HIST_VAL (log2 bins)'),
        Reg('LEGACY_HIST_VAL', 'ro', snapshot=s,
            desc='Count of the histogram bin selected by LEGACY_HIST_SEL '
                 '(set HIST_SEL, then SNAP_REQ)'),
        *_pair('LEGACY_GEN', 'Samples produced by the source generator',
               snapshot=s),
    ]
    return RegBlock('legacy', c.block_bases['legacy'], regs,
                    desc='Production replica: IQPacker -> '
                         'DmaStreamRingWrite at a fixed base '
                         f'(0x{c.legacy_base:08X}, '
                         f'{c.legacy_num_buffers} x '
                         f'{c.legacy_buffer_size >> 20} MiB) with '
                         'non-invasive counters (sync domain).')


def _ringv2_block(c):
    s = 'sync'
    regs = [
        Reg('RINGV2_CTRL', 'rw', width=16, reset=0x0300,
            desc='Ring v2 control (sync domain)',
            fields=[F('enable', 0, 1, 'Enable (takes effect at burst '
                                      'boundaries; disabling drains)'),
                    F('protect', 1, 1, 'Protect mode (never overwrite '
                                       'past CONSUMER_BURSTS)'),
                    F('header', 2, 1, 'Sub-buffer header words'),
                    F('src', 3, 3, 'Source: 0 off, 1 ramp64, 2 tagged, '
                                   '3 prbs31, 4 live rxiq'),
                    F('irq_threshold', 8, 1,
                      'IRQ every RINGV2_IRQ_EVERY committed bursts'),
                    F('irq_timer', 9, 1,
                      'IRQ RINGV2_IRQ_TIMEOUT cycles after the last IRQ '
                      'if bursts were committed'),
                    F('tag', 12, 4, 'Tag nibble of the tagged pattern '
                                    '(extension)')]),
        Reg('RINGV2_CMD', 'wo', width=3, desc='Ring v2 commands',
            fields=[F('soft_reset', 0, 1, 'Soft reset (accepted when '
                                          'idle; bumps EPOCH)'),
                    F('flush', 1, 1, 'Flush the partial burst'),
                    F('clear', 2, 1, 'Clear counters and generator')]),
        Reg('RINGV2_BASE', 'rw', reset=c.ringv2_window_base,
            desc='Ring base address (4 KiB aligned)'),
        Reg('RINGV2_SIZE_BURSTS', 'rw', reset=c.ringv2_window_bursts,
            desc='Ring size in 128-byte bursts (>= 2)'),
        Reg('RINGV2_SUBBUF_BURSTS', 'rw', width=16, reset=8192,
            desc='Sub-buffer size in bursts (header period)'),
        Reg('RINGV2_IRQ_EVERY', 'rw', width=16, reset=1024,
            desc='IRQ every N committed bursts (with CTRL.irq_threshold)'),
        Reg('RINGV2_IRQ_TIMEOUT', 'rw', reset=c.sync_hz // 100,
            desc='IRQ timeout in sync cycles (with CTRL.irq_timer)'),
        Reg('RINGV2_FLUSH_TIMEOUT', 'rw', reset=c.sync_hz // 1000,
            desc='Idle sync cycles before a partial burst is padded and '
                 'written'),
        Reg('RINGV2_MAX_OUTSTANDING', 'rw', width=4,
            reset=c.ringv2_max_outstanding,
            desc='Maximum outstanding bursts (1-8)'),
        Reg('RINGV2_RATE_INC', 'rw',
            reset=rate_inc(4_000_000, c.sync_hz),
            desc='Generator word rate = inc / 2^32 x 62.5 MHz (default '
                 '4 Mword/s = 32 MB/s)'),
        Reg('RINGV2_CONSUMER_BURSTS', 'rw', reset=0,
            desc='Bursts consumed by the PS (protect mode); crossed '
                 'coherently at any time'),
        Reg('RINGV2_COMMITTED_BURSTS', 'ro',
            desc='Bursts committed (write responses received; burst k '
                 'is at ring slot k mod SIZE_BURSTS, non-OKAY responses '
                 'also count in RINGV2_BRESP_ERR). Monotonic, gray-code '
                 'synchronized, read any time without a snapshot'),
        Reg('RINGV2_STATUS', 'ro', width=3, desc='Live status',
            fields=[F('idle', 0, 1, 'No burst in flight'),
                    F('enabled', 1, 1, 'Producer enabled'),
                    F('fifo_empty', 2, 1, 'FIFO empty')]),
        Reg('RINGV2_ISSUED_BURSTS', 'ro', snapshot=s,
            desc='Bursts issued (AW)'),
        *_pair('RINGV2_WORDS_IN', 'Words offered by the source',
               snapshot=s),
        Reg('RINGV2_DROP_FULL', 'ro', snapshot=s,
            desc='Words dropped because the FIFO was full'),
        Reg('RINGV2_DROP_PROTECT', 'ro', snapshot=s,
            desc='Words dropped by protect mode'),
        Reg('RINGV2_PAD_WORDS', 'ro', snapshot=s,
            desc='Pad words written by flushes'),
        Reg('RINGV2_FLUSHES', 'ro', snapshot=s, desc='Flushes'),
        Reg('RINGV2_BRESP_ERR', 'ro', snapshot=s,
            desc='BRESP != OKAY count'),
        Reg('RINGV2_FIFO_HWM', 'ro', width=16, snapshot=s,
            desc='FIFO high-water mark (words)'),
        Reg('RINGV2_MAX_OUTSTANDING_SEEN', 'ro', width=4, snapshot=s,
            desc='Maximum outstanding bursts seen'),
        Reg('RINGV2_LAT_MAX', 'ro', snapshot=s,
            desc='Maximum AW-to-B latency in sync cycles'),
        Reg('RINGV2_HIST_SEL', 'rw', width=4, reset=0,
            desc='Latency histogram bin selected for RINGV2_HIST_VAL'),
        Reg('RINGV2_HIST_VAL', 'ro', snapshot=s,
            desc='Count of the selected latency histogram bin'),
        Reg('RINGV2_EPOCH', 'ro', snapshot=s,
            desc='Soft-reset epoch counter'),
        Reg('RINGV2_HEADERS', 'ro', snapshot=s,
            desc='Sub-buffer headers written'),
        *_pair('RINGV2_GEN', 'Words produced by the source generator',
               snapshot=s),
        Reg('RINGV2_GUARD_BLOCKED', 'ro', snapshot=s,
            desc='Bursts not issued because they fell outside '
                 '[GUARD_LO, GUARD_HI)'),
    ]
    return RegBlock('ringv2', c.block_bases['ringv2'], regs,
                    desc='Ring buffer v2 (section 7), sync domain, '
                         'm_axi_ringv2.')


def _mt_block(c, n):
    p = f'MT{n}_'
    s = 'mem'
    base = c.mt0_default_base if n == 0 else c.mt1_default_base
    ctrl_reset = (2 | (0 << 3) | (16 << 7)
                  | (c.memtest_max_outstanding << 12))
    regs = [
        Reg(p + 'CTRL', 'rw', width=17, reset=ctrl_reset,
            desc='Memory tester configuration (quasi-static, mem domain)',
            fields=[F('mode', 0, 3, '0 write-only, 1 read-verify, 2 write-'
                                    'then-verify, 3 read-only, 4 byte-'
                                    'lane'),
                    F('pattern', 3, 4, '0 address, 1 walking-1, '
                                       '2 walking-0, 3 checkerboard, '
                                       '4 PRBS, 5 all-0, 6 all-1, '
                                       '7 toggle'),
                    F('burst_len', 7, 5, 'Beats per burst: 1, 2, 4, 8 '
                                         'or 16'),
                    F('max_outstanding', 12, 4, 'Outstanding bursts '
                                                '(1-8)'),
                    F('stop_on_error', 16, 1, 'Stop at the first error')]),
        Reg(p + 'CMD', 'wo', width=3, desc='Memory tester commands',
            fields=[F('start', 0, 1, 'Start'),
                    F('abort', 1, 1, 'Abort'),
                    F('clear', 2, 1, 'Clear counters')]),
        Reg(p + 'BASE', 'rw', reset=base, desc='Test region base address'),
        Reg(p + 'SIZE', 'rw', reset=c.mt_default_size,
            desc='Test region size in bytes'),
        Reg(p + 'PASSES', 'rw', width=16, reset=1,
            desc='Number of passes (0 = until abort)'),
        Reg(p + 'IDLE_CYCLES', 'rw', width=16, reset=0,
            desc='Idle cycles between bursts (aggressor duty cycle)'),
        Reg(p + 'SEED', 'rw', reset=1, desc='PRBS pattern seed'),
        Reg(p + 'STATUS', 'ro', width=3, desc='Live status',
            fields=[F('busy', 0, 1, 'Running'),
                    F('done', 1, 1, 'Finished'),
                    F('error', 2, 1, 'Data or response error seen')]),
        Reg(p + 'PASS_COUNT', 'ro', snapshot=s, desc='Completed passes'),
        *_pair(p + 'BYTES_WR', 'Bytes written', snapshot=s),
        *_pair(p + 'BYTES_RD', 'Bytes read', snapshot=s),
        *_pair(p + 'CYCLES', 'Active clk2x cycles', snapshot=s),
        Reg(p + 'ERR_COUNT', 'ro', snapshot=s, desc='Data errors'),
        Reg(p + 'FIRST_ERR_ADDR', 'ro', snapshot=s,
            desc='Address of the first data error'),
        *_pair(p + 'FIRST_ERR_EXP', 'Expected data of the first error',
               snapshot=s),
        *_pair(p + 'FIRST_ERR_ACT', 'Actual data of the first error',
               snapshot=s),
        *_pair(p + 'ERR_LANES', 'OR of the error bits (DQ lanes)',
               snapshot=s),
        Reg(p + 'BRESP_ERR', 'ro', snapshot=s, desc='BRESP != OKAY count'),
        Reg(p + 'RRESP_ERR', 'ro', snapshot=s, desc='RRESP != OKAY count'),
        Reg(p + 'WLAT_MAX', 'ro', snapshot=s,
            desc='Maximum write latency (AW to B) in clk2x cycles'),
        Reg(p + 'RLAT_MAX', 'ro', snapshot=s,
            desc='Maximum read latency (AR to RLAST) in clk2x cycles'),
        Reg(p + 'HIST_SEL', 'rw', width=5, reset=0,
            desc='Latency histogram selection',
            fields=[F('bin', 0, 4, 'log2 latency bin'),
                    F('read', 4, 1, '0 = write latency, 1 = read '
                                    'latency')]),
        Reg(p + 'HIST_VAL', 'ro', snapshot=s,
            desc='Count of the selected histogram bin'),
        Reg(p + 'GUARD_BLOCKED', 'ro', snapshot=s,
            desc='Bursts not issued because they fell outside '
                 '[GUARD_LO, GUARD_HI)'),
    ]
    port = 'HP0' if n == 0 else 'HP3'
    return RegBlock(f'mt{n}', c.block_bases[f'mt{n}'], regs,
                    desc=f'AXI memory tester mt{n} (section 8), mem '
                         f'domain (clk2x, 125 MHz), m_axi_mt{n} -> '
                         f'{port}.')


def _evt_block(c):
    return RegBlock('evt', c.block_bases['evt'], [
        Reg('EVT_CTRL', 'rw', width=16, reset=0xFF00,
            desc='Event recorder control (quasi-static, sync domain)',
            fields=[F('enable', 0, 1, 'Record CTRL_OUT transitions'),
                    F('mask', 8, 8, 'CTRL_OUT bits that generate '
                                    'events')]),
        Reg('EVT_LEVEL', 'ro', desc='Event FIFO level'),
        Reg('EVT_DATA_LO', 'ro',
            desc='FIFO head, bits 31:0 of the 36-bit record '
                 '(read before EVT_POP)',
            fields=[F('timestamp', 0, 27, 'Timestamp in 62.5 MHz cycles '
                                          '(low 27 bits)'),
                    F('value_lo', 27, 5, 'CTRL_OUT value bits 4:0')]),
        Reg('EVT_DATA_HI', 'ro', width=4,
            desc='FIFO head, bits 35:32 of the 36-bit record',
            fields=[F('value_hi', 0, 3, 'CTRL_OUT value bits 7:5'),
                    F('heartbeat', 3, 1, 'Heartbeat record (every '
                                         f'2^{c.evt_heartbeat_log2} '
                                         'cycles)')]),
        Reg('EVT_POP', 'wo', width=1, desc='Pop the FIFO head',
            fields=[F('pop', 0, 1, 'Pop')]),
        Reg('EVT_OVERFLOWS', 'ro', snapshot='sync',
            desc='Events lost because the FIFO was full'),
        Reg('EVT_CURRENT', 'ro', width=8,
            desc='Live CTRL_OUT value (synchronized)'),
        Reg('EVT_CMD', 'wo', width=1,
            desc='Event recorder command (extension)',
            fields=[F('clear', 0, 1, 'Clear EVT_OVERFLOWS')]),
    ], desc='AD9361 CTRL_OUT event recorder. Records are 36 bits: '
            'bit35 heartbeat, bits[34:27] CTRL_OUT value, bits[26:0] '
            'timestamp (low 27 bits of TS, 62.5 MHz cycles).')


def build_register_table(config: HwvalConfig = None) -> RegisterTable:
    """Build the hwval register table (single source of truth)."""
    if config is None:
        config = HwvalConfig()
    config.validate()
    c = config
    blocks = [
        _id_block(c),
        _census_block(c),
        _ingest_block(c),
        _legacy_block(c),
        _ringv2_block(c),
        _mt_block(c, 0),
        _mt_block(c, 1),
        _evt_block(c),
    ]
    return RegisterTable(
        'hwval', c.axi_lite_base, c.axi_lite_size, blocks,
        id_reg='ID', id_value=c.id, version=c.version_str,
        snapshot_domains=c.snapshot_domains, block_size=c.block_size,
        description=f'Fishball hwval validation core '
                    f'(platform {c.platform})')


MARKDOWN_PREAMBLE = """\
Generated from `scanner-hdl/hwval_hdl/regmap.py` (`build_register_table()`),
the single source of truth for the `hwval` register map. Do not edit by
hand; regenerate with
`python -m hwval_hdl.hwval_top --md ../doc/hwval_register_map.md`
(or `python -m hwval_hdl.regmap --md ...`). The design contract, including
the register access rules, is `doc/HW_VALIDATION_SUITE.md` section 6.

Snapshot protocol: write the domain mask to `SNAP_REQ`, poll `SNAP_ACK`
until it equals the mask (10 ms timeout means that domain's clock is
dead), then read the registers whose Snapshot column names that domain.
The 64-bit `_LO`/`_HI` pairs come from the same snapshot. Counters never
clear on read. Configuration registers of other domains are
quasi-static: change them while the block is disabled or idle."""


def write_json(path, config=None):
    with open(path, 'w', newline='\n') as f:
        f.write(build_register_table(config).to_json())


def write_svd(path, config=None):
    with open(path, 'wb') as f:
        f.write(build_register_table(config).to_svd())


def write_markdown(path, config=None):
    table = build_register_table(config)
    with open(path, 'w', newline='\n') as f:
        f.write(table.to_markdown(title='hwval register map',
                                  preamble=MARKDOWN_PREAMBLE))


def main():
    parser = argparse.ArgumentParser(
        description='Generate the hwval register map (JSON/SVD/Markdown)')
    parser.add_argument('--config', default='default',
                        help='hwval configuration name [default=%(default)r]')
    parser.add_argument('--json', help='Output JSON register map')
    parser.add_argument('--svd', help='Output SVD file')
    parser.add_argument('--md', help='Output Markdown document')
    args = parser.parse_args()
    config = config_mod.configs[args.config]()
    if args.json:
        write_json(args.json, config)
    if args.svd:
        write_svd(args.svd, config)
    if args.md:
        write_markdown(args.md, config)


if __name__ == '__main__':
    main()
