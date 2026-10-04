# scanner-hdl

The fork's gateware, in Amaranth, kept apart from upstream Maia's (`../maia-hdl`), which it
imports.

| Path | What |
|------|------|
| `radio_core/` | The radio core (product "rad1"): DDC lanes into one tagged lane ring, the wideband spectrometer, the raw IQ capture, the register bridge. Design: `../doc/changes/079_general_radio_core.md` |
| `hwval_hdl/` | The hardware-validation core for the two-unit bench (`../doc/HW_VALIDATION_SUITE.md`) |
| `test/` | Both packages' tests. They use upstream's `maia_hdl` and two of its test helpers (`../maia-hdl/test`) |
| `test_cocotb/` | Co-simulation tests (cocotb and Icarus; run through `../sim_hdl.bat` in Docker) |
| `generate_svd.py` | Writes the core's register map to `../scanner/core-pac/core.svd` |
| `radio_core/bench_map.py` | Writes the bench's map of the same registers, with their read-to-clear registers and clock domains, to `../bench/share/p25_regs.json` (`python -m radio_core.bench_map`) |

The Vivado projects and the IP packaging stay in `../maia-hdl/projects/fishball7020_p25/`,
`fishball7020_hwval/` and `../maia-hdl/ip/p25-core/`, `hwval-core/`, because ADI's scripts use
paths relative to them. `../build_fpga_p25_pretty.sh` builds the bitstream (`../BUILD_FPGA.md`).

## Tests

From this folder, in the repo's `.venv-hdl`:

```sh
python -m pytest test/
```

The long sweeps run with `MAIA_HDL_SLOW_TESTS=1`.

## Generating by hand

`radio_core` imports `maia_hdl`, so `../maia-hdl` goes on the path (with `;` between the entries
for Windows Python):

```sh
PYTHONPATH=.:../maia-hdl python -m radio_core.p25_top --config default p25_core.v
PYTHONPATH=.:../maia-hdl python generate_svd.py
```

The build does this itself (`../build_hdl.sh`, in Docker).
