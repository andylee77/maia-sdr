#
# Fishball radio core - the 8-VSB model's vectors for the gateware's tests (doc/changes/085): each
# stage's inputs and outputs, written by the model (scanner/src/protocol/atsc/pl/vectors.rs).
#
# The stage tests take windows of a set: `reset` (the first symbols from the reset), `count` (the
# first counted segments, with their early and late evaluations), `equalizer` (the first tap set's
# load) and `decoder` (the first data symbols). The default tests read the windows committed beside
# this file (`vsb_vectors/`, written by `python -m test.vsb_vectors <full set>` from scanner-hdl/);
# with VSB_VECTORS set to a full set's folder they cut them from it. The whole-field tests read a
# full set's files themselves.
#
# SPDX-License-Identifier: MIT
#

import json
import os
import pathlib
import sys

import numpy as np

HERE = pathlib.Path(__file__).parent / 'vsb_vectors'

TRACE = np.dtype([('pos', '<u8'), ('filtered', '<i4', 2), ('gain', '<u4'), ('gained', '<i4', 2),
                  ('theta', '<u4'), ('turned', '<i4', 2), ('dc', '<i4'), ('y', '<i4')])
SIDES = np.dtype([('n', '<u4'), ('late', '<u4'), ('pos', '<u8'), ('filtered', '<i4', 2),
                  ('theta', '<u4'), ('value', '<i4')])
FIELDS = np.dtype([('start', '<u8'), ('found', '<u4'), ('inverted', '<u4')])
LOADS = np.dtype([('field', '<u4'), ('start', '<u4'), ('taps', '<i2', 128)])
DECISIONS = np.dtype([('enc', 'u1'), ('pair', 'u1'), ('soft', '<i2')])

WINDOWS = ('reset', 'count', 'equalizer', 'decoder')
# The decoder's window: data symbols, and the equalizer's outputs that hold them.
DECODER_SYMBOLS = 12000
DECODER_OUTPUTS = 832 * 17


class Vectors:
    """A vector set: a full one from the model, or the committed windows."""
    def __init__(self, folder=None):
        folder = folder or os.environ.get('VSB_VECTORS')
        self.full = folder is not None
        self.dir = pathlib.Path(folder) if folder else HERE
        self.meta = json.loads((self.dir / 'meta.json').read_text())

    def _file(self, name, dtype):
        return np.fromfile(self.dir / name, dtype=dtype)

    def mf_table(self):
        if self.full:
            return self._file('mf_table.bin', '<i2').astype(int).tolist()
        return np.load(HERE / 'tables.npz')['mf_table'].astype(int).tolist()

    def window(self, name):
        """A window's arrays, each with the index of its first element in the full set
        (`<array>_at`)."""
        if not self.full:
            with np.load(HERE / f'{name}.npz') as z:
                return {k: z[k] for k in z.files}
        x = self._file('x.bin', '<i2').reshape(-1, 2)
        if name == 'reset':
            trace = self._file('trace.bin', TRACE)
            return dict(x=x[:2000], x_at=0, trace=trace[:1500], trace_at=0)
        if name == 'count':
            first = int(self.meta['first_segment'])
            trace = self._file('trace.bin', TRACE)[first:first + 200]
            sides = self._file('sides.bin', SIDES)
            sides = sides[(sides['n'] >= first) & (sides['n'] < first + 200)]
            pos = np.concatenate([trace['pos'], sides['pos']]) >> np.uint64(32)
            lo = (int(pos.min()) - 64) // 64 * 64
            hi = int(pos.max()) + 64
            return dict(x=x[lo:hi], x_at=lo, trace=trace, trace_at=first, sides=sides)
        if name == 'equalizer':
            loads = self._file('loads.bin', LOADS)
            at = int(loads[0]['start'])
            first = int(self.meta['first_field'])
            y = self._file('y.bin', '<i2')
            eq = self._file('eq_out.bin', '<i2')
            return dict(y=y[at - 2000:at + 2000], y_at=at - 2000,
                        eq_out=eq[at - 2000 - first:at + 2000 - first], eq_out_at=at - 2000,
                        load=loads[:1], fields=self._file('fields.bin', FIELDS))
        if name == 'decoder':
            return dict(eq_out=self._file('eq_out.bin', '<i2')[:DECODER_OUTPUTS],
                        decisions=self._file('decisions.bin', DECISIONS)[:DECODER_SYMBOLS])
        raise KeyError(name)


def write_windows(folder):
    """The committed windows, cut from the full set in `folder`."""
    v = Vectors(folder)
    HERE.mkdir(exist_ok=True)
    for name in WINDOWS:
        np.savez_compressed(HERE / f'{name}.npz', **v.window(name))
    np.savez_compressed(HERE / 'tables.npz', mf_table=np.array(v.mf_table(), dtype=np.int16))
    meta = {k: v.meta[k] for k in ('capture', 'fs', 'step0', 'first_segment', 'first_field')}
    (HERE / 'meta.json').write_text(json.dumps(meta, indent=2) + '\n')


if __name__ == '__main__':
    write_windows(sys.argv[1])
