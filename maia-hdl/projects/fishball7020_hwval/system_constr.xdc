# constraints
#
# Fishball hwval (hardware-validation) project constraints.
# Copy of fishball7020_p25/system_constr.xdc with these deltas
# (doc/HW_VALIDATION_SUITE.md section 6.1):
#   - rx_clk period 8.138 ns (real 1R1T LVDS maximum, 122.88 MHz
#     DATA_CLK) instead of 4 ns
#   - new pins: y1_clk (N18, 50 MHz oscillator, 20 ns clock) and
#     ad_clkout (R16, AD9361 CLK_OUT sampled as data, not a clock)
#   - the blanket axi_ad9361/inst/i_tx/* false path is REMOVED: the TX
#     path drives DDS stimulus in hwval and must be timing-checked
#   - the stale manual_decim false path is removed (no such cell)
#   - the p25_core sdr_reset waiver becomes a FIFO18E1 RST waiver
#     scoped to hwval_core
#   - new *_snapshadow* false path for quasi-static snapshot shadows
#   - new *_cdchold_reg* false path for DomainCrossing/ConfigSync holds
#   - new amaranth.vivado.false_path attribute rule (as maia_sdr.xdc)
# IO standards are deliberately identical to P25 (LVDS_25 / LVCMOS25,
# see finding F9: banks 34/35 VCCO is being measured, not changed here).
#
# ad9361 (SWAP == 0x1)

set_property  -dict {PACKAGE_PIN  U18  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_clk_in_p]        
set_property  -dict {PACKAGE_PIN  U19  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_clk_in_n]        
set_property  -dict {PACKAGE_PIN  Y16  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_frame_in_p]      
set_property  -dict {PACKAGE_PIN  Y17  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_frame_in_n]      
set_property  -dict {PACKAGE_PIN  Y18  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_p[0]]   
set_property  -dict {PACKAGE_PIN  Y19  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_n[0]]   
set_property  -dict {PACKAGE_PIN  T16  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_p[1]]   
set_property  -dict {PACKAGE_PIN  U17  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_n[1]]   
set_property  -dict {PACKAGE_PIN  V20  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_p[2]]   
set_property  -dict {PACKAGE_PIN  W20  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_n[2]]   
set_property  -dict {PACKAGE_PIN  T17  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_p[3]]   
set_property  -dict {PACKAGE_PIN  R18  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_n[3]]   
set_property  -dict {PACKAGE_PIN  T20  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_p[4]]   
set_property  -dict {PACKAGE_PIN  U20  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_n[4]]   
set_property  -dict {PACKAGE_PIN  W18  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_p[5]]   
set_property  -dict {PACKAGE_PIN  W19  IOSTANDARD LVDS_25 DIFF_TERM TRUE} [get_ports rx_data_in_n[5]]   
set_property  -dict {PACKAGE_PIN  U14  IOSTANDARD LVDS_25} [get_ports tx_clk_out_p]                     
set_property  -dict {PACKAGE_PIN  U15  IOSTANDARD LVDS_25} [get_ports tx_clk_out_n]                     
set_property  -dict {PACKAGE_PIN  V16  IOSTANDARD LVDS_25} [get_ports tx_frame_out_p]                   
set_property  -dict {PACKAGE_PIN  W16  IOSTANDARD LVDS_25} [get_ports tx_frame_out_n]                   
set_property  -dict {PACKAGE_PIN  V15  IOSTANDARD LVDS_25} [get_ports tx_data_out_p[0]]                 
set_property  -dict {PACKAGE_PIN  W15  IOSTANDARD LVDS_25} [get_ports tx_data_out_n[0]]                 
set_property  -dict {PACKAGE_PIN  V12  IOSTANDARD LVDS_25} [get_ports tx_data_out_p[1]]                 
set_property  -dict {PACKAGE_PIN  W13  IOSTANDARD LVDS_25} [get_ports tx_data_out_n[1]]                 
set_property  -dict {PACKAGE_PIN  W14  IOSTANDARD LVDS_25} [get_ports tx_data_out_p[2]]                 
set_property  -dict {PACKAGE_PIN  Y14  IOSTANDARD LVDS_25} [get_ports tx_data_out_n[2]]                 
set_property  -dict {PACKAGE_PIN  T12  IOSTANDARD LVDS_25} [get_ports tx_data_out_p[3]]                 
set_property  -dict {PACKAGE_PIN  U12  IOSTANDARD LVDS_25} [get_ports tx_data_out_n[3]]                 
set_property  -dict {PACKAGE_PIN  T11  IOSTANDARD LVDS_25} [get_ports tx_data_out_p[4]]                 
set_property  -dict {PACKAGE_PIN  T10  IOSTANDARD LVDS_25} [get_ports tx_data_out_n[4]]                 
set_property  -dict {PACKAGE_PIN  U13  IOSTANDARD LVDS_25} [get_ports tx_data_out_p[5]]                 
set_property  -dict {PACKAGE_PIN  V13  IOSTANDARD LVDS_25} [get_ports tx_data_out_n[5]]                  

set_property  -dict {PACKAGE_PIN  L20 IOSTANDARD LVCMOS25} [get_ports gpio_status[0]]                  
set_property  -dict {PACKAGE_PIN  L19 IOSTANDARD LVCMOS25} [get_ports gpio_status[1]]                  
set_property  -dict {PACKAGE_PIN  K19 IOSTANDARD LVCMOS25} [get_ports gpio_status[2]]                  
set_property  -dict {PACKAGE_PIN  T14 IOSTANDARD LVCMOS25} [get_ports gpio_status[3]]                  
set_property  -dict {PACKAGE_PIN  P15 IOSTANDARD LVCMOS25} [get_ports gpio_status[4]]                  
set_property  -dict {PACKAGE_PIN  M20 IOSTANDARD LVCMOS25} [get_ports gpio_status[5]]                  
set_property  -dict {PACKAGE_PIN  M19 IOSTANDARD LVCMOS25} [get_ports gpio_status[6]]                  
set_property  -dict {PACKAGE_PIN  N20 IOSTANDARD LVCMOS25} [get_ports gpio_status[7]]
                 
set_property  -dict {PACKAGE_PIN  J19 IOSTANDARD LVCMOS25} [get_ports gpio_ctl[0]]                     
set_property  -dict {PACKAGE_PIN  K14 IOSTANDARD LVCMOS25} [get_ports gpio_ctl[1]]                     
#set_property  -dict {PACKAGE_PIN  L17 IOSTANDARD LVCMOS25} [get_ports gpio_ctl[2]]                     
set_property  -dict {PACKAGE_PIN  R14 IOSTANDARD LVCMOS25} [get_ports gpio_ctl[2]]                     
set_property  -dict {PACKAGE_PIN  J20 IOSTANDARD LVCMOS25} [get_ports gpio_ctl[3]] 
set_property  -dict {PACKAGE_PIN  P20  IOSTANDARD LVCMOS25} [get_ports gpio_en_agc]
set_property  -dict {PACKAGE_PIN  R19  IOSTANDARD LVCMOS25} [get_ports gpio_resetb]

set_property  -dict {PACKAGE_PIN  T15  IOSTANDARD LVCMOS25} [get_ports enable]
set_property  -dict {PACKAGE_PIN  P18  IOSTANDARD LVCMOS25} [get_ports txnrx]

set_property  -dict {PACKAGE_PIN  M14  IOSTANDARD LVCMOS25 PULLTYPE PULLUP} [get_ports iic_scl]
set_property  -dict {PACKAGE_PIN  M15  IOSTANDARD LVCMOS25 PULLTYPE PULLUP} [get_ports iic_sda]

set_property  -dict {PACKAGE_PIN  R17  IOSTANDARD LVCMOS25  PULLTYPE PULLUP} [get_ports spi_csn]
set_property  -dict {PACKAGE_PIN  V18  IOSTANDARD LVCMOS25} [get_ports spi_clk]
set_property  -dict {PACKAGE_PIN  P16  IOSTANDARD LVCMOS25} [get_ports spi_mosi]
set_property  -dict {PACKAGE_PIN  V17  IOSTANDARD LVCMOS25} [get_ports spi_miso]

set_property  -dict {PACKAGE_PIN  L14  IOSTANDARD LVCMOS25} [get_ports pl_spi_clk_o]
set_property  -dict {PACKAGE_PIN  N15  IOSTANDARD LVCMOS25} [get_ports pl_spi_miso]
set_property  -dict {PACKAGE_PIN  N16  IOSTANDARD LVCMOS25} [get_ports pl_spi_mosi]

# hwval: 50 MHz board oscillator (IO_L13P_T2_MRCC_34, clock-capable) and
# AD9361 CLK_OUT (IO_L19P_T3_34). Bank 34 stays one IO standard family
# (LVCMOS25 next to LVDS_25), see F9.
set_property  -dict {PACKAGE_PIN  N18  IOSTANDARD LVCMOS25} [get_ports y1_clk]
set_property  -dict {PACKAGE_PIN  R16  IOSTANDARD LVCMOS25} [get_ports ad_clkout]


#create_clock -period 8.000 -name rx_clk [get_ports rx_clk_in_p]

# probably gone in 2016.4

create_clock -name clk_fpga_0 -period 10 [get_pins "i_system_wrapper/system_i/sys_ps7/inst/PS7_i/FCLKCLK[0]"]
create_clock -name clk_fpga_1 -period  5 [get_pins "i_system_wrapper/system_i/sys_ps7/inst/PS7_i/FCLKCLK[1]"]

create_clock -name spi0_clk      -period 40   [get_pins -hier */EMIOSPI0SCLKO]

set_input_jitter clk_fpga_0 0.3
set_input_jitter clk_fpga_1 0.15

#set_false_path -to [get_pins i_system_wrapper/system_i/lvds_clck2/inst/clk_out_reg/CLR]

set_false_path -from [get_pins {i_system_wrapper/system_i/axi_ad9361/inst/i_rx/i_up_adc_common/up_adc_gpio_out_int_reg[0]/C}]
set_false_path -from [get_pins {i_system_wrapper/system_i/axi_ad9361/inst/i_tx/i_up_dac_common/up_dac_gpio_out_int_reg[0]/C}]

set_false_path -from [get_pins {i_system_wrapper/system_i/axi_ad9361/inst/i_rx/i_up_adc_common/i_xfer_cntrl/d_data_cntrl_int_reg[0]/C}]
set_false_path -from [get_pins {i_system_wrapper/system_i/axi_ad9361/inst/i_tx/i_up_dac_common/i_xfer_cntrl/d_data_cntrl_int_reg[0]/C}]

# clocks

# 8.138 ns = 122.88 MHz DATA_CLK, the real 1R1T LVDS maximum (P25 uses
# an over-tight 4 ns here and then waives the TX path; hwval does not).
create_clock -name rx_clk       -period  8.138 [get_ports rx_clk_in_p]

# 50 MHz board oscillator Y1 (clock census reference).
create_clock -name y1_clk       -period 20.000 [get_ports y1_clk]

# Asynchronous status inputs, synchronised inside hwval_core:
# AD9361 CLK_OUT (sampled as data in clk3x) and CTRL_OUT (gpio_status,
# also read by the PS GPIO). No input-delay relationship exists.
set_false_path -from [get_ports ad_clkout]
set_false_path -from [get_ports {gpio_status[*]}]

# ── CDC synchronizer false paths ───────────────────────────────────────
#
# Maia HDL's RegisterCDC and Amaranth's FFSynchronizer /
# PulseSynchronizer emit 2-flop synchronizer chains with the
# destination flop marked ASYNC_REG=TRUE. The ASYNC_REG attribute
# correctly suppresses hold-time analysis (Vivado's default rule)
# but does NOT automatically waive setup-time analysis, so the
# inter-clock setup paths between the source and destination flops
# of every synchronizer chain are reported as failing by several ns.
#
# For the Fishball P25 core, this hits ~293 endpoints between
# clk_out1_system_maia_sdr_clk_0 (PL sync, 62.5 MHz) and clk_fpga_0
# (PS AXI, 100 MHz): the RegisterCDC request/response data lanes
# for every register bank (sdr_registers_cdc, demod_registers_cdc,
# iq_registers_cdc, lsm_registers_cdc, traffic_registers_cdc,
# traffic_lsm_registers_cdc) plus the 5 DMA interrupt
# PulseSynchronizers added in commit <TBD> for the v2 P25DDC
# fork.
#
# These synchronizer chains are correct by construction (MTBF ~ 1
# failure per universe-age at 2 sync stages and typical
# frequencies) and should not be analysed against the default
# inter-clock setup constraint. The canonical Xilinx waiver for
# ASYNC_REG-marked synchronizer chains is:
set_false_path -to [get_cells -hierarchical -filter {ASYNC_REG == TRUE}]

# hwval: Amaranth synchronizer/snapshot cells that carry the
# amaranth.vivado.false_path attribute (e.g. the census
# *_count_snapstage registers). Same rule as maia_sdr.xdc, applied at
# project level because hwval_core.xdc is comment-only (as p25_core.xdc).
set_false_path -to [get_cells -hierarchical -filter {amaranth.vivado.false_path == "TRUE"}]

# Maia RegisterCDC uses a pulse-gated data CDC pattern: a
# PulseSynchronizer handshake carries request/response edges
# (which DO get ASYNC_REG=TRUE from amaranth.lib.cdc and are
# caught by the filter above), while the actual register data
# lanes travel across a pair of bare Signals
# (cdc_request_data_src_reg / cdc_request_data_dest_reg and the
# response-direction equivalents in maia_hdl/cdc.py:102-140). The
# data-lane flops do NOT get ASYNC_REG because they are not
# inside a FFSynchronizer.
#
# The protocol is still correct -- the source flop is stable for
# many cycles before the pulse handshake permits the destination
# flop to sample it -- but Vivado's default analysis treats the
# cross-clock data-lane path as a real 2 ns inter-clock setup
# constraint and reports ~272 failing endpoints across all six
# P25 RegisterCDC instances (sdr, demod, iq, lsm, traffic,
# traffic_lsm). Waive those paths explicitly by cell name.
# hwval: kept in case hwval_core reuses maia RegisterCDC; if nothing
# matches, Vivado only reports a "No valid object(s)" critical warning.
set_false_path -to [get_cells -hierarchical -filter {NAME =~ *cdc_request_data_dest_reg*}]
set_false_path -to [get_cells -hierarchical -filter {NAME =~ *cdc_response_data_dest_reg*}]

# ── hwval snapshot shadows ─────────────────────────────────────────────
#
# Status from the sync / mem / sampling domains is read through
# snapshot shadow registers (doc/HW_VALIDATION_SUITE.md section 6.3):
# the source domain captures into *_snapshadow* on SNAP_REQ and only
# then raises the (ASYNC_REG-synchronised) SNAP_ACK; the AXI-Lite side
# reads the shadows after ACK. The shadows are quasi-static when read.
# The clock-census counters are also named *_count_snapshadow; they are
# sampled every AXI-Lite cycle into *_count_snapstage (which carries
# amaranth.vivado.false_path, see above).
#
# Scoped with -to clk_fpga_0: every consumer of a shadow is in the
# AXI-Lite domain (register read mux, census snapstage), so this waives
# exactly the crossings. A bare `-from *_snapshadow*` would also un-time
# the census counters' own increment loops (33-bit at up to 200 MHz),
# which must stay timing-checked. If Vivado reports failing paths that
# start at *_snapshadow* and end in another clock, fall back to the
# unscoped form: set_false_path -from [get_cells -hier -filter {NAME =~ *_snapshadow*}]
set_false_path -from [get_cells -hier -filter {NAME =~ *_snapshadow*}] -to [get_clocks clk_fpga_0]

# ── hwval configuration / command hold registers ───────────────────────
#
# The other direction (hwval_hdl/cdc_util.py DomainCrossing / ConfigSync):
# AXI-Lite writes land in *_cdchold registers that are held stable
# while a toggle handshake (FFSynchronizer, ASYNC_REG) tells the
# destination domain (sync / clk2x / sampling) to sample them. Without
# this waiver the 100 MHz -> 62.5 MHz crossings are timed as a 2 ns
# synchronous relationship and fail.
set_false_path -from [get_cells -hier -filter {NAME =~ *_cdchold_reg*}]

# ── ADI AD9361 TX rate counter ─────────────────────────────────────────
#
# hwval: the P25 blanket `set_false_path -to *axi_ad9361/inst/i_tx/*`
# is intentionally NOT carried over. hwval enables the axi_ad9361 DDS
# (DAC_DDS_DISABLE=0) for TX stimulus, so the TX path must be timing-
# checked. With rx_clk at its real 8.138 ns period the dac_rate_cnt
# chain that needed the waiver at 4 ns has ample margin.

# ── PS reset generator fan-out ─────────────────────────────────────────
#
# sys_rstgen's synchronous reset output fans out to 143+ loads in
# the ADI AXI_AD9361 DAC DMA regmap, with a single LUT1 logic
# level but ~5 ns of routing. Reset nets don't need per-cycle
# timing (they are held stable for many cycles), so waive the
# reset-net setup paths into the axi_ad9361_*_dma regmaps.
set_false_path -through [get_pins {i_system_wrapper/system_i/sys_rstgen/U0/ACTIVE_LOW_PR_OUT_DFF[0].FDRE_PER_N/Q}]

# ── hwval software reset into FIFO18E1 primitives ──────────────────────
#
# P25 waives `p25_core/inst/control_registers/control/field_sdr_reset_reg`
# -> RxIQ CDC FIFO18E1 RST (a PS-driven software reset, held for
# milliseconds, analysed as ~-5 ns recovery violations against every
# clock the FIFO sees). The hwval equivalent is CORE_RESET, whose
# register name is generated; waive at the destination instead: the
# async RST pins of the hwval_core CDC FIFO18E1 primitives
# (<ingest_cdc>/fifo/fifo/fifo18e1 and <evt>/fifo/fifo/fifo18e1), the
# same rule maia_sdr.xdc uses for rxiq_cdc/fifo/fifo18e1/RST.
set_false_path -to [get_pins -hierarchical -filter {NAME =~ *hwval_core*/fifo/fifo/fifo18e1/RST}]
