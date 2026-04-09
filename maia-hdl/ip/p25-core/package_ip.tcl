# Package Fishball P25 IP core

create_project p25_core_$::env(P25_CONFIG) . -force
add_files p25_core.v
add_files -fileset constrs_1 -norecurse ../p25_core.xdc
set_property top top [current_fileset]
load_features ipservices
ipx::package_project -import_files -root_dir . -vendor fishball-p25 -library user -taxonomy /Fishball-P25 -force
set_property name p25_core [ipx::current_core]
set_property library p25_core_$::env(P25_CONFIG) [ipx::current_core]
set_property display_name {Fishball P25} [ipx::current_core]
set_property description "Fishball P25 core (config: $::env(P25_CONFIG))" [ipx::current_core]
set_property vendor_display_name {Fishball P25} [ipx::current_core]
set_property version $::env(IP_CORE_VERSION) [ipx::current_core]

# sampling_clk interface
ipx::add_bus_interface sampling_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces sampling_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces sampling_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces sampling_clk -of_objects [ipx::current_core]]

# clk interface
ipx::add_bus_interface clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
ipx::add_port_map CLK [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]
set_property physical_name clk [ipx::get_port_maps CLK \
                                    -of_objects [ipx::get_bus_interfaces clk -of_objects [ipx::current_core]]]

# clk3x_clk interface
ipx::add_bus_interface clk3x_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces clk3x_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces clk3x_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces clk3x_clk -of_objects [ipx::current_core]]

# rst output
ipx::add_bus_parameter POLARITY [ipx::get_bus_interfaces rst -of_objects [ipx::current_core]]
set_property value ACTIVE_HIGH \
    [ipx::get_bus_parameters POLARITY -of_objects \
         [ipx::get_bus_interfaces rst -of_objects [ipx::current_core]]]

# s_axi_lite_clk interface
ipx::add_bus_interface s_axi_lite_clk [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:clock_rtl:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_clk -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:clock:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_clk -of_objects [ipx::current_core]]
ipx::add_bus_parameter FREQ_HZ [ipx::get_bus_interfaces s_axi_lite_clk -of_objects [ipx::current_core]]

# s_axi_lite_rst interface
ipx::add_bus_interface s_axi_lite_rst [ipx::current_core]
set_property abstraction_type_vlnv xilinx.com:signal:reset_rtl:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]
set_property bus_type_vlnv xilinx.com:signal:reset:1.0 \
    [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]
ipx::add_bus_parameter POLARITY [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]
set_property value ACTIVE_HIGH [ipx::get_bus_parameters POLARITY -of_objects [ipx::get_bus_interfaces s_axi_lite_rst -of_objects [ipx::current_core]]]

# associate buses to clocks
ipx::associate_bus_interfaces -busif s_axi_lite -clock clk -remove [ipx::current_core]
ipx::associate_bus_interfaces -busif s_axi_lite -clock s_axi_lite_clk [ipx::current_core]
ipx::associate_bus_interfaces -busif m_axi_dibit -clock clk [ipx::current_core]
ipx::associate_bus_interfaces -busif m_axi_traffic -clock clk [ipx::current_core]
# Phase 6C: control-channel post-DDC IQ ring DMA on m_axi_iq.
# Same clock domain as the dibit/traffic masters; SmartConnect on the
# block-design side handles arbitration to HP1.
ipx::associate_bus_interfaces -busif m_axi_iq -clock clk [ipx::current_core]

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
