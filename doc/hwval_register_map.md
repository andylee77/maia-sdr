# hwval register map

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
quasi-static: change them while the block is disabled or idle.

- Core: `hwval`, version `0.1.0`
- AXI-Lite base: `0x7C460000`, size 4096 bytes; offsets below are from the base
- Identification: `ID` reads `0x68777631`
- Snapshot domains (`SNAP_REQ` / `SNAP_ACK` bits): `sync` = 0x1, `mem` = 0x2, `sampling` = 0x4
- Unmapped addresses read `0xDEADBEEF`; writes to them are ignored; every access completes with OKAY
- Access: `ro` read-only, `rw` read-write, `wo` write-only (one-cycle pulse, reads 0), `w1c` write-one-to-clear

## Blocks

| Offset | Block | Registers | Description |
|---|---|---|---|
| `0x000` | `id` | 20 | Identification, snapshot control, interrupts and the DDR address guard (AXI-Lite domain). |
| `0x100` | `census` | 12 | Clock census: edges of every clock counted against FCLK0 (AXI-Lite domain, no snapshot). |
| `0x200` | `ingest` | 32 | ADC ingest monitor on re_in/im_in/valid_in (sampling domain; status via the sampling snapshot, which also freezes the statistics window). |
| `0x400` | `legacy` | 24 | Production replica: IQPacker -> DmaStreamRingWrite at a fixed base (0x22000000, 16 x 1 MiB) with non-invasive counters (sync domain). |
| `0x500` | `ringv2` | 31 | Ring buffer v2 (section 7), sync domain, m_axi_ringv2. |
| `0x600` | `mt0` | 30 | AXI memory tester mt0 (section 8), mem domain (clk2x, 125 MHz), m_axi_mt0 -> HP0. |
| `0x700` | `mt1` | 30 | AXI memory tester mt1 (section 8), mem domain (clk2x, 125 MHz), m_axi_mt1 -> HP3. |
| `0x800` | `evt` | 8 | AD9361 CTRL_OUT event recorder. Records are 36 bits: bit35 heartbeat, bits[34:27] CTRL_OUT value, bits[26:0] timestamp (low 27 bits of TS, 62.5 MHz cycles). |

## Block `id` (0x000)

Identification, snapshot control, interrupts and the DDR address guard (AXI-Lite domain).

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x000` | `ID` | ro | 32 | `0x68777631` | - | Core identification, 0x68777631 ("hwv1") |
| `0x004` | `VERSION` | ro | 24 | `0x100` | - | Core version, major<<16 \| minor<<8 \| patch |
| `0x008` | `FEATURES` | ro | 10 | `0x3FF` | - | Implemented features bitmask |
| `0x00C` | `SCRATCH` | rw | 32 | `0x0` | - | Scratch register (no side effects) |
| `0x010` | `DNA_LO` | ro | 32 | - | - | PL device DNA bits 31:0 (DNA_PORT, read once after reset; valid when DNA_STATUS.valid) |
| `0x014` | `DNA_HI` | ro | 25 | - | - | PL device DNA bits 56:32 (in bits 24:0) |
| `0x018` | `DNA_STATUS` | ro | 1 | - | - | Device DNA status |
| `0x01C` | `CORE_RESET` | rw | 1 | `0x0` | - | While 1, holds all non-AXI-Lite domains (sync, clk2x, clk3x, sampling, lclk, fclk1, y1) in reset; also clears GUARD_LOCK |
| `0x020` | `SNAP_REQ` | wo | 3 | `0x0` | - | Write a domain mask to request a snapshot of the status registers of those domains |
| `0x024` | `SNAP_ACK` | ro | 3 | - | - | Domain mask of the snapshots completed for the last SNAP_REQ write (poll until equal to the mask; 10 ms timeout = that domain clock is dead) |
| `0x028` | `SNAP_SEQ` | ro | 32 | - | - | Number of SNAP_REQ writes since reset (wraps) |
| `0x02C` | `TS_LO` | ro | 32 | - | sync | sync-domain free-running 64-bit timestamp, 62.5 MHz cycles (bits 31:0) |
| `0x030` | `TS_HI` | ro | 32 | - | sync | sync-domain free-running 64-bit timestamp (bits 63:32) |
| `0x034` | `IRQ_PENDING` | ro | 6 | - | - | Pending interrupt sources |
| `0x038` | `IRQ_ENABLE` | rw | 6 | `0x0` | - | Interrupt enables; interrupt_out = OR(IRQ_PENDING & IRQ_ENABLE) |
| `0x03C` | `IRQ_CLEAR` | w1c | 6 | `0x0` | - | Write 1 to clear the corresponding IRQ_PENDING bit (reads 0) |
| `0x040` | `IRQ_COUNT` | ro | 32 | - | - | Number of rising edges of interrupt_out (saturates) |
| `0x044` | `GUARD_LO` | rw | 32 | `0x20000000` | - | Address guard low bound (inclusive) for ringv2, mt0, mt1. Writes ignored while GUARD_LOCK = 1 |
| `0x048` | `GUARD_HI` | rw | 32 | `0x28000000` | - | Address guard high bound (exclusive) for ringv2, mt0, mt1. Writes ignored while GUARD_LOCK = 1 |
| `0x04C` | `GUARD_LOCK` | rw | 1 | `0x0` | - | Set once: after writing 1 the guard registers and this bit are read-only until CORE_RESET |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `VERSION` | `patch` | [7:0] | Patch version |
| `VERSION` | `minor` | [15:8] | Minor version |
| `VERSION` | `major` | [23:16] | Major version |
| `FEATURES` | `ringv2` | [0] | Ring buffer v2 (m_axi_ringv2) |
| `FEATURES` | `legacy` | [1] | Production-replica legacy ring (m_axi_legacy) |
| `FEATURES` | `mt0` | [2] | AXI memory tester mt0 (m_axi_mt0, HP0) |
| `FEATURES` | `mt1` | [3] | AXI memory tester mt1 (m_axi_mt1, HP3) |
| `FEATURES` | `census` | [4] | Clock census |
| `FEATURES` | `ingest` | [5] | ADC ingest monitor |
| `FEATURES` | `prbs` | [6] | Per-sample PRBS checker in the ingest monitor |
| `FEATURES` | `evt` | [7] | CTRL_OUT event recorder |
| `FEATURES` | `y1_clk` | [8] | y1_clk (50 MHz oscillator) counted by the census |
| `FEATURES` | `dna` | [9] | PL device DNA (DNA_LO / DNA_HI / DNA_STATUS) |
| `DNA_STATUS` | `valid` | [0] | DNA_LO / DNA_HI hold the 57-bit device DNA |
| `CORE_RESET` | `reset` | [0] | Core reset (active high) |
| `SNAP_REQ` | `sync` | [0] | sync domain (clk, 62.5 MHz) |
| `SNAP_REQ` | `mem` | [1] | mem domain (clk2x_clk, 125 MHz) |
| `SNAP_REQ` | `sampling` | [2] | sampling domain (sampling_clk) |
| `SNAP_ACK` | `sync` | [0] | sync domain (clk, 62.5 MHz) |
| `SNAP_ACK` | `mem` | [1] | mem domain (clk2x_clk, 125 MHz) |
| `SNAP_ACK` | `sampling` | [2] | sampling domain (sampling_clk) |
| `IRQ_PENDING` | `ringv2` | [0] | Ring v2 interrupt (IRQ_EVERY / IRQ_TIMEOUT coalesced) |
| `IRQ_PENDING` | `legacy` | [1] | Legacy ring sub-buffer completed |
| `IRQ_PENDING` | `mt0` | [2] | Memory tester mt0 done |
| `IRQ_PENDING` | `mt1` | [3] | Memory tester mt1 done |
| `IRQ_PENDING` | `census` | [4] | Clock census done |
| `IRQ_PENDING` | `evt` | [5] | Event FIFO non-empty (level: follows EVT_LEVEL != 0, IRQ_CLEAR has no effect on it) |
| `IRQ_ENABLE` | `ringv2` | [0] | Ring v2 interrupt (IRQ_EVERY / IRQ_TIMEOUT coalesced) |
| `IRQ_ENABLE` | `legacy` | [1] | Legacy ring sub-buffer completed |
| `IRQ_ENABLE` | `mt0` | [2] | Memory tester mt0 done |
| `IRQ_ENABLE` | `mt1` | [3] | Memory tester mt1 done |
| `IRQ_ENABLE` | `census` | [4] | Clock census done |
| `IRQ_ENABLE` | `evt` | [5] | Event FIFO non-empty (level: follows EVT_LEVEL != 0, IRQ_CLEAR has no effect on it) |
| `IRQ_CLEAR` | `ringv2` | [0] | Ring v2 interrupt (IRQ_EVERY / IRQ_TIMEOUT coalesced) |
| `IRQ_CLEAR` | `legacy` | [1] | Legacy ring sub-buffer completed |
| `IRQ_CLEAR` | `mt0` | [2] | Memory tester mt0 done |
| `IRQ_CLEAR` | `mt1` | [3] | Memory tester mt1 done |
| `IRQ_CLEAR` | `census` | [4] | Clock census done |
| `IRQ_CLEAR` | `evt` | [5] | Event FIFO non-empty (level: follows EVT_LEVEL != 0, IRQ_CLEAR has no effect on it) |
| `GUARD_LOCK` | `lock` | [0] | Guard lock |

## Block `census` (0x100)

Clock census: edges of every clock counted against FCLK0 (AXI-Lite domain, no snapshot).

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x100` | `CENSUS_CTRL` | wo | 1 | `0x0` | - | Census command |
| `0x104` | `CENSUS_GATE` | rw | 32 | `0x5F5E100` | - | Gate length in s_axi_lite (100 MHz) cycles |
| `0x108` | `CENSUS_STATUS` | ro | 2 | - | - | Census status |
| `0x10C` | `CENSUS_GATE_ACTUAL` | ro | 32 | - | - | Actual gate length in s_axi_lite cycles; f = count / GATE_ACTUAL x 100 MHz |
| `0x110` | `CENSUS_SYNC` | ro | 32 | - | - | Rising edges of clk (clk_out1, nominal 62.5 MHz) during the gate |
| `0x114` | `CENSUS_MEM` | ro | 32 | - | - | Rising edges of clk2x_clk (clk_out2, nominal 125 MHz) during the gate |
| `0x118` | `CENSUS_CLK3X` | ro | 32 | - | - | Rising edges of clk3x_clk (clk_out3, nominal 187.5 MHz) during the gate |
| `0x11C` | `CENSUS_SAMPLING` | ro | 32 | - | - | Rising edges of sampling_clk (util_ad9361_divclk/clk_out) during the gate |
| `0x120` | `CENSUS_LCLK` | ro | 32 | - | - | Rising edges of lclk_clk (axi_ad9361 l_clk) during the gate |
| `0x124` | `CENSUS_FCLK1` | ro | 32 | - | - | Rising edges of fclk1_clk (FCLK1, nominal 200 MHz) during the gate |
| `0x128` | `CENSUS_Y1` | ro | 32 | - | - | Rising edges of y1_clk (50 MHz oscillator, pin N18) during the gate |
| `0x12C` | `CENSUS_CLKOUT` | ro | 32 | - | - | Rising edges of the AD9361 CLK_OUT (ad_clkout, pin R16) sampled in clk3x during the gate |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `CENSUS_CTRL` | `start` | [0] | Start a measurement gate |
| `CENSUS_STATUS` | `busy` | [0] | Measurement in progress |
| `CENSUS_STATUS` | `done` | [1] | Counts valid |

## Block `ingest` (0x200)

ADC ingest monitor on re_in/im_in/valid_in (sampling domain; status via the sampling snapshot, which also freezes the statistics window).

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x200` | `INGEST_CTRL` | rw | 5 | `0x1` | - | Ingest monitor control (quasi-static, sampling domain) |
| `0x204` | `INGEST_CMD` | wo | 1 | `0x0` | - | Ingest monitor command |
| `0x208` | `SAMPLES_LO` | ro | 32 | - | sampling | Samples seen (bits 31:0) |
| `0x20C` | `SAMPLES_HI` | ro | 32 | - | sampling | Samples seen (bits 63:32) |
| `0x210` | `VALID_GAP_CYCLES` | ro | 32 | - | sampling | sampling cycles with valid_in = 0 |
| `0x214` | `VALID_GAP_RUNS` | ro | 32 | - | sampling | Runs of consecutive valid_in = 0 cycles |
| `0x218` | `CDC_WRERR` | ro | 32 | - | sampling | sampling->sync CDC FIFO write errors (overflows) |
| `0x21C` | `CDC_FULL_CYCLES` | ro | 32 | - | sampling | sampling cycles with the sampling->sync CDC FIFO full (extension) |
| `0x220` | `I_MIN` | ro | 32 | - | sampling | Minimum I in the window (sign-extended 12-bit) |
| `0x224` | `I_MAX` | ro | 32 | - | sampling | Maximum I in the window (sign-extended 12-bit) |
| `0x228` | `Q_MIN` | ro | 32 | - | sampling | Minimum Q in the window (sign-extended 12-bit) |
| `0x22C` | `Q_MAX` | ro | 32 | - | sampling | Maximum Q in the window (sign-extended 12-bit) |
| `0x230` | `I_SUM_LO` | ro | 32 | - | sampling | Sum of I in the window (sign-extended 48-bit) (bits 31:0) |
| `0x234` | `I_SUM_HI` | ro | 32 | - | sampling | Sum of I in the window (sign-extended 48-bit) (bits 63:32) |
| `0x238` | `Q_SUM_LO` | ro | 32 | - | sampling | Sum of Q in the window (sign-extended 48-bit) (bits 31:0) |
| `0x23C` | `Q_SUM_HI` | ro | 32 | - | sampling | Sum of Q in the window (sign-extended 48-bit) (bits 63:32) |
| `0x240` | `I_SUMSQ_LO` | ro | 32 | - | sampling | Sum of I^2 in the window (bits 31:0) |
| `0x244` | `I_SUMSQ_HI` | ro | 32 | - | sampling | Sum of I^2 in the window (bits 63:32) |
| `0x248` | `Q_SUMSQ_LO` | ro | 32 | - | sampling | Sum of Q^2 in the window (bits 31:0) |
| `0x24C` | `Q_SUMSQ_HI` | ro | 32 | - | sampling | Sum of Q^2 in the window (bits 63:32) |
| `0x250` | `CLIP_COUNT` | ro | 32 | - | sampling | Samples at full scale (-2048 or 2047) on I or Q |
| `0x254` | `I_OR_MASK` | ro | 12 | - | sampling | OR of all I samples in the window (stuck-at-0 bits) |
| `0x258` | `I_AND_MASK` | ro | 12 | - | sampling | AND of all I samples in the window (stuck-at-1 bits) |
| `0x25C` | `Q_OR_MASK` | ro | 12 | - | sampling | OR of all Q samples in the window (stuck-at-0 bits) |
| `0x260` | `Q_AND_MASK` | ro | 12 | - | sampling | AND of all Q samples in the window (stuck-at-1 bits) |
| `0x264` | `WIN_SAMPLES_LO` | ro | 32 | - | sampling | Samples in the statistics window (bits 31:0) |
| `0x268` | `WIN_SAMPLES_HI` | ro | 32 | - | sampling | Samples in the statistics window (bits 63:32) |
| `0x26C` | `PRBS_CHECKED_LO` | ro | 32 | - | sampling | Samples checked by the PRBS checker (bits 31:0) |
| `0x270` | `PRBS_CHECKED_HI` | ro | 32 | - | sampling | Samples checked by the PRBS checker (bits 63:32) |
| `0x274` | `PRBS_ERRORS` | ro | 32 | - | sampling | PRBS sample errors |
| `0x278` | `PRBS_OOS_EVENTS` | ro | 32 | - | sampling | PRBS loss-of-sync events |
| `0x27C` | `PRBS_STATUS` | ro | 1 | - | sampling | PRBS status |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `INGEST_CTRL` | `stats_enable` | [0] | Enable statistics |
| `INGEST_CTRL` | `prbs_enable` | [1] | Enable the PRBS checker |
| `INGEST_CTRL` | `prbs_mode` | [2] | 0 = AD9361 BIST PRBS (pn0fn), 1 = PN9/PN11 |
| `INGEST_CTRL` | `honor_valid` | [3] | Honour valid_in (1) or take every sampling clock (0, production behaviour) in the monitor and the sampling->sync CDC |
| `INGEST_CTRL` | `clear_on_snap` | [4] | Clear the window statistics on every sampling snapshot |
| `INGEST_CMD` | `clear` | [0] | Clear all ingest counters |
| `PRBS_STATUS` | `in_sync` | [0] | PRBS checker in sync |

## Block `legacy` (0x400)

Production replica: IQPacker -> DmaStreamRingWrite at a fixed base (0x22000000, 16 x 1 MiB) with non-invasive counters (sync domain).

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x400` | `LEGACY_CTRL` | rw | 3 | `0x0` | - | Legacy ring control (quasi-static, sync domain) |
| `0x404` | `LEGACY_CMD` | wo | 1 | `0x0` | - | Legacy ring command |
| `0x408` | `LEGACY_RATE_INC` | rw | 32 | `0x20C49BA6` | - | Sample strobe rate = inc / 2^32 x 62.5 MHz (default 8 MSPS) |
| `0x40C` | `LEGACY_BASE` | ro | 32 | `0x22000000` | - | Ring base address (fixed) |
| `0x410` | `LEGACY_SIZE` | ro | 32 | `0x1000000` | - | Ring size in bytes (NUM_BUFFERS x sub-buffer size) |
| `0x414` | `LEGACY_NUM_BUFFERS` | ro | 32 | `0x10` | - | Number of sub-buffers |
| `0x418` | `LEGACY_LAST_BUFFER` | ro | 32 | - | sync | Last completed sub-buffer index |
| `0x41C` | `LEGACY_NEXT_ADDRESS` | ro | 32 | - | sync | Next write address of the ring DMA |
| `0x420` | `LEGACY_PACKER_OVF` | ro | 32 | - | sync | IQPacker overflow pulses while the DMA is enabled |
| `0x424` | `LEGACY_PACKER_OVF_DISABLED` | ro | 32 | - | sync | IQPacker overflow pulses while the DMA is disabled |
| `0x428` | `LEGACY_WORDS_IN` | ro | 32 | - | sync | 64-bit words produced by the packer |
| `0x42C` | `LEGACY_WORDS_ACCEPTED` | ro | 32 | - | sync | 64-bit words accepted by the ring DMA |
| `0x430` | `LEGACY_AW` | ro | 32 | - | sync | AW handshakes |
| `0x434` | `LEGACY_B` | ro | 32 | - | sync | B handshakes |
| `0x438` | `LEGACY_BRESP_ERR` | ro | 32 | - | sync | BRESP != OKAY count |
| `0x43C` | `LEGACY_SUBBUF_DONE` | ro | 32 | - | sync | Completed sub-buffers |
| `0x440` | `LEGACY_STALL_CYCLES` | ro | 32 | - | sync | Cycles with packer data waiting on the DMA |
| `0x444` | `LEGACY_MAX_STALL` | ro | 32 | - | sync | Longest stall in sync cycles |
| `0x448` | `LEGACY_MAX_OUTSTANDING` | ro | 32 | - | sync | Maximum un-acknowledged bursts seen |
| `0x44C` | `LEGACY_LAT_MAX` | ro | 32 | - | sync | Maximum AW-to-B write latency in sync cycles |
| `0x450` | `LEGACY_HIST_SEL` | rw | 4 | `0x0` | - | Write-latency histogram bin selected for LEGACY_HIST_VAL (log2 bins) |
| `0x454` | `LEGACY_HIST_VAL` | ro | 32 | - | sync | Count of the histogram bin selected by LEGACY_HIST_SEL (set HIST_SEL, then SNAP_REQ) |
| `0x458` | `LEGACY_GEN_LO` | ro | 32 | - | sync | Samples produced by the source generator (bits 31:0) |
| `0x45C` | `LEGACY_GEN_HI` | ro | 32 | - | sync | Samples produced by the source generator (bits 63:32) |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `LEGACY_CTRL` | `dma_enable` | [0] | DmaStreamRingWrite enable |
| `LEGACY_CTRL` | `src` | [2:1] | Source: 0 off, 1 sample ramp, 2 live rxiq |
| `LEGACY_CMD` | `clear` | [0] | Clear counters and generator |

## Block `ringv2` (0x500)

Ring buffer v2 (section 7), sync domain, m_axi_ringv2.

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x500` | `RINGV2_CTRL` | rw | 16 | `0x300` | - | Ring v2 control (sync domain) |
| `0x504` | `RINGV2_CMD` | wo | 3 | `0x0` | - | Ring v2 commands |
| `0x508` | `RINGV2_BASE` | rw | 32 | `0x20000000` | - | Ring base address (4 KiB aligned) |
| `0x50C` | `RINGV2_SIZE_BURSTS` | rw | 32 | `0x20000` | - | Ring size in 128-byte bursts (>= 2) |
| `0x510` | `RINGV2_SUBBUF_BURSTS` | rw | 16 | `0x2000` | - | Sub-buffer size in bursts (header period) |
| `0x514` | `RINGV2_IRQ_EVERY` | rw | 16 | `0x400` | - | IRQ every N committed bursts (with CTRL.irq_threshold) |
| `0x518` | `RINGV2_IRQ_TIMEOUT` | rw | 32 | `0x98968` | - | IRQ timeout in sync cycles (with CTRL.irq_timer) |
| `0x51C` | `RINGV2_FLUSH_TIMEOUT` | rw | 32 | `0xF424` | - | Idle sync cycles before a partial burst is padded and written |
| `0x520` | `RINGV2_MAX_OUTSTANDING` | rw | 4 | `0x8` | - | Maximum outstanding bursts (1-8) |
| `0x524` | `RINGV2_RATE_INC` | rw | 32 | `0x10624DD3` | - | Generator word rate = inc / 2^32 x 62.5 MHz (default 4 Mword/s = 32 MB/s) |
| `0x528` | `RINGV2_CONSUMER_BURSTS` | rw | 32 | `0x0` | - | Bursts consumed by the PS (protect mode); crossed coherently at any time |
| `0x52C` | `RINGV2_COMMITTED_BURSTS` | ro | 32 | - | - | Bursts committed (write responses received; burst k is at ring slot k mod SIZE_BURSTS, non-OKAY responses also count in RINGV2_BRESP_ERR). Monotonic, gray-code synchronized, read any time without a snapshot |
| `0x530` | `RINGV2_STATUS` | ro | 3 | - | - | Live status |
| `0x534` | `RINGV2_ISSUED_BURSTS` | ro | 32 | - | sync | Bursts issued (AW) |
| `0x538` | `RINGV2_WORDS_IN_LO` | ro | 32 | - | sync | Words offered by the source (bits 31:0) |
| `0x53C` | `RINGV2_WORDS_IN_HI` | ro | 32 | - | sync | Words offered by the source (bits 63:32) |
| `0x540` | `RINGV2_DROP_FULL` | ro | 32 | - | sync | Words dropped because the FIFO was full |
| `0x544` | `RINGV2_DROP_PROTECT` | ro | 32 | - | sync | Words dropped by protect mode |
| `0x548` | `RINGV2_PAD_WORDS` | ro | 32 | - | sync | Pad words written by flushes |
| `0x54C` | `RINGV2_FLUSHES` | ro | 32 | - | sync | Flushes |
| `0x550` | `RINGV2_BRESP_ERR` | ro | 32 | - | sync | BRESP != OKAY count |
| `0x554` | `RINGV2_FIFO_HWM` | ro | 16 | - | sync | FIFO high-water mark (words) |
| `0x558` | `RINGV2_MAX_OUTSTANDING_SEEN` | ro | 4 | - | sync | Maximum outstanding bursts seen |
| `0x55C` | `RINGV2_LAT_MAX` | ro | 32 | - | sync | Maximum AW-to-B latency in sync cycles |
| `0x560` | `RINGV2_HIST_SEL` | rw | 4 | `0x0` | - | Latency histogram bin selected for RINGV2_HIST_VAL |
| `0x564` | `RINGV2_HIST_VAL` | ro | 32 | - | sync | Count of the selected latency histogram bin |
| `0x568` | `RINGV2_EPOCH` | ro | 32 | - | sync | Soft-reset epoch counter |
| `0x56C` | `RINGV2_HEADERS` | ro | 32 | - | sync | Sub-buffer headers written |
| `0x570` | `RINGV2_GEN_LO` | ro | 32 | - | sync | Words produced by the source generator (bits 31:0) |
| `0x574` | `RINGV2_GEN_HI` | ro | 32 | - | sync | Words produced by the source generator (bits 63:32) |
| `0x578` | `RINGV2_GUARD_BLOCKED` | ro | 32 | - | sync | Bursts not issued because they fell outside [GUARD_LO, GUARD_HI) |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `RINGV2_CTRL` | `enable` | [0] | Enable (takes effect at burst boundaries; disabling drains) |
| `RINGV2_CTRL` | `protect` | [1] | Protect mode (never overwrite past CONSUMER_BURSTS) |
| `RINGV2_CTRL` | `header` | [2] | Sub-buffer header words |
| `RINGV2_CTRL` | `src` | [5:3] | Source: 0 off, 1 ramp64, 2 tagged, 3 prbs31, 4 live rxiq |
| `RINGV2_CTRL` | `irq_threshold` | [8] | IRQ every RINGV2_IRQ_EVERY committed bursts |
| `RINGV2_CTRL` | `irq_timer` | [9] | IRQ RINGV2_IRQ_TIMEOUT cycles after the last IRQ if bursts were committed |
| `RINGV2_CTRL` | `tag` | [15:12] | Tag nibble of the tagged pattern (extension) |
| `RINGV2_CMD` | `soft_reset` | [0] | Soft reset (accepted when idle; bumps EPOCH) |
| `RINGV2_CMD` | `flush` | [1] | Flush the partial burst |
| `RINGV2_CMD` | `clear` | [2] | Clear counters and generator |
| `RINGV2_STATUS` | `idle` | [0] | No burst in flight |
| `RINGV2_STATUS` | `enabled` | [1] | Producer enabled |
| `RINGV2_STATUS` | `fifo_empty` | [2] | FIFO empty |

## Block `mt0` (0x600)

AXI memory tester mt0 (section 8), mem domain (clk2x, 125 MHz), m_axi_mt0 -> HP0.

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x600` | `MT0_CTRL` | rw | 17 | `0x8802` | - | Memory tester configuration (quasi-static, mem domain) |
| `0x604` | `MT0_CMD` | wo | 3 | `0x0` | - | Memory tester commands |
| `0x608` | `MT0_BASE` | rw | 32 | `0x24000000` | - | Test region base address |
| `0x60C` | `MT0_SIZE` | rw | 32 | `0x2000000` | - | Test region size in bytes |
| `0x610` | `MT0_PASSES` | rw | 16 | `0x1` | - | Number of passes (0 = until abort) |
| `0x614` | `MT0_IDLE_CYCLES` | rw | 16 | `0x0` | - | Idle cycles between bursts (aggressor duty cycle) |
| `0x618` | `MT0_SEED` | rw | 32 | `0x1` | - | PRBS pattern seed |
| `0x61C` | `MT0_STATUS` | ro | 3 | - | - | Live status |
| `0x620` | `MT0_PASS_COUNT` | ro | 32 | - | mem | Completed passes |
| `0x624` | `MT0_BYTES_WR_LO` | ro | 32 | - | mem | Bytes written (bits 31:0) |
| `0x628` | `MT0_BYTES_WR_HI` | ro | 32 | - | mem | Bytes written (bits 63:32) |
| `0x62C` | `MT0_BYTES_RD_LO` | ro | 32 | - | mem | Bytes read (bits 31:0) |
| `0x630` | `MT0_BYTES_RD_HI` | ro | 32 | - | mem | Bytes read (bits 63:32) |
| `0x634` | `MT0_CYCLES_LO` | ro | 32 | - | mem | Active clk2x cycles (bits 31:0) |
| `0x638` | `MT0_CYCLES_HI` | ro | 32 | - | mem | Active clk2x cycles (bits 63:32) |
| `0x63C` | `MT0_ERR_COUNT` | ro | 32 | - | mem | Data errors |
| `0x640` | `MT0_FIRST_ERR_ADDR` | ro | 32 | - | mem | Address of the first data error |
| `0x644` | `MT0_FIRST_ERR_EXP_LO` | ro | 32 | - | mem | Expected data of the first error (bits 31:0) |
| `0x648` | `MT0_FIRST_ERR_EXP_HI` | ro | 32 | - | mem | Expected data of the first error (bits 63:32) |
| `0x64C` | `MT0_FIRST_ERR_ACT_LO` | ro | 32 | - | mem | Actual data of the first error (bits 31:0) |
| `0x650` | `MT0_FIRST_ERR_ACT_HI` | ro | 32 | - | mem | Actual data of the first error (bits 63:32) |
| `0x654` | `MT0_ERR_LANES_LO` | ro | 32 | - | mem | OR of the error bits (DQ lanes) (bits 31:0) |
| `0x658` | `MT0_ERR_LANES_HI` | ro | 32 | - | mem | OR of the error bits (DQ lanes) (bits 63:32) |
| `0x65C` | `MT0_BRESP_ERR` | ro | 32 | - | mem | BRESP != OKAY count |
| `0x660` | `MT0_RRESP_ERR` | ro | 32 | - | mem | RRESP != OKAY count |
| `0x664` | `MT0_WLAT_MAX` | ro | 32 | - | mem | Maximum write latency (AW to B) in clk2x cycles |
| `0x668` | `MT0_RLAT_MAX` | ro | 32 | - | mem | Maximum read latency (AR to RLAST) in clk2x cycles |
| `0x66C` | `MT0_HIST_SEL` | rw | 5 | `0x0` | - | Latency histogram selection |
| `0x670` | `MT0_HIST_VAL` | ro | 32 | - | mem | Count of the selected histogram bin |
| `0x674` | `MT0_GUARD_BLOCKED` | ro | 32 | - | mem | Bursts not issued because they fell outside [GUARD_LO, GUARD_HI) |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `MT0_CTRL` | `mode` | [2:0] | 0 write-only, 1 read-verify, 2 write-then-verify, 3 read-only, 4 byte-lane |
| `MT0_CTRL` | `pattern` | [6:3] | 0 address, 1 walking-1, 2 walking-0, 3 checkerboard, 4 PRBS, 5 all-0, 6 all-1, 7 toggle |
| `MT0_CTRL` | `burst_len` | [11:7] | Beats per burst: 1, 2, 4, 8 or 16 |
| `MT0_CTRL` | `max_outstanding` | [15:12] | Outstanding bursts (1-8) |
| `MT0_CTRL` | `stop_on_error` | [16] | Stop at the first error |
| `MT0_CMD` | `start` | [0] | Start |
| `MT0_CMD` | `abort` | [1] | Abort |
| `MT0_CMD` | `clear` | [2] | Clear counters |
| `MT0_STATUS` | `busy` | [0] | Running |
| `MT0_STATUS` | `done` | [1] | Finished |
| `MT0_STATUS` | `error` | [2] | Data or response error seen |
| `MT0_HIST_SEL` | `bin` | [3:0] | log2 latency bin |
| `MT0_HIST_SEL` | `read` | [4] | 0 = write latency, 1 = read latency |

## Block `mt1` (0x700)

AXI memory tester mt1 (section 8), mem domain (clk2x, 125 MHz), m_axi_mt1 -> HP3.

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x700` | `MT1_CTRL` | rw | 17 | `0x8802` | - | Memory tester configuration (quasi-static, mem domain) |
| `0x704` | `MT1_CMD` | wo | 3 | `0x0` | - | Memory tester commands |
| `0x708` | `MT1_BASE` | rw | 32 | `0x26000000` | - | Test region base address |
| `0x70C` | `MT1_SIZE` | rw | 32 | `0x2000000` | - | Test region size in bytes |
| `0x710` | `MT1_PASSES` | rw | 16 | `0x1` | - | Number of passes (0 = until abort) |
| `0x714` | `MT1_IDLE_CYCLES` | rw | 16 | `0x0` | - | Idle cycles between bursts (aggressor duty cycle) |
| `0x718` | `MT1_SEED` | rw | 32 | `0x1` | - | PRBS pattern seed |
| `0x71C` | `MT1_STATUS` | ro | 3 | - | - | Live status |
| `0x720` | `MT1_PASS_COUNT` | ro | 32 | - | mem | Completed passes |
| `0x724` | `MT1_BYTES_WR_LO` | ro | 32 | - | mem | Bytes written (bits 31:0) |
| `0x728` | `MT1_BYTES_WR_HI` | ro | 32 | - | mem | Bytes written (bits 63:32) |
| `0x72C` | `MT1_BYTES_RD_LO` | ro | 32 | - | mem | Bytes read (bits 31:0) |
| `0x730` | `MT1_BYTES_RD_HI` | ro | 32 | - | mem | Bytes read (bits 63:32) |
| `0x734` | `MT1_CYCLES_LO` | ro | 32 | - | mem | Active clk2x cycles (bits 31:0) |
| `0x738` | `MT1_CYCLES_HI` | ro | 32 | - | mem | Active clk2x cycles (bits 63:32) |
| `0x73C` | `MT1_ERR_COUNT` | ro | 32 | - | mem | Data errors |
| `0x740` | `MT1_FIRST_ERR_ADDR` | ro | 32 | - | mem | Address of the first data error |
| `0x744` | `MT1_FIRST_ERR_EXP_LO` | ro | 32 | - | mem | Expected data of the first error (bits 31:0) |
| `0x748` | `MT1_FIRST_ERR_EXP_HI` | ro | 32 | - | mem | Expected data of the first error (bits 63:32) |
| `0x74C` | `MT1_FIRST_ERR_ACT_LO` | ro | 32 | - | mem | Actual data of the first error (bits 31:0) |
| `0x750` | `MT1_FIRST_ERR_ACT_HI` | ro | 32 | - | mem | Actual data of the first error (bits 63:32) |
| `0x754` | `MT1_ERR_LANES_LO` | ro | 32 | - | mem | OR of the error bits (DQ lanes) (bits 31:0) |
| `0x758` | `MT1_ERR_LANES_HI` | ro | 32 | - | mem | OR of the error bits (DQ lanes) (bits 63:32) |
| `0x75C` | `MT1_BRESP_ERR` | ro | 32 | - | mem | BRESP != OKAY count |
| `0x760` | `MT1_RRESP_ERR` | ro | 32 | - | mem | RRESP != OKAY count |
| `0x764` | `MT1_WLAT_MAX` | ro | 32 | - | mem | Maximum write latency (AW to B) in clk2x cycles |
| `0x768` | `MT1_RLAT_MAX` | ro | 32 | - | mem | Maximum read latency (AR to RLAST) in clk2x cycles |
| `0x76C` | `MT1_HIST_SEL` | rw | 5 | `0x0` | - | Latency histogram selection |
| `0x770` | `MT1_HIST_VAL` | ro | 32 | - | mem | Count of the selected histogram bin |
| `0x774` | `MT1_GUARD_BLOCKED` | ro | 32 | - | mem | Bursts not issued because they fell outside [GUARD_LO, GUARD_HI) |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `MT1_CTRL` | `mode` | [2:0] | 0 write-only, 1 read-verify, 2 write-then-verify, 3 read-only, 4 byte-lane |
| `MT1_CTRL` | `pattern` | [6:3] | 0 address, 1 walking-1, 2 walking-0, 3 checkerboard, 4 PRBS, 5 all-0, 6 all-1, 7 toggle |
| `MT1_CTRL` | `burst_len` | [11:7] | Beats per burst: 1, 2, 4, 8 or 16 |
| `MT1_CTRL` | `max_outstanding` | [15:12] | Outstanding bursts (1-8) |
| `MT1_CTRL` | `stop_on_error` | [16] | Stop at the first error |
| `MT1_CMD` | `start` | [0] | Start |
| `MT1_CMD` | `abort` | [1] | Abort |
| `MT1_CMD` | `clear` | [2] | Clear counters |
| `MT1_STATUS` | `busy` | [0] | Running |
| `MT1_STATUS` | `done` | [1] | Finished |
| `MT1_STATUS` | `error` | [2] | Data or response error seen |
| `MT1_HIST_SEL` | `bin` | [3:0] | log2 latency bin |
| `MT1_HIST_SEL` | `read` | [4] | 0 = write latency, 1 = read latency |

## Block `evt` (0x800)

AD9361 CTRL_OUT event recorder. Records are 36 bits: bit35 heartbeat, bits[34:27] CTRL_OUT value, bits[26:0] timestamp (low 27 bits of TS, 62.5 MHz cycles).

| Offset | Register | Access | Width | Reset | Snapshot | Description |
|---|---|---|---|---|---|---|
| `0x800` | `EVT_CTRL` | rw | 16 | `0xFF00` | - | Event recorder control (quasi-static, sync domain) |
| `0x804` | `EVT_LEVEL` | ro | 32 | - | - | Event FIFO level |
| `0x808` | `EVT_DATA_LO` | ro | 32 | - | - | FIFO head, bits 31:0 of the 36-bit record (read before EVT_POP) |
| `0x80C` | `EVT_DATA_HI` | ro | 4 | - | - | FIFO head, bits 35:32 of the 36-bit record |
| `0x810` | `EVT_POP` | wo | 1 | `0x0` | - | Pop the FIFO head |
| `0x814` | `EVT_OVERFLOWS` | ro | 32 | - | sync | Events lost because the FIFO was full |
| `0x818` | `EVT_CURRENT` | ro | 8 | - | - | Live CTRL_OUT value (synchronized) |
| `0x81C` | `EVT_CMD` | wo | 1 | `0x0` | - | Event recorder command (extension) |

Fields:

| Register | Field | Bits | Description |
|---|---|---|---|
| `EVT_CTRL` | `enable` | [0] | Record CTRL_OUT transitions |
| `EVT_CTRL` | `mask` | [15:8] | CTRL_OUT bits that generate events |
| `EVT_DATA_LO` | `timestamp` | [26:0] | Timestamp in 62.5 MHz cycles (low 27 bits) |
| `EVT_DATA_LO` | `value_lo` | [31:27] | CTRL_OUT value bits 4:0 |
| `EVT_DATA_HI` | `value_hi` | [2:0] | CTRL_OUT value bits 7:5 |
| `EVT_DATA_HI` | `heartbeat` | [3] | Heartbeat record (every 2^26 cycles) |
| `EVT_POP` | `pop` | [0] | Pop |
| `EVT_CMD` | `clear` | [0] | Clear EVT_OVERFLOWS |
