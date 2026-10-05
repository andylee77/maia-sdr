#
# Fishball radio core - whole-field simulations in Verilator (doc/changes/085): a design's Verilog
# from Amaranth, built with a C++ bench (test_verilator/) and run in the local Docker image
# (test_verilator/Dockerfile: `docker build -t fishball-verilator:24.04 test_verilator`).
#
# SPDX-License-Identifier: MIT
#

import json
import os
import pathlib
import shutil
import subprocess

from amaranth.back import verilog

REPO = pathlib.Path(__file__).resolve().parents[2]
BENCHES = REPO / 'scanner-hdl' / 'test_verilator'
IMAGE = 'fishball-verilator:24.04'


def available():
    """Docker and the image are here."""
    try:
        r = subprocess.run(['docker', 'image', 'inspect', IMAGE], capture_output=True)
    except FileNotFoundError:
        return False
    return r.returncode == 0


def in_container(path):
    """A path under the repository as the container sees it."""
    return '/work/' + pathlib.Path(path).resolve().relative_to(REPO).as_posix()


def run(top, ports, name, bench, args, build_dir):
    """Converts `top` to Verilog as module `name`, builds it with `bench` (a file in
    test_verilator/) and runs it with `args` (paths given as the container sees them). Returns
    the run's standard output."""
    build_dir = pathlib.Path(build_dir)
    build_dir.mkdir(parents=True, exist_ok=True)
    (build_dir / f'{name}.v').write_text(verilog.convert(top, ports=ports, name=name))
    shutil.copy(BENCHES / bench, build_dir / 'tb.cpp')
    here = in_container(build_dir)
    script = (f'cd {here} && verilator --cc --exe --build -O3 -j 8 -Wno-fatal -Wno-lint -Wno-style '
              f'--x-assign 0 --x-initial 0 --top-module {name} {name}.v tb.cpp -o sim '
              f'> build.log 2>&1 && ./obj_dir/sim {" ".join(args)}')
    r = subprocess.run(['docker', 'run', '--rm', '-v', f'{REPO}:/work', IMAGE, 'bash', '-c', script],
                       capture_output=True, text=True)
    if r.returncode != 0:
        log = (build_dir / 'build.log').read_text(errors='replace')[-4000:] if (build_dir / 'build.log').exists() else ''
        raise RuntimeError(f'verilator run failed ({r.returncode}):\n{r.stdout}\n{r.stderr}\n{log}')
    return r.stdout


def summary(path):
    return json.loads(pathlib.Path(path).read_text())
