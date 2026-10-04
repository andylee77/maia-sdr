#
# Fishball hardware validation (hwval) - AXI4-Lite register file
#
# Declarative register table + an AXI4-Lite subordinate that always
# completes transactions. See doc/HW_VALIDATION_SUITE.md section 6.3.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
import amaranth.back.verilog

import json
from typing import Dict, List, Optional
import xml.etree.ElementTree as ET

from maia_hdl import axi


ACCESS_TYPES = ('ro', 'rw', 'wo', 'w1c')

# Value returned by reads of unmapped addresses.
UNMAPPED_READ_VALUE = 0xDEAD_BEEF

JSON_SCHEMA = 'fbench.regmap/1'


class RegField:
    """Bit field of a register.

    Parameters
    ----------
    name : str
        Field name (lower case by convention).
    lsb : int
        Position of the least significant bit.
    width : int
        Field width in bits.
    desc : str
        Human readable description.
    """
    def __init__(self, name: str, lsb: int, width: int = 1, desc: str = ''):
        if lsb < 0 or width < 1:
            raise ValueError(f'invalid field {name}: lsb={lsb} width={width}')
        self.name = name
        self.lsb = lsb
        self.width = width
        self.desc = desc

    @property
    def msb(self):
        return self.lsb + self.width - 1

    def __repr__(self):
        return f'RegField({self.name!r}, {self.lsb}, {self.width})'


class Reg:
    """Register definition.

    Parameters
    ----------
    name : str
        Register name. Names are the contract with the software (JSON map,
        agent, CLI), so they are upper case and unique in the whole table.
    access : str
        One of ``'ro'`` (read-only: the value is driven by the hardware),
        ``'rw'`` (read-write storage held by the register file), ``'wo'``
        (write-only: a write produces a one-cycle pulse carrying the written
        value, reads return 0) or ``'w1c'`` (write-one-to-clear: a write
        produces a one-cycle pulse of the written ones, which the owner uses
        to clear bits; reads return the ``rvalue`` input, 0 by default).
    width : int
        Number of implemented bits (1 to 32). Reads are zero-extended to 32
        bits. Bus accesses are always full 32-bit words.
    reset : int
        Reset value of ``rw`` storage. For constant ``ro`` registers it is
        the constant value.
    fields : Optional[List[RegField]]
        Bit fields. When omitted, the register has a single field called
        ``value`` covering all the implemented bits.
    desc : str
        Human readable description.
    snapshot : Optional[str]
        Name of the snapshot domain that this register is read through
        (``None`` for registers that live in the AXI-Lite domain or are
        synchronized live).
    const : Optional[int]
        For ``ro`` registers only: constant read value (served by the
        register file, the top level does not need to drive it).
    offset : Optional[int]
        Byte offset inside the block. When omitted, the next free word of
        the block is assigned (declaration order).
    """
    def __init__(self, name: str, access: str, *, width: int = 32,
                 reset: int = 0, fields: Optional[List[RegField]] = None,
                 desc: str = '', snapshot: Optional[str] = None,
                 const: Optional[int] = None,
                 offset: Optional[int] = None):
        if access not in ACCESS_TYPES:
            raise ValueError(f'register {name}: invalid access {access!r}')
        if not 1 <= width <= 32:
            raise ValueError(f'register {name}: invalid width {width}')
        if const is not None:
            if access != 'ro':
                raise ValueError(f'register {name}: const requires ro')
            reset = const
        if reset < 0 or reset >= 1 << width:
            raise ValueError(f'register {name}: reset {reset:#x} does not '
                             f'fit in {width} bits')
        self.name = name
        self.access = access
        self.width = width
        self.reset = reset
        self.desc = desc
        self.snapshot = snapshot
        self.const = const
        self.offset = offset
        self.block = None
        if fields is None:
            fields = [RegField('value', 0, width, desc)]
        self.fields = list(fields)
        used = 0
        for f in self.fields:
            if f.msb >= width:
                raise ValueError(f'register {name}: field {f.name} exceeds '
                                 f'the register width {width}')
            mask = ((1 << f.width) - 1) << f.lsb
            if used & mask:
                raise ValueError(f'register {name}: field {f.name} overlaps')
            used |= mask
        if len({f.name for f in self.fields}) != len(self.fields):
            raise ValueError(f'register {name}: duplicate field names')

    @property
    def readable(self):
        return self.access in ('ro', 'rw', 'w1c')

    @property
    def writable(self):
        return self.access in ('rw', 'wo', 'w1c')

    def field(self, name: str) -> RegField:
        for f in self.fields:
            if f.name == name:
                return f
        raise KeyError(f'register {self.name} has no field {name}')

    def __repr__(self):
        return f'Reg({self.name!r}, {self.access!r})'


class RegBlock:
    """Block of registers (a fixed-size window of the register space).

    Parameters
    ----------
    name : str
        Block name (lower case), used as the JSON block name.
    offset : int
        Byte offset of the block from the core base.
    regs : List[Reg]
        Registers of the block.
    desc : str
        Human readable description.
    """
    def __init__(self, name: str, offset: int, regs: List[Reg] = (),
                 desc: str = ''):
        self.name = name
        self.offset = offset
        self.desc = desc
        self.regs = []
        for reg in regs:
            self.add(reg)

    def add(self, reg: Reg) -> Reg:
        reg.block = self
        self.regs.append(reg)
        return reg


class RegisterTable:
    """Complete register table of a core.

    This is the single source of truth for the register map. It assigns
    offsets, validates the layout and emits the JSON map (``to_json``),
    a CMSIS-SVD file (``to_svd``) and a Markdown document
    (``to_markdown``).

    Parameters
    ----------
    core : str
        Core name (``'hwval'``).
    base : int
        Physical base address of the AXI-Lite window.
    size : int
        Size of the AXI-Lite window in bytes (power of two).
    blocks : List[RegBlock]
        Register blocks.
    id_reg : str
        Name of the identification register.
    id_value : int
        Expected value of the identification register.
    version : str
        Core version string.
    snapshot_domains : Dict[str, int]
        Snapshot domain name to SNAP_REQ/SNAP_ACK bit mask.
    block_size : int
        Size of each block in bytes.
    """
    word_bytes = 4

    def __init__(self, core: str, base: int, size: int,
                 blocks: List[RegBlock], *, id_reg: str, id_value: int,
                 version: str, snapshot_domains: Dict[str, int],
                 block_size: int = 0x100, description: str = ''):
        if size & (size - 1):
            raise ValueError('size must be a power of two')
        if block_size & (block_size - 1) or block_size < 4:
            raise ValueError('block_size must be a power of two')
        self.core = core
        self.base = base
        self.size = size
        self.blocks = list(blocks)
        self.id_reg = id_reg
        self.id_value = id_value
        self.version = version
        self.snapshot_domains = dict(snapshot_domains)
        self.block_size = block_size
        self.description = description
        self._assign_offsets()
        self._validate()

    # ── Layout ──────────────────────────────────────────────────
    @property
    def address_bits(self):
        return (self.size - 1).bit_length()

    def _assign_offsets(self):
        for block in self.blocks:
            used = {r.offset for r in block.regs if r.offset is not None}
            nxt = 0
            for reg in block.regs:
                if reg.offset is not None:
                    continue
                while nxt in used:
                    nxt += self.word_bytes
                reg.offset = nxt
                used.add(nxt)
                nxt += self.word_bytes

    def _validate(self):
        names = set()
        block_names = set()
        block_offsets = set()
        for block in self.blocks:
            if block.name in block_names:
                raise ValueError(f'duplicate block {block.name}')
            block_names.add(block.name)
            if block.offset % self.block_size:
                raise ValueError(f'block {block.name} is not aligned')
            if block.offset + self.block_size > self.size:
                raise ValueError(f'block {block.name} is outside the window')
            if block.offset in block_offsets:
                raise ValueError(f'block {block.name} overlaps')
            block_offsets.add(block.offset)
            offsets = set()
            for reg in block.regs:
                if reg.name in names:
                    raise ValueError(f'duplicate register {reg.name}')
                names.add(reg.name)
                if reg.offset % self.word_bytes:
                    raise ValueError(f'register {reg.name} is not aligned')
                if not 0 <= reg.offset < self.block_size:
                    raise ValueError(f'register {reg.name} is outside its '
                                     f'block')
                if reg.offset in offsets:
                    raise ValueError(f'register {reg.name} overlaps')
                offsets.add(reg.offset)
                if (reg.snapshot is not None
                        and reg.snapshot not in self.snapshot_domains):
                    raise ValueError(f'register {reg.name}: unknown '
                                     f'snapshot domain {reg.snapshot}')
        if self.id_reg not in names:
            raise ValueError(f'id register {self.id_reg} not in the table')

    def registers(self):
        """Iterate over all the registers in address order."""
        for block in sorted(self.blocks, key=lambda b: b.offset):
            for reg in sorted(block.regs, key=lambda r: r.offset):
                yield reg

    def address(self, reg) -> int:
        """Byte offset of a register from the core base."""
        if isinstance(reg, str):
            reg = self[reg]
        return reg.block.offset + reg.offset

    def __getitem__(self, name: str) -> Reg:
        for block in self.blocks:
            for reg in block.regs:
                if reg.name == name:
                    return reg
        raise KeyError(name)

    def __contains__(self, name: str) -> bool:
        try:
            self[name]
        except KeyError:
            return False
        return True

    def block(self, name: str) -> RegBlock:
        for block in self.blocks:
            if block.name == name:
                return block
        raise KeyError(name)

    # ── JSON ────────────────────────────────────────────────────
    def to_dict(self):
        def hex3(v):
            return f'0x{v:03X}'
        blocks = []
        for block in sorted(self.blocks, key=lambda b: b.offset):
            regs = []
            for reg in sorted(block.regs, key=lambda r: r.offset):
                regs.append({
                    'name': reg.name,
                    'offset': hex3(self.address(reg)),
                    'access': reg.access,
                    'width': reg.width,
                    'reset': f'0x{reg.reset:X}',
                    'snapshot': reg.snapshot,
                    'desc': reg.desc,
                    'fields': [
                        {'name': f.name, 'lsb': f.lsb, 'width': f.width,
                         'desc': f.desc}
                        for f in sorted(reg.fields, key=lambda f: f.lsb)],
                })
            blocks.append({
                'name': block.name,
                'offset': hex3(block.offset),
                'regs': regs,
            })
        return {
            'schema': JSON_SCHEMA,
            'core': self.core,
            'base': f'0x{self.base:08X}',
            'size': self.size,
            'id_reg': self.id_reg,
            'id_value': f'0x{self.id_value:08X}',
            'version': self.version,
            'snapshot_domains': dict(self.snapshot_domains),
            'blocks': blocks,
        }

    def to_json(self, indent: Optional[int] = 2) -> str:
        return json.dumps(self.to_dict(), indent=indent) + '\n'

    # ── SVD ─────────────────────────────────────────────────────
    def to_svd(self, metadata: Optional[Dict[str, str]] = None,
               base_address: int = 0) -> bytes:
        """CMSIS-SVD description (same layout as maia_hdl.register).

        ``base_address`` defaults to 0 like the Maia SDR and P25 PACs,
        which are used on top of a UIO/devmem mapping of the window.
        """
        meta = {
            'vendor': 'Andy Lee',
            'vendorID': 'fishball-hwval',
            'name': self.core.upper(),
            'series': 'Fishball hwval',
            'version': self.version,
            'description': (self.description
                             or f'Fishball {self.core} IP core'),
            'licenseText': 'SPDX-License-Identifier: MIT',
        }
        if metadata:
            meta.update(metadata)
        access_map = {
            'ro': 'read-only',
            'rw': 'read-write',
            'wo': 'write-only',
            'w1c': 'read-write',
        }
        device = ET.Element('device')
        device.set('schemaVersion', '1.1')
        device.set('xmlns:xs', 'http://www.w3.org/2001/XMLSchema-instance')
        device.set('xs:noNamespaceSchemaLocation', 'CMSIS-SVD.xsd')
        for key in ['vendor', 'vendorID', 'name', 'series', 'version',
                    'description', 'licenseText']:
            el = ET.SubElement(device, key)
            el.text = meta[key]
        for element in ['width', 'size']:
            el = ET.SubElement(device, element)
            el.text = '32'
        peripherals = ET.SubElement(device, 'peripherals')
        peripheral = ET.SubElement(peripherals, 'peripheral')
        for key in ['name', 'version', 'description']:
            el = ET.SubElement(peripheral, key)
            el.text = meta[key]
        el = ET.SubElement(peripheral, 'baseAddress')
        el.text = f'0x{base_address:08x}'
        el = ET.SubElement(peripheral, 'access')
        el.text = 'read-write'
        address_block = ET.SubElement(peripheral, 'addressBlock')
        el = ET.SubElement(address_block, 'offset')
        el.text = '0'
        el = ET.SubElement(address_block, 'size')
        el.text = f'0x{self.size:x}'
        el = ET.SubElement(address_block, 'usage')
        el.text = 'registers'
        registers = ET.SubElement(peripheral, 'registers')
        for reg in self.registers():
            r = ET.SubElement(registers, 'register')
            el = ET.SubElement(r, 'name')
            el.text = reg.name
            el = ET.SubElement(r, 'description')
            el.text = reg.desc or reg.name
            el = ET.SubElement(r, 'addressOffset')
            el.text = f'0x{self.address(reg):x}'
            el = ET.SubElement(r, 'size')
            el.text = '32'
            el = ET.SubElement(r, 'access')
            el.text = access_map[reg.access]
            el = ET.SubElement(r, 'resetValue')
            el.text = f'0x{reg.reset:08x}'
            el = ET.SubElement(r, 'resetMask')
            el.text = '0xffffffff'
            fields = ET.SubElement(r, 'fields')
            for field in sorted(reg.fields, key=lambda f: f.lsb):
                f = ET.SubElement(fields, 'field')
                el = ET.SubElement(f, 'name')
                el.text = field.name
                el = ET.SubElement(f, 'description')
                el.text = field.desc or field.name
                el = ET.SubElement(f, 'bitRange')
                el.text = f'[{field.msb}:{field.lsb}]'
                el = ET.SubElement(f, 'access')
                el.text = access_map[reg.access]
                if reg.access == 'w1c':
                    el = ET.SubElement(f, 'modifiedWriteValues')
                    el.text = 'oneToClear'
        ET.indent(device, space=' ' * 2, level=0)
        return (b'<?xml version="1.0" encoding="utf-8"?>\n'
                + ET.tostring(device) + b'\n')

    # ── Markdown ────────────────────────────────────────────────
    def to_markdown(self, title: Optional[str] = None,
                    preamble: str = '') -> str:
        def esc(text):
            return (text or '').replace('|', '\\|').replace('\n', ' ')

        def bits(f):
            return (f'[{f.lsb}]' if f.width == 1
                    else f'[{f.msb}:{f.lsb}]')

        out = []
        out.append(f'# {title or self.core + " register map"}')
        out.append('')
        if preamble:
            out.append(preamble.strip())
            out.append('')
        out.append(f'- Core: `{self.core}`, version `{self.version}`')
        out.append(f'- AXI-Lite base: `0x{self.base:08X}`, size '
                   f'{self.size} bytes; offsets below are from the base')
        out.append(f'- Identification: `{self.id_reg}` reads '
                   f'`0x{self.id_value:08X}`')
        doms = ', '.join(f'`{k}` = {v:#x}'
                         for k, v in self.snapshot_domains.items())
        out.append(f'- Snapshot domains (`SNAP_REQ` / `SNAP_ACK` bits): '
                   f'{doms}')
        out.append(f'- Unmapped addresses read `0x{UNMAPPED_READ_VALUE:08X}`'
                   f'; writes to them are ignored; every access completes '
                   f'with OKAY')
        out.append('- Access: `ro` read-only, `rw` read-write, `wo` '
                   'write-only (one-cycle pulse, reads 0), `w1c` '
                   'write-one-to-clear')
        out.append('')
        out.append('## Blocks')
        out.append('')
        out.append('| Offset | Block | Registers | Description |')
        out.append('|---|---|---|---|')
        for block in sorted(self.blocks, key=lambda b: b.offset):
            out.append(f'| `0x{block.offset:03X}` | `{block.name}` | '
                       f'{len(block.regs)} | {esc(block.desc)} |')
        out.append('')
        for block in sorted(self.blocks, key=lambda b: b.offset):
            out.append(f'## Block `{block.name}` (0x{block.offset:03X})')
            out.append('')
            if block.desc:
                out.append(esc(block.desc))
                out.append('')
            out.append('| Offset | Register | Access | Width | Reset | '
                       'Snapshot | Description |')
            out.append('|---|---|---|---|---|---|---|')
            regs = sorted(block.regs, key=lambda r: r.offset)
            for reg in regs:
                snap = reg.snapshot or '-'
                reset = ('-' if reg.access == 'ro' and reg.const is None
                         else f'`0x{reg.reset:X}`')
                out.append(
                    f'| `0x{self.address(reg):03X}` | `{reg.name}` | '
                    f'{reg.access} | {reg.width} | {reset} | {snap} | '
                    f'{esc(reg.desc)} |')
            out.append('')
            multi = [r for r in regs
                     if len(r.fields) > 1 or r.fields[0].name != 'value']
            if multi:
                out.append('Fields:')
                out.append('')
                out.append('| Register | Field | Bits | Description |')
                out.append('|---|---|---|---|')
                for reg in multi:
                    for f in sorted(reg.fields, key=lambda f: f.lsb):
                        out.append(f'| `{reg.name}` | `{f.name}` | '
                                   f'{bits(f)} | {esc(f.desc)} |')
                out.append('')
        return '\n'.join(out).rstrip('\n') + '\n'


class AxiLiteRegisterFile(Elaboratable):
    """AXI4-Lite subordinate implementing a ``RegisterTable``.

    The bridge always completes transactions: one outstanding read and one
    outstanding write are accepted at a time, every response is OKAY,
    unmapped reads return ``0xDEADBEEF`` and unmapped writes are ignored.
    AW and W are accepted independently (in any order). Reads have no side
    effects. The read path is pipelined (address register, per-block
    multiplexer register, block select register), so read data arrives three
    cycles after the AR handshake.

    The register file runs in the ``sync`` domain; use a ``DomainRenamer``
    to place it in the AXI-Lite clock domain.

    Parameters
    ----------
    table : RegisterTable
        Register table to implement.
    name : Optional[str]
        Prefix of the AXI4-Lite pin names (``'s_axi_lite'``).

    Attributes
    ----------
    axi : AxiInterface
        AXI4-Lite subordinate interface (32-bit data, byte addresses).
    value : Dict[str, Signal]
        Per register value. ``ro``: input driven by the owner (ignored for
        constant registers). ``rw``: output with the stored value. ``wo``:
        output, the written value (masked by WSTRB) for one cycle, 0
        otherwise. ``w1c``: output, the written ones for one cycle.
        Also accessible as ``regs[name]``.
    wstb : Dict[str, Signal]
        Per writable register, one-cycle pulse when it is written. For
        ``rw`` registers it is asserted in the same cycle as the new value.
    inhibit : Dict[str, Signal]
        Per ``rw`` register, input: when asserted writes are ignored.
    clear : Dict[str, Signal]
        Per ``rw`` register, input: when asserted the storage returns to
        its reset value (takes precedence over writes).
    rvalue : Dict[str, Signal]
        Per ``w1c`` register, input: value returned by reads.
    """
    def __init__(self, table: RegisterTable, name: Optional[str] = None):
        self.table = table
        self.aw = table.address_bits
        self.axi = axi.AxiInterface(
            axi.AxiDevice.SUBORDINATE,
            [axi.AxiChannel(axi.AxiDirection.READ, self.aw, 32),
             axi.AxiChannel(axi.AxiDirection.WRITE, self.aw, 32)],
            axi.AxiVersion.AXI4LITE,
            name=name)

        self.value = {}
        self.wstb = {}
        self.inhibit = {}
        self.clear = {}
        self.rvalue = {}
        for reg in table.registers():
            lname = reg.name.lower()
            init = reg.reset if reg.access == 'rw' else 0
            self.value[reg.name] = Signal(reg.width, name=f'reg_{lname}',
                                          init=init)
            if reg.writable:
                self.wstb[reg.name] = Signal(name=f'reg_{lname}_wstb')
            if reg.access == 'rw':
                self.inhibit[reg.name] = Signal(name=f'reg_{lname}_inhibit')
                self.clear[reg.name] = Signal(name=f'reg_{lname}_clear')
            if reg.access == 'w1c':
                self.rvalue[reg.name] = Signal(reg.width,
                                               name=f'reg_{lname}_rvalue')

    def __getitem__(self, name: str) -> Signal:
        return self.value[name]

    def field(self, reg_name: str, field_name: str) -> Value:
        """Slice of a register value corresponding to a field."""
        f = self.table[reg_name].field(field_name)
        return self.value[reg_name][f.lsb:f.lsb + f.width]

    def ports(self):
        ports = list(self.axi.ports())
        for reg in self.table.registers():
            ports.append(self.value[reg.name])
            if reg.name in self.wstb:
                ports.append(self.wstb[reg.name])
            if reg.name in self.inhibit:
                ports.append(self.inhibit[reg.name])
                ports.append(self.clear[reg.name])
            if reg.name in self.rvalue:
                ports.append(self.rvalue[reg.name])
        return ports

    def _read_value(self, reg: Reg) -> Value:
        if reg.const is not None:
            return C(reg.const, 32)
        if reg.access in ('ro', 'rw'):
            return self.value[reg.name]
        if reg.access == 'w1c':
            return self.rvalue[reg.name]
        return C(0, 32)

    def elaborate(self, platform):
        m = Module()
        a = self.axi
        word_bits = self.aw - 2
        wib = (self.table.block_size // 4 - 1).bit_length()  # word-in-block
        blk_bits = word_bits - wib

        # ── Write channel ──────────────────────────────────────
        aw_full = Signal()
        w_full = Signal()
        waddr = Signal(word_bits, reset_less=True)
        wdata = Signal(32, reset_less=True)
        wstrb = Signal(4, reset_less=True)
        do_write = Signal()
        m.d.comb += [
            a.awready.eq(~aw_full),
            a.wready.eq(~w_full),
            do_write.eq(aw_full & w_full & ~a.bvalid),
            a.bresp.eq(axi.AxiResp.OKAY.value),
        ]
        with m.If(a.aw_handshake()):
            m.d.sync += [
                aw_full.eq(1),
                waddr.eq(a.awaddr[2:]),
            ]
        with m.If(a.w_handshake()):
            m.d.sync += [
                w_full.eq(1),
                wdata.eq(a.wdata),
                wstrb.eq(a.wstrb),
            ]
        with m.If(do_write):
            m.d.sync += [
                aw_full.eq(0),
                w_full.eq(0),
                a.bvalid.eq(1),
            ]
        with m.Elif(a.b_handshake()):
            m.d.sync += a.bvalid.eq(0)

        bytemask = Signal(32)
        m.d.comb += bytemask.eq(Cat(*[wstrb[j].replicate(8)
                                      for j in range(4)]))
        masked = wdata & bytemask

        for reg in self.table.registers():
            if not reg.writable:
                continue
            word = self.table.address(reg) >> 2
            hit = Signal(name=f'hit_{reg.name.lower()}')
            m.d.comb += hit.eq(do_write & (waddr == word))
            val = self.value[reg.name]
            wstb = self.wstb[reg.name]
            w = reg.width
            if reg.access == 'rw':
                inhibit = self.inhibit[reg.name]
                m.d.sync += wstb.eq(0)
                with m.If(self.clear[reg.name]):
                    m.d.sync += val.eq(reg.reset)
                with m.Elif(hit & ~inhibit):
                    m.d.sync += [
                        val.eq((val & ~bytemask[:w]) | masked[:w]),
                        wstb.eq(1),
                    ]
            else:
                # wo / w1c: one-cycle pulse carrying the written value
                m.d.sync += [
                    val.eq(Mux(hit, masked[:w], 0)),
                    wstb.eq(hit),
                ]

        # ── Read channel ───────────────────────────────────────
        ar_busy = Signal()
        raddr = Signal(word_bits, reset_less=True)
        rd_s1 = Signal()
        rd_s2 = Signal()
        blk_q = Signal(max(blk_bits, 1), reset_less=True)
        m.d.comb += [
            a.arready.eq(~ar_busy),
            a.rresp.eq(axi.AxiResp.OKAY.value),
        ]
        m.d.sync += [
            rd_s1.eq(a.ar_handshake()),
            rd_s2.eq(rd_s1),
        ]
        with m.If(a.ar_handshake()):
            m.d.sync += [
                ar_busy.eq(1),
                raddr.eq(a.araddr[2:]),
            ]

        # Stage 1: per-block multiplexers
        blk_data = {}
        for block in sorted(self.table.blocks, key=lambda b: b.offset):
            bd = Signal(32, name=f'rd_{block.name}', reset_less=True)
            blk_data[block.offset // self.table.block_size] = bd
            with m.If(rd_s1):
                with m.Switch(raddr[:wib]):
                    for reg in sorted(block.regs, key=lambda r: r.offset):
                        with m.Case(reg.offset // 4):
                            m.d.sync += bd.eq(self._read_value(reg))
                    with m.Default():
                        m.d.sync += bd.eq(UNMAPPED_READ_VALUE)
        with m.If(rd_s1):
            m.d.sync += blk_q.eq(raddr[wib:] if blk_bits else 0)

        # Stage 2: block select
        with m.If(rd_s2):
            with m.Switch(blk_q):
                for index, bd in blk_data.items():
                    with m.Case(index):
                        m.d.sync += a.rdata.eq(bd)
                with m.Default():
                    m.d.sync += a.rdata.eq(UNMAPPED_READ_VALUE)
            m.d.sync += a.rvalid.eq(1)
        with m.Elif(a.r_handshake()):
            m.d.sync += [
                a.rvalid.eq(0),
                ar_busy.eq(0),
            ]

        return m


if __name__ == '__main__':
    table = RegisterTable(
        'example', 0x4000_0000, 4096,
        [RegBlock('id', 0, [Reg('ID', 'ro', const=0x1234_5678),
                            Reg('SCRATCH', 'rw')])],
        id_reg='ID', id_value=0x1234_5678, version='0.0.0',
        snapshot_domains={})
    regs = AxiLiteRegisterFile(table, name='s_axi_lite')
    print(amaranth.back.verilog.convert(regs, ports=regs.ports()))
