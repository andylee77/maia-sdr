#
# rerun_with_strategy.tcl
#
# Phase 6E.6e timing-closure helper. Opens the existing
# fishball_p25 project, resets impl_1 + synth_1 (preserving the
# IP synth cache), switches to a tighter implementation strategy,
# re-runs synth + impl, generates the bitstream, exports the XSA.
#
# The default build_fpga.bat path uses Performance_ExplorePostRoutePhysOpt
# which left a 5-endpoint clk_fpga_0 violation in the AXI HP2
# interconnect after the lerp pipeline fix shifted placement.
# Performance_ExtraTimingOpt does more aggressive timing-driven
# placement and should fix it without source changes.
#
# Run from the project directory:
#   vivado -mode batch -source rerun_with_strategy.tcl -notrace
#

set IGNORE_VERSION_CHECK 1

# Open existing project. Faster than recreating from scratch
# because the IP cache, BD, and constraint files are all reused.
open_project fishball_p25.xpr

# Reset only the top-level synth + impl runs. Leaves the
# system_*_synth_1 IP runs (BlockDesign IP cache) alone --
# those don't depend on the implementation strategy and would
# take ~5 minutes to re-synthesise unnecessarily.
reset_run impl_1
reset_run synth_1

# Switch implementation strategy. Performance_ExtraTimingOpt
# is more aggressive than Performance_ExplorePostRoutePhysOpt:
# it runs additional placement + routing iterations focused on
# closing setup violations on the worst-slack endpoints.
set_property strategy Performance_ExtraTimingOpt [get_runs impl_1]

# Re-run synth + impl. wait_on_run blocks until impl_1 finishes
# (synth_1 fires automatically as a dependency).
launch_runs impl_1 -to_step write_bitstream -jobs 8
wait_on_run impl_1

# Sanity check that the implementation actually completed.
set impl_status [get_property STATUS [get_runs impl_1]]
puts "impl_1 status: $impl_status"
if {[get_property PROGRESS [get_runs impl_1]] != "100%"} {
    puts "ERROR: impl_1 did not complete to 100%"
    exit 1
}

# Open the implementation, write a fresh timing report, and
# export the hardware platform (XSA).
open_run impl_1
report_timing_summary -warn_on_violation -file timing_impl.log

# Export hardware platform. The build_fpga.bat post-step copies
# this to the Tezuka bitstream cache.
set xsa_path "fishball_p25.sdk/system_top.xsa"
file mkdir [file dirname $xsa_path]
write_hw_platform -fixed -force -include_bit -file $xsa_path

puts ""
puts "============================================================"
puts " Re-bake with Performance_ExtraTimingOpt complete"
puts "============================================================"
puts " XSA: [pwd]/$xsa_path"
puts " Timing report: [pwd]/timing_impl.log"
puts ""

close_project
