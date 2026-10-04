VIVADO := vivado -mode batch -source
IP_CORE_VERSION := $(shell PYTHONPATH="../../../scanner-hdl:../../:${PYTHONPATH}" python3 -m radio_core.p25_top_version)

configs = default

all: $(configs)

$(configs):
	mkdir -p $@
	PYTHONPATH="../../../scanner-hdl:../../:${PYTHONPATH}" python3 -m radio_core.p25_top --config $@ $@/p25_core.v
	cd $@ && IP_CORE_VERSION=$(IP_CORE_VERSION) P25_CONFIG=$@ $(VIVADO) ../package_ip.tcl

clean:
	rm -rf $(configs)

.PHONY: all clean $(configs)
