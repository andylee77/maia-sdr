#!/usr/bin/env python3
"""Prototype lowpass FIR design for the P25 polyphase channelizer.

Designs an even-symmetric lowpass prototype of length `M * K` whose
passband is one polyphase branch wide (cutoff at Nyquist/M). When this
filter is decomposed into M polyphase branches and the M sub-filter
outputs are FFT'd every M input samples, the result is M frequency-
shifted critically-sampled lowpass channels at fs/M sample rate.

Output is a Python module containing:

    PROTO_COEFFS : list[int]   # length M*K, signed 16-bit
    M             : int        # number of branches
    K             : int        # taps per branch (so total = M*K)
    COEFF_WIDTH   : int        # bit width of stored values
    DESIGN_META   : dict       # human-readable design parameters

The HDL channelizer imports this module at compile time. Re-run after
any prototype design change; the HDL bake then picks up the new ROM.

Usage:
    python tools/polyphase_proto_design.py \
        --M 64 --taps-per-branch 6 \
        --output scanner-hdl/radio_core/polyphase_proto_coeffs.py

Optional `--plot` shows the prototype's magnitude response and the
overlap of two adjacent polyphase bin frequency responses for a
quick sanity check on the design.
"""

import argparse
import datetime
import sys

import numpy as np
import scipy.signal


def design_prototype(M: int, K: int, beta: float = 8.6) -> np.ndarray:
    """Design an M*K-tap lowpass prototype via Kaiser-windowed firwin.

    Cutoff is at 1/(2*M) of the input Nyquist rate so the passband
    width matches the polyphase bin spacing.

    Parameters
    ----------
    M : int
        Number of polyphase branches (= FFT size).
    K : int
        Number of taps per branch.  Total prototype length is M*K.
    beta : float
        Kaiser window beta. 8.6 gives ~80 dB stopband attenuation.

    Returns
    -------
    h : ndarray, float64, length M*K
        Prototype impulse response, normalized so peak |H(f)| = 1.
    """
    n_taps = M * K
    cutoff = 1.0 / M
    h = scipy.signal.firwin(
        n_taps, cutoff=cutoff, window=('kaiser', beta),
        pass_zero='lowpass', scale=False)
    h /= np.max(np.abs(np.fft.fft(h, 4 * n_taps)))
    return h


def quantize(h: np.ndarray, coeff_width: int) -> np.ndarray:
    """Quantize float coefficients to symmetric signed `coeff_width`-bit.

    Maps |h| <= max(|h|) to [-(2**(w-1)-1), 2**(w-1)-1] and rounds.
    The +/- range is symmetric (one less negative-side value than
    full two's-complement), avoiding the off-by-one peak-tap issue
    documented in feedback memory `feedback_maia_ddc_peak_scale`.
    """
    scale = (2 ** (coeff_width - 1)) - 1
    q = np.round(h / np.max(np.abs(h)) * scale).astype(int)
    return q


def write_module(path: str, q: np.ndarray, M: int, K: int,
                 coeff_width: int, design_args: dict) -> None:
    n_taps = M * K
    assert q.shape == (n_taps,)
    timestamp = datetime.datetime.now().strftime('%Y-%m-%d %H:%M:%S')
    coeffs_repr = ', '.join(str(int(c)) for c in q)
    body = f'''"""Prototype FIR coefficients for the P25 polyphase channelizer.

GENERATED FILE — do not edit by hand. Re-run
`tools/polyphase_proto_design.py` to regenerate.

Generated: {timestamp}
Design args: {design_args!r}
"""

# Polyphase parameters — must stay in sync with the HDL channelizer
# instantiation in p25_top.py.
M = {M}
K = {K}
COEFF_WIDTH = {coeff_width}

# Total prototype length is M * K. Ordering is natural time-domain
# (h[0] = first sample). The HDL polyphase decomposition reshapes
# this into M sub-filters via h_i[k] = PROTO_COEFFS[i + k*M].
PROTO_COEFFS = [
    {coeffs_repr}
]

assert len(PROTO_COEFFS) == M * K

DESIGN_META = {design_args!r}
'''
    with open(path, 'w', encoding='utf-8') as f:
        f.write(body)


def plot_response(h: np.ndarray, M: int) -> None:
    import matplotlib.pyplot as plt
    n_fft = 16 * len(h)
    H = np.fft.fftshift(np.fft.fft(h, n_fft))
    f = np.linspace(-0.5, 0.5, n_fft, endpoint=False)
    fig, axs = plt.subplots(2, 1, figsize=(10, 6))
    axs[0].plot(f, 20 * np.log10(np.abs(H) + 1e-12))
    axs[0].set_xlabel('normalized frequency (cycles/sample)')
    axs[0].set_ylabel('|H(f)| dB')
    axs[0].set_xlim(-1.5 / M, 1.5 / M)
    axs[0].set_ylim(-100, 5)
    axs[0].axvline(+0.5 / M, color='r', linestyle='--', alpha=0.4)
    axs[0].axvline(-0.5 / M, color='r', linestyle='--', alpha=0.4)
    axs[0].grid(True)
    axs[0].set_title(f'Prototype lowpass — M={M}, len={len(h)}')
    axs[1].plot(f, 20 * np.log10(np.abs(H) + 1e-12), label='bin 0')
    H_shift = np.fft.fftshift(
        np.fft.fft(h * np.exp(2j * np.pi * np.arange(len(h)) / M), n_fft))
    axs[1].plot(f, 20 * np.log10(np.abs(H_shift) + 1e-12), label='bin 1')
    axs[1].set_xlabel('normalized frequency (cycles/sample)')
    axs[1].set_ylabel('|H(f)| dB')
    axs[1].set_xlim(-2.0 / M, 2.0 / M)
    axs[1].set_ylim(-100, 5)
    axs[1].legend()
    axs[1].grid(True)
    axs[1].set_title('Adjacent bin overlap (passband-edge crossover)')
    plt.tight_layout()
    plt.show()


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--M', type=int, default=64,
                   help='polyphase branches / FFT size (power of 2)')
    p.add_argument('--taps-per-branch', type=int, default=6,
                   help='taps per polyphase branch')
    p.add_argument('--coeff-width', type=int, default=16,
                   help='quantization width (signed)')
    p.add_argument('--beta', type=float, default=8.6,
                   help='Kaiser window beta (8.6 ~ -80 dB stopband)')
    p.add_argument('--output', default='scanner-hdl/radio_core/polyphase_proto_coeffs.py',
                   help='destination Python module')
    p.add_argument('--plot', action='store_true',
                   help='show magnitude response')
    args = p.parse_args(argv)

    if args.M & (args.M - 1):
        p.error(f'--M must be a power of 2, got {args.M}')
    if args.M < 4 or args.M > 1024:
        p.error(f'--M out of practical range [4, 1024]')

    h = design_prototype(args.M, args.taps_per_branch, beta=args.beta)
    q = quantize(h, args.coeff_width)

    design_args = {
        'M': args.M,
        'K': args.taps_per_branch,
        'coeff_width': args.coeff_width,
        'kaiser_beta': args.beta,
        'cutoff_normalized': 1.0 / args.M,
        'tool': 'tools/polyphase_proto_design.py',
    }
    write_module(args.output, q, args.M, args.taps_per_branch,
                 args.coeff_width, design_args)
    print(f'wrote {args.output}: {len(q)} taps, '
          f'peak quantized = {int(np.max(np.abs(q)))}, '
          f'min = {int(np.min(q))}, max = {int(np.max(q))}')

    if args.plot:
        plot_response(h, args.M)


if __name__ == '__main__':
    sys.exit(main())
