####################################################################################
## Fishball hwval (hardware-validation) - Vivado project build
## Based on fishball7020_p25 (see README.md for the deltas)
## The Windows flow is build_fpga.bat --hwval; this Makefile is the
## ADI Linux-make equivalent and is kept in step with fishball7020_p25.
####################################################################################

PROJECT_NAME := fishball_hwval

M_DEPS += ../../adi-hdl/library/common/ad_iobuf.v
M_DEPS += ../../adi-hdl/library/axi_ad9361/axi_ad9361_delay.tcl
M_DEPS += ip_cores
LIB_DEPS += xilinx/util_clkdiv
LIB_DEPS += axi_ad9361
LIB_DEPS += util_rfifo
LIB_DEPS += util_wfifo

include ../../adi-hdl/projects/scripts/project-xilinx.mk

ip_cores:
	$(MAKE) -C ../../ip

.PHONY: ip_cores
