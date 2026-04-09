# Fishball P25 - Block Design (in-tree build)
# Sources pluto base WITH maia_iio (keeps IIO DMA for AD9361 streaming),
# then replaces the maia_sdr IP with p25_core.
#
# What we KEEP from maia_iio: axi_dmac (RX+TX DMA), util_cpack2/upack2,
# AD9361 IIO streaming, 8-bit mode support, FIR filters.
# What we REMOVE: maia_sdr IP (spectrometer + recorder).
# What we ADD: p25_core (DDC + C4FM demod + dibit DMA).

set LVDS_ENABLE "LVDS_ENABLE"
set fishball "fishball"
set p25_iio "p25_iio"
set with_tx_fir "with_tx_fir"
set with_rx_fir_maia "with_rx_fir_maia"
set maia_iio "maia_iio"

# Source the pluto base design (relative paths resolve in-tree)
source ../pluto/system_bd.tcl

# ── Replace maia_sdr with p25_core ────────────────────────────────────
# Delete maia_sdr IP (spectrometer + recorder) but keep everything else
# the maia_iio path created (IIO DMA, cpack/upack, FIR filters, etc.)
delete_bd_objs [get_bd_cells maia_sdr]

# Clean up dangling nets from deleted maia_sdr
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

# Leave the 8-bit mode / FIR filter chain INTACT.
# The mux chain (muxcs8, rxcs12_cs8, etc.) is still wired to the ADC/DAC
# pack/unpack path. Without maia_sdr's decim outputs driving the select
# inputs, the muxes default to pass-through (input 0 = direct FIFO data).
#
# Only delete the cells that directly reference deleted maia_sdr ports:
# - interclk FIFOs connected to maia_sdr/decim_re_out, decim_im_out
# - mux_decim_i/q which use interclk outputs
# Delete cells that directly referenced maia_sdr's decim outputs.
# interclk FIFOs had s_axis_tdata connected to maia_sdr/decim_re_out etc.
# mux_decim_i/q had data_in_1/valid_in_1 from the interclk outputs.
foreach cell {interclk_i interclk_q mux_decim_i mux_decim_q} {
    catch {delete_bd_objs [get_bd_cells $cell]}
}

# The 8-bit mode mux chain (muxcs8, rxcs12_cs8, etc.) remains but always
# operates in pass-through mode (select=0 → input 0 = direct FIFO data).
# After deleting mux_decim/interclk, some mux _in_1 inputs are dangling.
# Tie them to valid sources to prevent trimming errors in opt_design.
#
# rxcs12_cs8 sample inputs (were from mux_decim outputs)
ad_connect util_ad9361_adc_fifo/dout_data_0 rxcs12_cs8/sample_in1
ad_connect util_ad9361_adc_fifo/dout_data_1 rxcs12_cs8/sample_in2

# muxcs8 valid_in_1 (was from rx_fir_decimator or mux_decim_i)
ad_connect util_ad9361_adc_fifo/dout_valid_0 muxcs8/valid_in_1

# muxcs8_2 valid_in_1 and data_in_1 are already connected by pluto base

# Reconnect gpio_o to PS7 (was through logic_orgpio OR gate)
catch {delete_bd_objs [get_bd_nets -quiet -filter {NAME =~ gpio_o*}]}
ad_connect sys_ps7/GPIO_O gpio_o

# Add P25 IP repo (in-tree, relative to project dir)
set p25_ip_dir [file normalize ../../ip/p25-core]
set_property ip_repo_paths [concat [get_property ip_repo_paths [current_project]] \
    [list $p25_ip_dir]] [current_project]
update_ip_catalog

set p25_config $::env(P25_CONFIG)
set p25_version $::env(IP_CORE_VERSION)
create_bd_cell -type ip \
    -vlnv fishball-p25:p25_core_${p25_config}:p25_core:${p25_version} p25_core

# ── Rewire clocks ─────────────────────────────────────────────────────
ad_connect sys_cpu_clk p25_core/s_axi_lite_clk
ad_connect sys_cpu_reset p25_core/s_axi_lite_rst
ad_connect maia_sdr_clk/clk_out1 p25_core/clk
ad_connect maia_sdr_clk/clk_out3 p25_core/clk3x_clk
ad_connect p25_core/sampling_clk util_ad9361_divclk/clk_out

# ── Rewire IQ data ───────────────────────────────────────────────────
ad_connect adc_i_slice/Dout p25_core/re_in
ad_connect adc_q_slice/Dout p25_core/im_in

# ── AXI-Lite ─────────────────────────────────────────────────────────
ad_cpu_interconnect 0x7C460000 p25_core

# ── DMA: dibit + traffic + IQ on HP1 ────────────────────────────────
# HP1 was used by maia_sdr/m_axi_spectrometer (now deleted).
# Reuse HP1 for all three P25 DMA masters. ad_mem_hp1_interconnect is
# idempotent — repeated calls extend the same SmartConnect rather than
# creating a new one. HP1 budget at ~1.7 GB/s easily absorbs:
#   - dibit  ~1.28 KB/s
#   - traffic ~1.28 KB/s
#   - iq    ~250 KB/s   (Phase 6C, 62.5 kSPS x 4 B post-DDC IQ)
# See doc/P25_ADDRESS_MAP.md for the full carve-out / bandwidth table.
ad_ip_parameter sys_ps7 CONFIG.PCW_USE_S_AXI_HP1 {1}
ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 sys_ps7/S_AXI_HP1
ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 p25_core/m_axi_dibit
ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 p25_core/m_axi_traffic
ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 p25_core/m_axi_iq

# ── Interrupt ─────────────────────────────────────────────────────────
# With maia_iio, pluto base wired:
#   In13 = axi_ad9361_adc_dma/irq
#   In12 = axi_ad9361_dac_dma/irq
#   In11 = maia_sdr/interrupt_out (now deleted)
# Reconnect In11 to p25_core.
set irq_nets [get_bd_nets -quiet -of_objects [get_bd_pins sys_concat_intc/In11]]
if {$irq_nets ne ""} {
    disconnect_bd_net $irq_nets [get_bd_pins sys_concat_intc/In11]
}
connect_bd_net [get_bd_pins p25_core/interrupt_out] [get_bd_pins sys_concat_intc/In11]
