// Systems' SDRTrunk playlist pane: this radio's systems downloaded as a playlist, and a playlist
// taken in. The radio reads the file and says, system by system, where each would go (into the
// configured system on its channels' frequencies, or a new one), which of its channels would be
// sites and what its alias lists would add. Each change of a tick or a target asks the radio
// again; nothing is saved before Import.

import { h, toast, table } from '../dom.js';
import { api } from '../api.js';
import { mhz, num } from '../format.js';
import { protocol } from '../protocols.js';

// A request's body is no larger.
const MAX_BYTES = 2 * 1024 * 1024;
// The target select's value for a system of its own.
const NEW = '';

const plural = (n, one, many = `${one}s`) => `${num(n)} ${n === 1 ? one : many}`;

// What a channel is to the system it goes into.
const WHAT = {
  new: () => 'a new site',
  gains: c => `gives ${c.site_label} the frequencies it lacks`,
  configured: c => `${c.site_label}'s control channel`,
  known: c => `a channel ${c.site_label} lists`,
  unsupported: () => '',
};

function frequencies(c) {
  const shown = c.frequencies_hz.slice(0, 3).map(f => mhz(f).replace(' MHz', '')).join(', ');
  const more = c.frequencies_hz.length - 3;
  return c.frequencies_hz.length ? `${shown}${more > 0 ? ` and ${more} more` : ''} MHz` : '';
}

// What an import adds, as a sentence's parts.
function adds(p) {
  return [
    p.systems_added.length ? plural(p.systems_added.length, 'new system') : null,
    p.sites_added.length ? plural(p.sites_added.length, 'new site') : null,
    p.sites_updated.length ? `${plural(p.sites_updated.length, 'site')} given frequencies` : null,
    p.aliases_added ? `${plural(p.aliases_added, 'alias', 'aliases')} added` : null,
    p.aliases_named ? `${plural(p.aliases_named, 'talkgroup')} named` : null,
  ].filter(Boolean);
}

// `systems()` are the configured systems; `done()` is told the configuration changed.
export function playlistPane({ systems, done }) {
  const file = h('input', { class: 'input', type: 'file', accept: '.xml,text/xml,application/xml', 'aria-label': 'An SDRTrunk playlist' });
  const out = h('div', { class: 'pl-out' });
  const row = (name, note, control) => h('div', { class: 'set-row' },
    h('div', { class: 'set-what' }, h('div', { text: name }), h('div', { class: 'sub', text: note })), h('div', { class: 'set-ctl' }, control));
  const el = h('div', { class: 'pl' },
    h('h2', { text: 'SDRTrunk playlist' }),
    h('p', { class: 'card-note', text: 'SDRTrunk keeps its systems and names in a playlist: an XML file in the playlist folder of its own folder.' }),
    row('Export', 'This radio\'s systems as a playlist: a channel for each site, on its control channel and those that take over from it, and each system\'s aliases as a list named after it. A talkgroup\'s tuner is not in it: SDRTrunk has none.',
      h('a', { class: 'btn', href: '/api/v1/playlist?download=true', text: 'Download' })),
    row('Import', 'A playlist\'s systems, with their channels as sites and their alias lists as aliases. What it would change is shown first.', file),
    out);

  // The file's text and name, what the radio last said it would do, and what is taken of each
  // system: { into, channels (a Set, or null for the radio's suggestion), aliases }.
  let xml = null;
  let name = '';
  let plan = null;
  const take = new Map();
  // The systems whose channels that are not taken are unfolded.
  const open = new Set();
  // A newer file or answer ends an older one.
  let turn = 0;

  const say = (cls, text) => out.replaceChildren(h('p', { class: cls, text }));

  const request = () => ({
    xml,
    systems: [...take].map(([system, t]) => ({ name: system, into: t.into, channels: t.channels ? [...t.channels] : undefined, aliases: t.aliases })),
  });

  async function preview(first) {
    const mine = ++turn;
    try {
      const p = await api.playlist(first ? { xml } : request(), true);
      if (mine !== turn) return;
      plan = p;
      for (const s of p.systems) {
        const was = take.get(s.name);
        take.set(s.name, {
          into: s.into,
          channels: new Set(s.channels.filter(c => c.picked).map(c => c.index)),
          // What is wanted stays through a change of target that has no place for it yet.
          aliases: was ? was.aliases : s.lists.length > 0,
        });
      }
      draw();
    } catch (e) {
      if (mine === turn) say('pl-bad', `${name}: ${e.message}`);
    }
  }

  function channelRows(s, t, list) {
    return list.map(c => {
      const box = h('input', { type: 'checkbox', 'aria-label': `Take ${c.site} ${c.name}` });
      const takes = c.state === 'new' || c.state === 'gains';
      box.checked = c.picked;
      box.hidden = !takes;
      box.addEventListener('change', () => {
        if (box.checked) t.channels.add(c.index);
        else t.channels.delete(c.index);
        preview(false);
      });
      return h('tr', null, h('td', null, box), h('td', { text: c.site }), h('td', { text: c.enabled ? `${c.name} · SDRTrunk starts it` : c.name }),
        h('td', { text: frequencies(c) }), h('td', { text: [WHAT[c.state](c), c.note].filter(Boolean).join(': ') }));
    });
  }

  function card(s) {
    const t = take.get(s.name);
    const title = h('strong', { text: s.name || 'Channels with no system name' });
    if (!s.protocol) {
      const why = [...new Set(s.channels.map(c => c.note).filter(Boolean))].join('; ');
      return h('section', { class: 'pl-sys' }, h('div', { class: 'row' }, title, h('span', { class: 'dim', text: plural(s.channels.length, 'channel') })),
        h('p', { class: 'dim', text: `Nothing of it is taken: ${why}.` }));
    }
    const into = h('select', { class: 'input', 'aria-label': `Where ${s.name} goes` },
      h('option', { value: NEW, text: 'A new system' }),
      ...(systems() || []).filter(x => x.protocol === s.protocol).map(x => h('option', { value: x.id, text: x.id === s.matched ? `${x.label} (on its frequencies)` : x.label })));
    into.value = s.into || NEW;
    into.addEventListener('change', () => {
      t.into = into.value || null;
      // Its channels are what the radio suggests for the new place.
      t.channels = null;
      preview(false);
    });
    const picked = s.channels.some(c => c.picked);
    const names = h('input', { type: 'checkbox', 'aria-label': `Take the aliases of ${s.name}` });
    names.checked = t.aliases && s.lists.length > 0;
    names.disabled = !s.lists.length || (!s.into && !picked);
    names.addEventListener('change', () => {
      t.aliases = names.checked;
      preview(false);
    });
    const lists = s.lists.map(l => [
      `${l.name}: ${plural(l.aliases, 'alias', 'aliases')}, ${num(l.added)} to add`,
      l.named ? `${plural(l.named, 'talkgroup')} held under its number named` : null,
      l.kept ? `${plural(l.kept, 'number')} already named` : null,
      l.skipped ? `${num(l.skipped)} with nothing the scanner keeps` : null,
    ].filter(Boolean).join(', ')).join('; ');
    const namesNote = !s.lists.length ? 'Its channels name no alias list.' : !s.into && !picked ? `${lists}. A system of its own needs a site first: take a channel.` : `${lists}.`;
    // The channels that are taken or offered; the rest folded away.
    const shown = s.channels.filter(c => c.picked || ((c.state === 'new' || c.state === 'gains') && !c.note));
    const rest = s.channels.filter(c => !shown.includes(c));
    const head = ['', 'Site', 'Channel', 'Frequencies', 'What it is'];
    const more = h('details', { class: 'pl-more' }, h('summary', { text: `${plural(rest.length, 'other channel')}, not taken` }), table(head, channelRows(s, t, rest)));
    more.open = open.has(s.name);
    more.addEventListener('toggle', () => (more.open ? open.add(s.name) : open.delete(s.name)));
    return h('section', { class: 'pl-sys' },
      h('div', { class: 'row' }, title, h('span', { class: 'dim', text: protocol(s.protocol).label }), h('div', { class: 'spacer' }), h('span', { class: 'dim', text: 'into' }), into),
      h('label', { class: 'pl-names' }, names, h('span', { text: namesNote })),
      shown.length ? table(head, channelRows(s, t, shown)) : null,
      rest.length ? more : null);
  }

  function draw() {
    const what = adds(plan);
    const go = h('button', { class: 'btn primary', type: 'button', text: what.length ? 'Import' : 'Nothing to import', disabled: !what.length });
    go.addEventListener('click', async () => {
      go.disabled = true;
      const mine = ++turn;
      try {
        const r = await api.playlist(request(), false);
        const said = `From ${name}: ${adds(r).join(', ') || 'nothing'}.`;
        toast(said);
        done();
        if (mine !== turn) return;
        reset();
        say(null, said);
      } catch (e) {
        toast(e.message, true);
        if (mine === turn) draw();
      }
    });
    const unused = plan.unused_lists.map(l => `${l.name || 'no list'} (${num(l.aliases)})`).join(', ');
    out.replaceChildren(...[
      h('div', { class: 'pl-sum' },
        h('span', null, h('strong', { text: name }), `: ${plural(plan.channels, 'channel')} of ${plural(plan.systems.length, 'system')}, ${plural(plan.aliases, 'alias', 'aliases')}. `,
          what.length ? `As ticked: ${what.join(', ')}.` : 'As ticked, nothing changes.'),
        h('div', { class: 'spacer' }), go),
      ...plan.systems.map(card),
      unused ? h('p', { class: 'dim', text: `Alias lists no channel uses, not taken: ${unused}.` }) : null,
    ].filter(Boolean));
  }

  file.addEventListener('change', async () => {
    const f = file.files[0];
    if (!f) return;
    turn += 1;
    take.clear();
    open.clear();
    plan = null;
    name = f.name;
    if (f.size > MAX_BYTES) {
      say('pl-bad', `${name} is larger than 2 MB: the radio reads no playlist that large.`);
      return;
    }
    say('dim', `Reading ${name}…`);
    xml = await f.text();
    preview(true);
  });

  function reset() {
    turn += 1;
    xml = plan = null;
    take.clear();
    file.value = '';
    out.replaceChildren();
  }

  return { el };
}
