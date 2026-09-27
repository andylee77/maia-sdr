# fishball7020_hwval -- hardware-validation Vivado project

Validation bitstream for the Fishball Z7020 bench (`fbench` Tier 1). The design
contract is `doc/HW_VALIDATION_SUITE.md` section 6 (block design, DDR windows,
register rules) and section 11 (build and delivery). The gateware is
`maia-hdl/hwval_hdl/` (`hwval_core`, Amaranth top module `top`), packaged by
`maia-hdl/ip/hwval-core/` as
`fishball-hwval:hwval_core_default:hwval_core:0.1.0`.

## Build

```bash
./build_fpga_hwval_pretty.sh        # Git Bash, wraps build_fpga.bat --hwval
```

or `build_fpga.bat --hwval` from `cmd`. Output:
`fishball_hwval.sdk/system_top.xsa`, copied to
`tezuka_fw/board/tezuka/fishball7020/bitstream/hwval/system_top.xsa`.
The register map that matches the bitstream is published to
`bench/share/hwval_regs.json` and `doc/hwval_register_map.md`.

Timing failure is a hard error: `system_top_bad_timing.xsa` is never promoted
for this project (it is for P25).

## Deltas vs `fishball7020_p25`

Everything not listed here is byte-for-byte the P25 project (same pluto base,
same maia_sdr removal surgery, same IIO DMA on HP2, same pins and IO
standards, same implementation strategy).

| Area | P25 | hwval |
|---|---|---|
| Core | `p25_core` | `hwval_core` (its `rst` output, if exported, stays unconnected as in P25) |
| BD flags | `fishball LVDS_ENABLE maia_iio with_tx_fir with_rx_fir_maia` (+ unused `p25_iio`) | same, without the unused `p25_iio` |
| `axi_ad9361` DDS | `DAC_DDS_DISABLE=1` (pluto base) | `DAC_DDS_DISABLE=0` (TX stimulus) |
| HP1 (@ `clk_out1`, 62.5 MHz) | seven P25 ring masters | `m_axi_ringv2`, `m_axi_legacy` |
| HP0 / HP3 | unused | `m_axi_mt0` -> HP0, `m_axi_mt1` -> HP3, 64-bit, @ `clk_out2` (125 MHz) |
| AXI-Lite | `0x7C46_0000` | `0x7C46_0000` (4 KiB) |
| IRQ | `sys_concat_intc/In11` (SPI 55) | same |
| Core clocks | `s_axi_lite_clk`, `clk`, `clk2x_clk`, `clk3x_clk`, `sampling_clk` | same plus `lclk_clk` (`axi_ad9361/l_clk`), `fclk1_clk` (`sys_200m_clk`), `y1_clk` |
| Core data | `re_in`, `im_in` | same plus `valid_in` (`util_ad9361_adc_fifo/dout_valid_0`), `ctrl_out[7:0]`, `ad_clkout` |
| New top-level pins | -- | `y1_clk` N18 LVCMOS25 (50 MHz oscillator), `ad_clkout` R16 LVCMOS25 (AD9361 CLK_OUT, data) |
| `system_top.v` | -- | `gpio_i[7:0]` (the `gpio_status` / CTRL_OUT pins) teed to BD port `ctrl_out`; PS GPIO still reads them |
| `rx_clk` period | 4 ns | 8.138 ns (122.88 MHz, real 1R1T LVDS maximum) |
| `axi_ad9361/inst/i_tx/*` false path | present | removed (TX path is timing-checked) |
| `manual_decim` false path | present (stale) | removed |
| `p25_core ... field_sdr_reset_reg` false path | present | replaced by `-to *hwval_core*/fifo/fifo/fifo18e1/RST` (ingest and evt CDC FIFOs) |
| New waivers | -- | `-from *_snapshadow* -to clk_fpga_0` (snapshot shadows and census counters, read only in the AXI-Lite domain; scoped so the census counters' own increment paths stay timed), `-from *_cdchold_reg*` (DomainCrossing/ConfigSync hold registers, `hwval_hdl/cdc_util.py`), `-to amaranth.vivado.false_path == "TRUE"` cells (census `*_count_snapstage`, rule from `maia_sdr.xdc`), `-from` ports `ad_clkout` and `gpio_status[*]` (async inputs), `y1_clk` 20 ns clock |
| Bad-timing XSA | promoted by `build_fpga.bat` | hard failure |

Kept unchanged on purpose: the global `ASYNC_REG` false path, the maia
`cdc_request/response_data_dest_reg` waivers (only a "no valid object"
critical warning if hwval does not use RegisterCDC), the `sys_rstgen` reset
fan-out waiver and the four axi_ad9361 GPIO/xfer-control waivers.

## Device tree / SD image

The matching device tree is `tezuka_fw/board/tezuka/fishball7020/dts/fishball-hwval.dts(i)`
(UIO `hwval-core@7c460000`, rxbuffers `hwval-ringv2` / `hwval-legacy`,
reserved `hwval-memtest`). The Tezuka P25 build places the hwval `BOOT.bin` +
`devicetree.dtb` under `sdimg/bench/images/hwval/` (see Tezuka
`doc/changes/005_fishball_hwval_dual_image.md`).
