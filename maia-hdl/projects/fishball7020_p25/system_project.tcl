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

# use improved implementation strategy for best timing results
# (Phase 6E.6e: Performance_ExtraTimingOpt for tighter intra-clock
# closure on the dense post-CORDIC layout. Was
# Performance_ExplorePostRoutePhysOpt; switched after the lerp
# pipeline fix shifted violations into the AXI HP2 interconnect.)
set_property strategy Performance_ExtraTimingOpt [get_runs impl_1]

set_property is_enabled false [get_files  *system_sys_ps7_0.xdc]
adi_project_run fishball_p25
source $ad_hdl_dir/library/axi_ad9361/axi_ad9361_delay.tcl
