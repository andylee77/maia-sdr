#
# Fishball hardware validation (hwval) - AXI4-Lite manager BFM for tests
#
# SPDX-License-Identifier: MIT
#

"""Minimal AXI4-Lite manager for Amaranth 0.5 async testbenches.

Every helper raises ``AxiTimeout`` instead of hanging when the subordinate
never completes a handshake, so tests can assert "the bus never hangs".
"""


class AxiTimeout(Exception):
    pass


async def axil_write(ctx, axi, addr, data, *, strb=0xF, domain='sync',
                     aw_delay=0, w_delay=0, b_delay=0, timeout=200):
    """Write one word. Returns BRESP.

    ``aw_delay``/``w_delay`` delay the assertion of AWVALID/WVALID (in
    cycles) so W can come before AW and vice versa; ``b_delay`` delays
    BREADY.
    """
    aw_done = w_done = False
    ctx.set(axi.awaddr, addr)
    ctx.set(axi.wdata, data)
    ctx.set(axi.wstrb, strb)
    for cycle in range(timeout):
        aw_active = not aw_done and cycle >= aw_delay
        w_active = not w_done and cycle >= w_delay
        b_ready = cycle >= b_delay
        ctx.set(axi.awvalid, aw_active)
        ctx.set(axi.wvalid, w_active)
        ctx.set(axi.bready, b_ready)
        aw_hs = aw_active and ctx.get(axi.awready)
        w_hs = w_active and ctx.get(axi.wready)
        b_hs = b_ready and ctx.get(axi.bvalid)
        bresp = ctx.get(axi.bresp)
        if b_hs:
            assert aw_done and w_done, 'BVALID before AW and W handshakes'
        await ctx.tick(domain)
        aw_done |= bool(aw_hs)
        w_done |= bool(w_hs)
        if b_hs:
            ctx.set(axi.awvalid, 0)
            ctx.set(axi.wvalid, 0)
            ctx.set(axi.bready, 0)
            return bresp
    ctx.set(axi.awvalid, 0)
    ctx.set(axi.wvalid, 0)
    ctx.set(axi.bready, 0)
    raise AxiTimeout(f'write to {addr:#x} did not complete')


async def axil_read(ctx, axi, addr, *, domain='sync', ar_delay=0,
                    r_delay=0, timeout=200, with_resp=False):
    """Read one word. Returns RDATA (or (RDATA, RRESP))."""
    ar_done = False
    ctx.set(axi.araddr, addr)
    for cycle in range(timeout):
        ar_active = not ar_done and cycle >= ar_delay
        r_ready = ar_done and cycle >= r_delay
        ctx.set(axi.arvalid, ar_active)
        ctx.set(axi.rready, r_ready)
        ar_hs = ar_active and ctx.get(axi.arready)
        r_hs = r_ready and ctx.get(axi.rvalid)
        rdata = ctx.get(axi.rdata)
        rresp = ctx.get(axi.rresp)
        await ctx.tick(domain)
        ar_done |= bool(ar_hs)
        if r_hs:
            ctx.set(axi.arvalid, 0)
            ctx.set(axi.rready, 0)
            return (rdata, rresp) if with_resp else rdata
    ctx.set(axi.arvalid, 0)
    ctx.set(axi.rready, 0)
    raise AxiTimeout(f'read from {addr:#x} did not complete')
