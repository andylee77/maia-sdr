// Change 071: "Systems" — find the P25 systems this radio can hear and
// turn them into sites. A sweep steps the receiver across the bands,
// probes every continuous carrier with both control decoders (LSM and
// C4FM) and reads each site's identity, band table and neighbours. The
// radio is busy meanwhile (calls are not followed) and returns to the
// active site at the end.

import { h, setText, card, toast } from '../dom.js';
import { api } from '../api.js';
import { kick } from '../store.js';
import { mhz, DASH } from '../format.js';

const POLL_MS = 1000;

function scanCard(onStarted) {
  const c = card('Find systems');
  const all = h('input', { type: 'checkbox', 'aria-label': 'VHF and UHF too' });
  const start = h('button', { class: 'btn primary', type: 'button', text: 'Scan' });
  const cancel = h('button', { class: 'btn', type: 'button', text: 'Cancel', hidden: true });
  const status = h('p', { class: 'card-note' });
  const bar = h('div', { style: { height: '6px', background: 'var(--line)', borderRadius: '3px', overflow: 'hidden', marginTop: '10px' } });
  const fill = h('div', { style: { height: '100%', width: '0%', background: 'var(--accent)', transition: 'width 0.4s' } });
  bar.append(fill);
  c.body.append(
    h('p', { class: 'card-note', style: { marginTop: '0' }, text: 'Searches the 700, 800 and 900 MHz P25 bands for control channels. It takes a minute or two; the radio does not follow calls meanwhile and returns to the active site at the end.' }),
    h('div', { class: 'row', style: { gap: '10px', flexWrap: 'wrap' } },
      start, cancel, h('label', { class: 'row', style: { gap: '6px' } }, all, h('span', { text: 'VHF and UHF too (slower)' }))),
    bar, status);
  start.addEventListener('click', async () => {
    if (!confirm('Start a sweep? Calls are not followed until it ends.')) return;
    try {
      await api.discoveryScan({ all: all.checked });
      onStarted();
    } catch (e) {
      toast('Scan not started: ' + e.message, true);
    }
  });
  cancel.addEventListener('click', async () => {
    try { await api.discoveryCancel(); } catch (e) { toast(e.message, true); }
  });
  function update(d) {
    const running = ['sweeping', 'probing', 'restoring'].includes(d.state);
    start.disabled = running;
    cancel.hidden = !running;
    let pct = 0;
    if (running || d.state === 'done') {
      const sweep = d.steps ? d.step / d.steps : 0;
      const probe = d.to_probe ? d.probed / d.to_probe : 0;
      pct = d.state === 'done' ? 100 : Math.round(100 * (0.3 * sweep + 0.7 * probe));
    }
    fill.style.width = pct + '%';
    const words = {
      idle: 'No sweep yet.',
      sweeping: 'Scanning the spectrum: step ' + d.step + ' of ' + d.steps + '.',
      probing: 'Checking carriers: ' + d.probed + ' of ' + d.to_probe + (d.probing_hz ? ' (now ' + mhz(d.probing_hz, 5) + ')' : '') + '.',
      restoring: 'Returning to the active site.',
      done: 'Done: ' + d.sites.length + ' P25 site' + (d.sites.length === 1 ? '' : 's') + ', ' + d.other.length + ' other carrier' + (d.other.length === 1 ? '' : 's') + '.',
      cancelled: 'Cancelled; results so far are below.',
      error: 'Failed: ' + (d.error || 'unknown error'),
    };
    setText(status, words[d.state || 'idle'] || d.state);
  }
  return { el: c.el, update };
}

function sitesCard(onChanged) {
  const c = card('P25 sites found');
  const wrap = h('div', { class: 'table-wrap' });
  const empty = h('p', { class: 'card-note', text: 'Nothing yet. Run a scan.' });
  c.body.append(empty, wrap);

  async function add(s) {
    const suggested = 'System ' + (s.system_id != null ? s.system_id.toString(16).toUpperCase().padStart(3, '0') : '?') + ' site ' + s.rfss_id + '-' + s.site_id;
    const label = prompt('Name for this site:', suggested);
    if (!label) return;
    try {
      const r = await api.discoveryAdd({ key: keyOf(s), label });
      toast('Added ' + r.label);
      onChanged();
    } catch (e) {
      toast('Not added: ' + e.message, true);
    }
  }

  async function listen(name) {
    if (!confirm('Switch to site ' + name + '?')) return;
    try {
      await api.setSite(name);
      await api.applyPreset({ preset: 'auto' });
      toast('Switched to ' + name);
      kick(500);
    } catch (e) {
      toast('Switch failed: ' + e.message, true);
    }
  }

  function keyOf(s) {
    const hex = (v, n) => (v || 0).toString(16).toUpperCase().padStart(n, '0');
    return hex(s.wacn, 5) + '-' + hex(s.system_id, 3) + '-' + (s.rfss_id || 0) + '-' + (s.site_id || 0);
  }

  function update(d, busy) {
    const sites = (d.sites || []).slice().sort((a, b) => (b.level_db || 0) - (a.level_db || 0));
    empty.hidden = sites.length > 0;
    wrap.hidden = sites.length === 0;
    const head = h('tr', null, ...['Control channel', 'System', 'Site', 'NAC', 'Mod.', 'Signal', 'CRC', 'Neighbours', ''].map(t => h('th', { text: t })));
    const rows = sites.map(s => {
      const hex = (v, n) => v == null ? DASH : v.toString(16).toUpperCase().padStart(n, '0');
      const action = s.existing_site
        ? h('button', { class: 'btn small', type: 'button', text: 'Listen', title: 'Switch to site ' + s.existing_site, disabled: busy, onclick: () => listen(s.existing_site) })
        : h('button', { class: 'btn small', type: 'button', text: 'Add', title: 'Make this a site', onclick: () => add(s) });
      const nb = (s.neighbours || []).map(n => 'RFSS ' + n.rfss_id + ' site ' + n.site_id + (n.freq_hz ? ' ' + (n.freq_hz / 1e6).toFixed(5) : '')).join('\n');
      return h('tr', null,
        h('td', { text: (s.freq_hz / 1e6).toFixed(5) + (s.via_neighbour ? ' *' : '') }),
        h('td', { text: hex(s.wacn, 5) + '-' + hex(s.system_id, 3) }),
        h('td', { text: (s.rfss_id ?? DASH) + '-' + (s.site_id ?? DASH) + (s.existing_site ? ' (' + s.existing_site + ')' : '') }),
        h('td', { text: hex(s.nac, 3) }),
        h('td', { text: s.modulation }),
        h('td', { text: s.level_db ? s.level_db.toFixed(0) + ' dB' : DASH }),
        h('td', { text: Math.round(s.crc_pct) + '%' }),
        h('td', { text: String((s.neighbours || []).length), title: nb }),
        h('td', null, action));
    });
    wrap.replaceChildren(h('table', { class: 'table' }, h('thead', null, head), h('tbody', null, ...rows)));
  }
  return { el: c.el, update };
}

function otherCard() {
  const c = card('Other continuous carriers');
  const list = h('p', { class: 'card-note', style: { marginTop: '0' } });
  c.body.append(list,
    h('p', { class: 'card-note', text: 'Steady signals that are not P25 (for example DMR, NXDN or data links).' }));
  function update(d) {
    const fmt = x => (x.freq_hz / 1e6).toFixed(5) + ' (' + x.level_db.toFixed(0) + ' dB)';
    const o = (d.other || []).slice().sort((a, b) => a.freq_hz - b.freq_hz);
    const v = (d.p25_voice || []).slice().sort((a, b) => a.freq_hz - b.freq_hz);
    setText(list, (o.length ? o.map(fmt).join(', ') : 'None.') +
      (v.length ? ' · P25 voice channels busy during the scan: ' + v.map(fmt).join(', ') : ''));
  }
  return { el: c.el, update };
}

export function mount(host) {
  const sites = sitesCard(() => load());
  const other = otherCard();
  const scan = scanCard(() => load());
  host.append(h('div', { class: 'stack' }, scan.el, sites.el, other.el));
  let timer = null;
  async function load() {
    try {
      const d = await api.discovery();
      scan.update(d);
      sites.update(d, d.radio_busy);
      other.update(d);
    } catch { /* retried */ }
  }
  load();
  timer = setInterval(() => { if (!document.hidden) load(); }, POLL_MS);
  return {
    update() {},
    unmount() { clearInterval(timer); },
  };
}
