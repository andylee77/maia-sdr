# The fork's HDL tests. They use upstream Maia's package (maia-hdl/maia_hdl) and two of its test
# helpers (maia-hdl/test: amaranth_sim, common_edge), which this puts on the import path.
import pathlib
import sys

_MAIA_HDL = pathlib.Path(__file__).resolve().parents[2] / 'maia-hdl'
for _path in (_MAIA_HDL, _MAIA_HDL / 'test'):
    if str(_path) not in sys.path:
        sys.path.append(str(_path))
