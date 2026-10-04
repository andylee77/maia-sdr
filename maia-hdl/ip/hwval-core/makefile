VIVADO := vivado -mode batch -source
# hwval_hdl has no *_version module: the version is part of the VLNV
# contract (doc/HW_VALIDATION_SUITE.md section 6.1) and must match
# IP_CORE_VERSION in build_fpga.bat --hwval.
IP_CORE_VERSION ?= 0.1.0

configs = default

all: $(configs)

$(configs):
	mkdir -p $@
	PYTHONPATH="../../../scanner-hdl:../../:${PYTHONPATH}" python3 -m hwval_hdl.hwval_top --config $@ $@/hwval_core.v --svd $@/hwval.svd --json $@/hwval_regs.json --md $@/hwval_register_map.md
	cd $@ && IP_CORE_VERSION=$(IP_CORE_VERSION) HWVAL_CONFIG=$@ $(VIVADO) ../package_ip.tcl

clean:
	rm -rf $(configs)

.PHONY: all clean $(configs)
