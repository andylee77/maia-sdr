// System cards: a system's name, protocol and identity on one line, its details (location,
// county, type, voice) on the next, then a table of its sites (name, identity, control channel,
// the others it may move to, traffic channels). Every value is text that edits in place; the
// same card fills in a found system before it is added and edits a configured one.

import { h, iconButton } from '../dom.js';
import { protocol } from '../protocols.js';

const MHZ = hz => (hz / 1e6).toFixed(5).replace(/0+$/, '').replace(/\.$/, '');
export const hzText = list => (list || []).map(MHZ).join(', ');

// "454.36875, 451.0875" <-> Hz.
export function hzList(text, what) {
  return text.split(/[\s,;]+/).filter(Boolean).map(t => {
    const v = Math.round(Number(t) * 1e6);
    if (!Number.isFinite(v) || v < 70e6 || v > 6e9) throw new Error(`${what}: ${t} is not a frequency in MHz`);
    return v;
  });
}

// "5 = 454.36875" lines <-> {5: Hz}.
function lcnPlan(text) {
  const out = {};
  for (const line of text.split('\n').map(l => l.trim()).filter(Boolean)) {
    const m = line.match(/^(\d+)\s*[=:,]\s*([\d.]+)$/);
    if (!m) throw new Error(`"${line}" is not "LCN = MHz"`);
    out[m[1]] = hzList(m[2], `LCN ${m[1]}`)[0];
  }
  return out;
}

// A value that reads as text and edits in place; it widens with what is typed.
export function inline(value, label, { cls = '', min = 4, onInput } = {}) {
  const el = h('input', {
    class: `inline ${cls}`.trim(), type: 'text', value: value ?? '', placeholder: label,
    'aria-label': label, spellcheck: 'false', autocomplete: 'off',
  });
  const fit = () => { el.style.width = `calc(${Math.max((el.value || el.placeholder).length, min) + 0.5}ch + 10px)`; };
  el.addEventListener('input', () => {
    fit();
    if (onInput) onInput();
  });
  fit();
  return el;
}

function inlineSelect(options, value, label, onInput) {
  const el = h('select', { class: 'inline', 'aria-label': label }, ...options.map(([v, t]) => h('option', { value: v, text: t })));
  el.value = value;
  if (onInput) el.addEventListener('change', onInput);
  return el;
}

// An identity value as its protocol types it, its other spelling after it (RFSS "1 · 1").
function idValue(f, v, onInput) {
  if (f.options) {
    const el = inlineSelect([['', '?'], ...f.options.map(o => [o, o])], v ?? '', f.label, onInput);
    return { f, el, node: el };
  }
  const el = inline(v === null || v === undefined ? '' : f.show(v), f.label, { min: 1, onInput });
  if (!f.hint) return { f, el, node: el };
  const hint = h('span', { class: 'hex' });
  const show = () => {
    try {
      hint.textContent = el.value.trim() ? ` ${f.hint(f.parse(el.value.trim()))}` : '';
    } catch {
      hint.textContent = '';
    }
  };
  el.addEventListener('input', show);
  show();
  return { f, el, node: h('span', { class: 'nowrap' }, el, hint) };
}

// The typed identity over `base`; an emptied value is null.
function readIds(values, base) {
  const out = { ...(base || {}) };
  for (const { f, el } of values) {
    const t = el.value.trim();
    try {
      out[f.key] = !t ? null : f.options ? t : f.parse(t);
    } catch (e) {
      throw new Error(`${f.label} ${e.message}`);
    }
  }
  return out;
}

// A frequency list shown as wrapping text (`dim` ones in grey: heard, not configured); a click
// edits it.
function freqList(list, dim, label, onInput) {
  let text = hzText(list);
  let input = null;
  const span = h('span', { class: 'list', title: 'Click to edit' });
  const draw = () => {
    span.replaceChildren(...(list.length ? list.map((f, i) => h('span', { class: dim.includes(f) ? 'heard' : '', text: `${i ? ', ' : ''}${MHZ(f)}` })) : [h('span', { class: 'hex', text: label })]));
  };
  draw();
  span.addEventListener('click', () => {
    input = inline(text, label, { min: 12, onInput });
    span.replaceWith(input);
    input.focus();
  });
  return { node: span, value: () => (input ? input.value : text) };
}

// A system's head. `right`: nodes at the end of the first line. read() gives
// {label, identity, details}.
export function systemHead(sys, { right = [], onInput } = {}) {
  const p = protocol(sys.protocol);
  const d = sys.details || {};
  const name = inline(sys.label, 'System name', { cls: 'title', onInput });
  const ids = p.identityEdits.map(f => idValue(f, (sys.identity || {})[f.key], onInput));
  const details = [['location', 'Location'], ['county', 'County'], ['system_type', 'System type'], ['voice', 'System voice']]
    .map(([key, label]) => [key, inline(key === 'system_type' ? d.system_type ?? p.systemType : d[key], label, { cls: `d-${key}`, onInput })]);
  return {
    el: h('div', { class: 'sys-head' },
      h('div', { class: 'line' }, name, h('span', { class: 'badge', text: p.label }),
        h('span', { class: 'ids' }, ...ids.flatMap(x => [h('span', { class: 'k', text: x.f.label }), x.node])),
        h('span', { class: 'spacer' }), ...right),
      h('div', { class: 'line details' }, ...details.flatMap(([, el], i) => [i ? h('span', { class: 'sep', text: '·' }) : null, el]))),
    read: () => ({
      label: name.value,
      identity: readIds(ids, sys.identity),
      details: Object.fromEntries(details.map(([k, el]) => [k, el.value])),
    }),
  };
}

// A system's sites as a table: a line per site (name, identity, control channel, then `tail`),
// and under it a line of its other frequencies (the control channels it may move to, its
// traffic channels). Each of `rows`: {site, lead (before the name: a tick), heard (traffic
// channels granted on the air), tail (reception, status, buttons)}. `traffic` shows the
// traffic channels; `settings` adds each site's receiver settings behind a button. Each row's
// read() gives the site as the API takes it.
export function siteTable(proto, rows, { traffic = true, settings = false, onInput } = {}) {
  const p = protocol(proto);
  const leads = rows.some(r => r.lead);
  // [heading, width class]: every table has the same columns, so tables line up.
  const cols = [
    ...(leads ? [['', 'c-tick']] : []), ['Name', 'c-name'], ...p.siteIdentityEdits.map(f => [f.label, 'c-id']),
    ['Control', 'c-freq'], ...(p.edits.controlSlot ? [['TS', 'c-ts']] : []), ['', 'c-end'],
  ];
  const out = rows.map(r => siteRow(p, r, { leads, traffic, settings, onInput, span: cols.length }));
  return {
    el: h('div', { class: 'sites-wrap' }, h('table', { class: 'sites' },
      h('colgroup', null, ...cols.map(([, cls]) => h('col', { class: cls }))),
      h('thead', null, h('tr', null, ...cols.map(([c]) => h('th', { text: c })))),
      h('tbody', null, ...out.flatMap(x => x.trs)))),
    rows: out,
  };
}

function siteRow(p, { site, lead = null, heard = [], tail = [] }, { leads, traffic, settings, onInput, span }) {
  let edited = false;
  const changed = () => {
    edited = true;
    if (onInput) onInput();
  };
  const name = inline(site.label, 'Site name', { onInput: changed });
  const ids = p.siteIdentityEdits.map(f => idValue(f, (site.identity || {})[f.key], changed));
  const control = inline(site.control.freq_hz ? MHZ(site.control.freq_hz) : '', 'MHz', { min: 8, onInput: changed });
  const also = freqList(site.control.alternates_hz || [], [], 'none', changed);
  const slot = inlineSelect([['', '?'], ['1', '1'], ['2', '2']], site.control.timeslot ? String(site.control.timeslot) : '', 'Control timeslot', changed);
  const configured = site.channels_hz || [];
  const extra = heard.filter(f => !configured.includes(f));
  const channels = freqList([...configured, ...extra].sort((a, b) => a - b), extra, 'none yet', changed);

  const more = settings ? siteSettings(p, site, changed) : null;
  const moreRow = more ? h('tr', { class: 'more', hidden: true }, h('td', { colspan: String(span) }, more.el)) : null;
  const moreButton = more ? iconButton('tune', 'Settings', { onclick: () => { moreRow.hidden = !moreRow.hidden; } }) : null;

  const tr = h('tr', { class: 'site' },
    ...(leads ? [h('td', null, lead)] : []), h('td', null, name), ...ids.map(x => h('td', null, x.node)),
    h('td', null, control),
    ...(p.edits.controlSlot ? [h('td', null, slot)] : []),
    h('td', { class: 'end' }, ...tail, moreButton));
  const freqs = h('tr', { class: 'freqs' },
    ...(leads ? [h('td')] : []),
    h('td', { colspan: String(span - (leads ? 1 : 0)) },
      h('span', { class: 'k', text: 'Also' }), also.node,
      ...(traffic ? [h('span', { class: 'k', text: 'Traffic' }), channels.node] : [])));
  return {
    trs: [tr, freqs, ...(moreRow ? [moreRow] : [])],
    site,
    edited: () => edited,
    read() {
      const ctl = hzList(control.value, `${name.value}: control channel`);
      if (ctl.length !== 1) throw new Error(`${name.value || 'A site'}: one control channel (the others go in Also)`);
      return {
        label: name.value,
        identity: readIds(ids, site.identity),
        control: {
          freq_hz: ctl[0],
          alternates_hz: hzList(also.value(), `${name.value}: also`),
          lcn: more ? more.lcn() : site.control.lcn ?? null,
          timeslot: p.edits.controlSlot && slot.value ? Number(slot.value) : site.control.timeslot ?? null,
        },
        channels_hz: traffic ? hzList(channels.value(), `${name.value}: traffic`) : site.channels_hz || [],
        ...(more ? more.read() : {}),
      };
    },
  };
}

// A configured site's receiver settings: the receive window, and what its protocol lets the user
// set (the modulation, the control LCN and the channel plan).
function siteSettings(p, site, onInput) {
  const modulation = inlineSelect([['auto', 'automatic'], ['lsm', 'LSM'], ['c4fm', 'C4FM']], site.modulation || 'auto', 'Modulation', onInput);
  const auto = h('input', { type: 'checkbox', 'aria-label': 'Move the window to the busiest channels' });
  auto.checked = site.window ? site.window.auto : true;
  auto.addEventListener('change', onInput);
  const position = inlineSelect([['center', 'centre'], ['top', 'top'], ['bottom', 'bottom']], (site.window && site.window.cc_position) || 'center', 'Control channel in the window', onInput);
  const lcn = inline(site.control.lcn ?? '', 'LCN', { min: 3, onInput });
  const plan = h('textarea', { class: 'input', rows: 3, cols: 28, 'aria-label': 'LCN plan', placeholder: '5 = 454.36875' });
  plan.value = Object.entries((site.channel_plan && site.channel_plan.lcn_hz) || {}).map(([k, v]) => `${k} = ${MHZ(v)}`).join('\n');
  plan.addEventListener('input', onInput);
  const el = h('div', { class: 'scan-settings' },
    ...(p.edits.modulation ? [h('span', { text: 'Modulation' }), modulation] : []),
    h('label', { class: 'row' }, auto, h('span', { text: 'Move the window to the busiest channels,' })),
    h('span', { text: 'control channel at the' }), position,
    ...(p.edits.controlSlot ? [h('span', { text: 'Control LCN' }), lcn] : []),
    ...(p.edits.lcnPlan ? [h('span', { text: 'Channel plan (one "LCN = MHz" a line)' }), plan] : []));
  return {
    el,
    lcn: () => (p.edits.controlSlot && String(lcn.value).trim() !== '' ? Number(lcn.value) : site.control.lcn ?? null),
    read: () => ({
      modulation: p.edits.modulation ? modulation.value : site.modulation || 'auto',
      channel_plan: p.edits.lcnPlan ? { lcn_hz: lcnPlan(plan.value) } : site.channel_plan || null,
      window: { ...(site.window || {}), auto: auto.checked, cc_position: position.value },
      notes: site.notes || [],
    }),
  };
}

// The traffic channels a site's learned state has seen granted.
export function heardTraffic(learned, controlHz) {
  return Object.keys((learned && learned.grants) || {}).map(Number).filter(f => f && f !== controlHz).sort((a, b) => a - b);
}
