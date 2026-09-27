# Package Fishball hwval (hardware-validation) IP core
#
# Mirrors ip/p25-core/package_ip.tcl. Differences, all driven by the
# hwval_core port list (doc/HW_VALIDATION_SUITE.md section 6.1):
#   - the `rst` output POLARITY block is guarded (applied only if the
#     generated Verilog has a `rst` port)
#   - three extra clock inputs: lclk_clk, fclk1_clk, y1_clk
#   - m_axi_ringv2 / m_axi_legacy -> clk (62.5 MHz, HP1)
#     m_axi_mt0 / m_axi_mt1       -> clk2x_clk (125 MHz, HP0 / HP3)
#   - ad_clkout is a plain data input (AD9361 CLK_OUT sampled in
#     clk3x_clk); a clock interface inferred from its name is removed
#   - writes package_ip.ok as the very last step so build_fpga.bat can
#     tell a complete packaging run from one that died half-way
#
# VLNV: fishball-hwval:hwval_core_${HWVAL_CONFIG}:hwval_core:${IP_CORE_VERSION}

create_project hwval_core_$::env(HWVAL_CONFIG) . -force
add_files hwval_core.v
add_files -fileset constrs_1 -norecurse ../hwval_core.xdc
set_property top top [current_fileset]
load_features ipservices
ipx::package_project -import_files -root_dir . -vendor fishball-hwval -library user -taxonomy /Fishball-HWVAL -force
set_property name hwval_core [ipx::current_core]
set_property library hwval_core_$::env(HWVAL_CONFIG) [ipx::current_core]
set_property display_name {Fishball hwval} [ipx::current_core]
set_property description "Fishball hardware-validation core (config: $::env(HWVAL_CONFIG))" [ipx::current_core]
set_property vendor_display_name {Fishball hwval} [ipx::current_core]
set_property version $::env(IP_CORE_VERSION) [ipx::current_core]

# sampling_clk interface (util_ad9361_divclk/clk_out)
ipx::add_bus_interface sampling_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces sampling_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces sampling_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces sampling_clk -of_objects [ipx::current_core]]

# clk interface (sync domain, maia_sdr_clk/clk_out1 = 62.5 MHz)
ipx::add_bus_interface clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
ipx::add_port_map CLK [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
set_property physical_name clk [ipx::get_port_maps CLK \
                                    -of_objects [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]]

# clk3x_clk interface (maia_sdr_clk/clk_out3 = 187.5 MHz)
ipx::add_bus_interface clk3x_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces clk3x_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces clk3x_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces clk3x_clk -of_objects [ipx::current_core]]

# clk2x_clk interface (mem domain, maia_sdr_clk/clk_out2 = 125 MHz)
ipx::add_bus_interface clk2x_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces clk2x_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces clk2x_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces clk2x_clk -of_objects [ipx::current_core]]

# lclk_clk interface (axi_ad9361/l_clk, census only)
ipx::add_bus_interface lclk_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces lclk_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces lclk_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces lclk_clk -of_objects [ipx::current_core]]

# fclk1_clk interface (sys_200m_clk = PS7 FCLK1 200 MHz, census only)
ipx::add_bus_interface fclk1_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces fclk1_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces fclk1_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces fclk1_clk -of_objects [ipx::current_core]]

# y1_clk interface (50 MHz board oscillator on pin N18, census only)
ipx::add_bus_interface y1_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces y1_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces y1_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces y1_clk -of_objects [ipx::current_core]]

# rst output (sync-domain reset driven from CORE_RESET; left unconnected
# in the BD like p25_core/rst). Guarded: present only if hwval_top.py
# exports sync.rst as a port (it currently does).
if {![catch {ipx::get_bus_interfaces rst -of_objects [ipx::current_core]} rst_busif] \
        && [llength $rst_busif] > 0} {
    ipx::add_bus_parameter POLARITY [ipx::get_bus_interfaces rst -of_objects [ipx::current_core]]
    set_property value ACTIVE_HIGH \
        [ipx::get_bus_parameters POLARITY -of_objects \
             [ipx::get_bus_interfaces rst -of_objects [ipx::current_core]]]
}

# s_axi_lite_clk interface (sys_cpu_clk = PS7 FCLK0 100 MHz)
ipx::add_bus_interface s_axi_lite_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces s_axi_lite_clk -of_objects [ipx::current_core]]

# s_axi_lite_rst interface (active-high, sys_cpu_reset)
ipx::add_bus_interface s_axi_lite_rst [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:reset_rtl:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:reset:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]
ipx::add_bus_parameter POLARITY [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]
set_property value ACTIVE_HIGH [ipx::get_bus_parameters POLARITY -of_objects [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]]

# ad_clkout is DATA (AD9361 CLK_OUT sampled in clk3x_clk), not a clock.
# The packager may infer a clock interface from the "clk" in its name;
# remove any clock interface that maps it so the BD sees a plain pin.
set ad_clkout_ifs {}
foreach busif [ipx::get_bus_interfaces -of_objects [ipx::current_core]] {
    if {![string match "*:signal:clock:*" [get_property bus_type_vlnv $busif]]} {
        continue
    }
    foreach pm [ipx::get_port_maps -of_objects $busif] {
        if {[get_property physical_name $pm] eq "ad_clkout"} {
            lappend ad_clkout_ifs [get_property name $busif]
        }
    }
}
foreach busif_name $ad_clkout_ifs {
    puts "INFO: hwval_core: removing inferred clock interface '$busif_name' (ad_clkout is a data input)"
    ipx::remove_bus_interface $busif_name [ipx::current_core]
}

# associate buses to clocks
ipx::associate_bus_interfaces -busif s_axi_lite -clock clk -remove [ipx::current_core]
ipx::associate_bus_interfaces -busif s_axi_lite -clock s_axi_lite_clk [ipx::current_core]

# AXI managers: ring v2 + production-replica ring -> clk (HP1 @ clk_out1),
# memory testers -> clk2x_clk (HP0 / HP3 @ clk_out2). The packager
# associates AXI interfaces with `clk` by default (that is why
# s_axi_lite needs the -remove above); generalise that: drop any
# association with a clock other than the intended one, then associate.
# ASSOCIATED_BUSIF is the bus parameter ipx::associate_bus_interfaces
# maintains (same name adi_ip_xilinx.tcl sets directly).
proc hwval_assoc_busifs {clkif} {
    if {[catch {get_property value [ipx::get_bus_parameters ASSOCIATED_BUSIF \
            -of_objects [ipx::get_bus_interfaces $clkif -of_objects [ipx::current_core]]]} v]} {
        return {}
    }
    return [split $v ":"]
}
set hwval_clock_ifs {}
foreach busif [ipx::get_bus_interfaces -of_objects [ipx::current_core]] {
    if {[string match "*:signal:clock:*" [get_property bus_type_vlnv $busif]]} {
        lappend hwval_clock_ifs [get_property name $busif]
    }
}
set hwval_busif_clock [list \
    s_axi_lite   s_axi_lite_clk \
    m_axi_ringv2 clk \
    m_axi_legacy clk \
    m_axi_mt0    clk2x_clk \
    m_axi_mt1    clk2x_clk]
foreach {b want} $hwval_busif_clock {
    foreach c $hwval_clock_ifs {
        if {$c ne $want && [lsearch -exact [hwval_assoc_busifs $c] $b] >= 0} {
            puts "INFO: hwval_core: removing association $b -> $c"
            ipx::associate_bus_interfaces -busif $b -clock $c -remove [ipx::current_core]
        }
    }
    if {[lsearch -exact [hwval_assoc_busifs $want] $b] < 0} {
        ipx::associate_bus_interfaces -busif $b -clock $want [ipx::current_core]
    }
}

# Verify: each AXI interface is associated with exactly its clock. A
# manager left on the wrong clock would only show up much later, as a
# BD validation error or a bogus CDC in the HP interconnect.
foreach {b want} $hwval_busif_clock {
    set owners {}
    foreach c $hwval_clock_ifs {
        if {[lsearch -exact [hwval_assoc_busifs $c] $b] >= 0} {
            lappend owners $c
        }
    }
    puts "INFO: hwval_core: $b is associated with clock(s): $owners"
    if {$owners ne [list $want]} {
        error "hwval_core: $b must be associated with $want only (got: $owners)"
    }
}

# interrupt
ipx::add_bus_interface interrupt [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:interrupt_rtl:1.0 \
    [ipx::get_bus_interfaces interrupt -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:interrupt:1.0 \
    [ipx::get_bus_interfaces interrupt -of_objects [ipx::current_core]]
set_property interface_mode master \
    [ipx::get_bus_interfaces interrupt -of_objects [ipx::current_core]]
ipx::add_port_map INTERRUPT \
    [ipx::get_bus_interfaces interrupt -of_objects [ipx::current_core]]
set_property physical_name interrupt_out \
    [ipx::get_port_maps INTERRUPT \
         -of_objects [ipx::get_bus_interfaces interrupt -of_objects [ipx::current_core]]]

ipx::create_xgui_files [ipx::current_core]
ipx::save_core [ipx::current_core]

# Completion marker checked by build_fpga.bat --hwval.
set ok_file [open package_ip.ok w]
puts $ok_file "hwval_core $::env(HWVAL_CONFIG) $::env(IP_CORE_VERSION)"
close $ok_file
