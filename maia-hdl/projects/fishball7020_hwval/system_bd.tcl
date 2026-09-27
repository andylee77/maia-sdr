# Fishball hwval (hardware-validation) - Block Design (in-tree build)
# Contract: doc/HW_VALIDATION_SUITE.md section 6.1. Deltas vs P25: README.md.
#
# Sources the pluto base WITH maia_iio exactly like fishball7020_p25
# (keeps axi_dmac RX/TX on HP2, util_cpack2/upack2, 8-bit mux chain,
# FIR filters), removes maia_sdr with the identical surgery, then adds
# hwval_core instead of p25_core:
#   - axi_ad9361 DDS enabled (TX stimulus)
#   - m_axi_ringv2 + m_axi_legacy -> HP1 interconnect @ clk_out1 (62.5 MHz)
#   - m_axi_mt0 -> S_AXI_HP0, m_axi_mt1 -> S_AXI_HP3 @ clk_out2 (125 MHz)
#   - AXI-Lite @ 0x7C460000, IRQ sys_concat_intc/In11 (same as P25)
#   - new BD ports: y1_clk, ad_clkout, ctrl_out[7:0]

set LVDS_ENABLE "LVDS_ENABLE"
set fishball "fishball"
set with_tx_fir "with_tx_fir"
set with_rx_fir_maia "with_rx_fir_maia"
set maia_iio "maia_iio"

# Source the pluto base design (relative paths resolve in-tree)
source ../pluto/system_bd.tcl

# ── Remove maia_sdr (identical to fishball7020_p25/system_bd.tcl) ─────
delete_bd_objs [get_bd_cells maia_sdr]

# Clean up dangling nets from deleted maia_sdr. NOTE: this also removes
# the maia_sdr_clk_clk_out1/2/3 nets (they match maia_sdr_*); every
# clk_out consumer is reconnected below, S_AXI_HP1_ACLK by the HP1 proc.
foreach net [get_bd_nets -quiet -filter {NAME =~ maia_sdr_*}] {
    catch {delete_bd_objs $net}
}
# Clean up maia_sdr interface nets (spectrometer HP1, recorder HP2)
foreach intf_net [get_bd_intf_nets -quiet -filter {NAME =~ *maia_sdr*}] {
    catch {delete_bd_objs $intf_net}
}

# Delete sweeper_io (connected to maia_sdr/clk_fastlock_out which no longer exists)
catch {delete_bd_objs [get_bd_cells sweeper_io]}
catch {delete_bd_objs [get_bd_cells logic_orgpio]}

# Delete cells that directly referenced maia_sdr's decim outputs.
foreach cell {interclk_i interclk_q mux_decim_i mux_decim_q} {
    catch {delete_bd_objs [get_bd_cells $cell]}
}

# Tie the now-dangling mux inputs to valid sources (as P25).
ad_connect util_ad9361_adc_fifo/dout_data_0 rxcs12_cs8/sample_in1
ad_connect util_ad9361_adc_fifo/dout_data_1 rxcs12_cs8/sample_in2
ad_connect util_ad9361_adc_fifo/dout_valid_0 muxcs8/valid_in_1

# Reconnect gpio_o to PS7 (was through logic_orgpio OR gate)
catch {delete_bd_objs [get_bd_nets -quiet -filter {NAME =~ gpio_o*}]}
ad_connect sys_ps7/GPIO_O gpio_o

# ── axi_ad9361: enable the DDS tone generator (TX stimulus) ──────────
# pluto base sets DAC_DDS_DISABLE 1; everything else (LVDS, 1R1T,
# ADC_INIT_DELAY=30, TDD disabled) stays exactly as P25.
ad_ip_parameter axi_ad9361 CONFIG.DAC_DDS_DISABLE 0

# ── PS7: enable S_AXI_HP0 and S_AXI_HP3, 64-bit ───────────────────────
# (ad_mem_hp0/hp3_interconnect would also set PCW_USE_S_AXI_HPx; done
# explicitly so the data width can be checked before any connection.)
ad_ip_parameter sys_ps7 CONFIG.PCW_USE_S_AXI_HP0 {1}
ad_ip_parameter sys_ps7 CONFIG.PCW_USE_S_AXI_HP3 {1}
foreach hp {HP0 HP3} {
    set hp_width [get_property CONFIG.PCW_S_AXI_${hp}_DATA_WIDTH [get_bd_cells sys_ps7]]
    if {$hp_width ne "64"} {
        ad_ip_parameter sys_ps7 CONFIG.PCW_S_AXI_${hp}_DATA_WIDTH {64}
        set hp_width [get_property CONFIG.PCW_S_AXI_${hp}_DATA_WIDTH [get_bd_cells sys_ps7]]
    }
    puts "INFO: hwval: sys_ps7 S_AXI_${hp} data width = $hp_width"
    if {$hp_width ne "64"} {
        error "hwval: sys_ps7 S_AXI_${hp} data width is $hp_width, expected 64"
    }
}

# ── hwval_core IP (in-tree repo, relative to project dir) ─────────────
set hwval_ip_dir [file normalize ../../ip/hwval-core]
set_property ip_repo_paths [concat [get_property ip_repo_paths [current_project]] \
    [list $hwval_ip_dir]] [current_project]
update_ip_catalog

set hwval_config $::env(HWVAL_CONFIG)
set hwval_version $::env(IP_CORE_VERSION)
create_bd_cell -type ip \
    -vlnv fishball-hwval:hwval_core_${hwval_config}:hwval_core:${hwval_version} hwval_core

# ── Clocks ────────────────────────────────────────────────────────────
ad_connect sys_cpu_clk hwval_core/s_axi_lite_clk
ad_connect sys_cpu_reset hwval_core/s_axi_lite_rst
ad_connect maia_sdr_clk/clk_out1 hwval_core/clk
ad_connect maia_sdr_clk/clk_out2 hwval_core/clk2x_clk
ad_connect maia_sdr_clk/clk_out3 hwval_core/clk3x_clk
ad_connect hwval_core/sampling_clk util_ad9361_divclk/clk_out
ad_connect axi_ad9361/l_clk hwval_core/lclk_clk
ad_connect sys_200m_clk hwval_core/fclk1_clk

# New top-level inputs (pins in system_constr.xdc, wired in system_top.v)
create_bd_port -dir I -type clk -freq_hz 50000000 y1_clk
ad_connect y1_clk hwval_core/y1_clk
create_bd_port -dir I ad_clkout
ad_connect ad_clkout hwval_core/ad_clkout
create_bd_port -dir I -from 7 -to 0 ctrl_out
ad_connect ctrl_out hwval_core/ctrl_out

# ── IQ data (same slices as P25) + FIFO valid ─────────────────────────
ad_connect adc_i_slice/Dout hwval_core/re_in
ad_connect adc_q_slice/Dout hwval_core/im_in
ad_connect util_ad9361_adc_fifo/dout_valid_0 hwval_core/valid_in

# ── AXI-Lite ──────────────────────────────────────────────────────────
ad_cpu_interconnect 0x7C460000 hwval_core

# ── DMA: ring v2 + legacy replica ring on HP1 @ clk_out1 ──────────────
# HP1 was maia_sdr/m_axi_spectrometer (now deleted). Same pattern as
# P25: first call creates axi_hp1_interconnect -> S_AXI_HP1 (and
# reconnects S_AXI_HP1_ACLK), later calls add slave ports.
ad_ip_parameter sys_ps7 CONFIG.PCW_USE_S_AXI_HP1 {1}
ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 sys_ps7/S_AXI_HP1
ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 hwval_core/m_axi_ringv2
ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 hwval_core/m_axi_legacy

# ── Memory testers: mt0 -> HP0, mt1 -> HP3 @ clk_out2 (125 MHz) ───────
# Same ADI procs: each creates axi_hp{0,3}_interconnect (axi_interconnect
# on xc7z), connects S_AXI_HPx_ACLK to clk_out2 and assigns
# HPx_DDR_LOWOCM to the master's address space.
ad_mem_hp0_interconnect maia_sdr_clk/clk_out2 sys_ps7/S_AXI_HP0
ad_mem_hp0_interconnect maia_sdr_clk/clk_out2 hwval_core/m_axi_mt0
ad_mem_hp3_interconnect maia_sdr_clk/clk_out2 sys_ps7/S_AXI_HP3
ad_mem_hp3_interconnect maia_sdr_clk/clk_out2 hwval_core/m_axi_mt1

# ── Interrupt ─────────────────────────────────────────────────────────
# With maia_iio, pluto base wired:
#   In13 = axi_ad9361_adc_dma/irq
#   In12 = axi_ad9361_dac_dma/irq
#   In11 = maia_sdr/interrupt_out (now deleted)
# Reconnect In11 to hwval_core (DT: SPI 55, same as p25-core).
set irq_nets [get_bd_nets -quiet -of_objects [get_bd_pins sys_concat_intc/In11]]
if {$irq_nets ne ""} {
    disconnect_bd_net $irq_nets [get_bd_pins sys_concat_intc/In11]
}
connect_bd_net [get_bd_pins hwval_core/interrupt_out] [get_bd_pins sys_concat_intc/In11]
