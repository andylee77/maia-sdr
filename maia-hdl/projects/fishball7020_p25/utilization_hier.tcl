# Route-design hook (system_project.tcl): the hierarchical utilization report, written beside the
# run's other reports (fishball_p25.runs/impl_1/utilization_hier.rpt): the design to depth 4, then
# the radio core's own cell down to its blocks' parts (each lane's DDC stages and packetizer, the
# spectrometer's FFT, integrator and DMA, the rings and the register banks).
report_utilization -hierarchical -hierarchical_depth 4 -file utilization_hier.rpt
report_utilization -hierarchical -hierarchical_depth 4 \
    -cells [get_cells i_system_wrapper/system_i/p25_core/inst] -append -file utilization_hier.rpt
