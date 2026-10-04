set IGNORE_VERSION_CHECK 1
source ../../adi-hdl/scripts/adi_env.tcl
source $ad_hdl_dir/projects/scripts/adi_project_xilinx.tcl
source $ad_hdl_dir/projects/scripts/adi_board.tcl

set p_device "xc7z020clg400-1"
adi_project fishball_p25

adi_project_files fishball_p25 [list \
  "system_top.v" \
  "system_constr.xdc" \
  "$ad_hdl_dir/library/common/ad_iobuf.v"]

# Implementation strategy for intra-clock timing closure (ExtraTimingOpt closed paths that
# ExplorePostRoutePhysOpt left into the AXI HP2 interconnect).
set_property strategy Performance_ExtraTimingOpt [get_runs impl_1]

set_property STEPS.ROUTE_DESIGN.TCL.POST [file normalize utilization_hier.tcl] [get_runs impl_1]

set_property is_enabled false [get_files  *system_sys_ps7_0.xdc]
# adi_project_run writes fishball_p25.sdk/system_top.xsa only when timing is met; otherwise it
# writes system_top_bad_timing.xsa and raises "Timing Constraints NOT met", which aborts this
# script with a non-zero exit. build_fpga.bat --p25 treats that as a hard failure.
adi_project_run fishball_p25
source $ad_hdl_dir/library/axi_ad9361/axi_ad9361_delay.tcl
