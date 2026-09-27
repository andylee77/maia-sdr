set IGNORE_VERSION_CHECK 1
source ../../adi-hdl/scripts/adi_env.tcl
source $ad_hdl_dir/projects/scripts/adi_project_xilinx.tcl
source $ad_hdl_dir/projects/scripts/adi_board.tcl

set p_device "xc7z020clg400-1"
adi_project fishball_hwval

adi_project_files fishball_hwval [list \
  "system_top.v" \
  "system_constr.xdc" \
  "$ad_hdl_dir/library/common/ad_iobuf.v"]

# Same implementation strategy as fishball7020_p25
# (Performance_ExtraTimingOpt).
set_property strategy Performance_ExtraTimingOpt [get_runs impl_1]

set_property is_enabled false [get_files  *system_sys_ps7_0.xdc]
# adi_project_run writes fishball_hwval.sdk/system_top.xsa only when
# timing is met; otherwise it writes system_top_bad_timing.xsa and
# raises "Timing Constraints NOT met", which aborts this script with a
# non-zero exit. build_fpga.bat --hwval treats that as a hard failure
# (no bad_timing promotion for hwval).
adi_project_run fishball_hwval
source $ad_hdl_dir/library/axi_ad9361/axi_ad9361_delay.tcl
