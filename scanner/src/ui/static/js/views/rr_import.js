// RadioReference's downloads into a system (the Aliases page's Import): its talkgroups file as
// aliases, its sites file as sites. The radio tells the two apart and says what a file would
// change; nothing changes until that is shown and confirmed.

import { h, toast, table, switchInput } from '../dom.js';
import { api } from '../api.js';
import { mhz, num } from '../format.js';

// Rows of a talkgroups file shown before it is imported.
const SAMPLE = 200;

const plural = (n, one, many = `${one}s`) => `${num(n)} ${n === 1 ? one : many}`;

// `system()` is the system files go into; `done(result)` is told what an import did.
export function importPane({ system, done }) {
  const file = h('input', { class: 'input', type: 'file', accept: '.csv,text/csv', 'aria-label': 'A RadioReference file' });
  const out = h('div', { class: 'imp-out' });
  const into = h('strong');
  const el = h('div', { class: 'imp', hidden: true },
    h('h3', { text: 'From RadioReference' }),
    h('p', { class: 'card-note' },
      'RadioReference offers two files for a trunked system. Its talkgroups file gives an alias to each talkgroup that has none, named and grouped as the file says. Its sites file adds the sites the system lacks, and gives the ones it has the channels they lack. Choose either for ',
      into, ': what it would change is shown first.'),
    h('div', { class: 'row' }, file), out,
    h('p', { class: 'card-note' }, 'An SDRTrunk playlist\'s alias lists come in with its systems: ', h('a', { href: '#systems', text: 'Systems' }), ', SDRTrunk playlist.'));
  // A newer file, or another system, ends what an older one started.
  let turn = 0;

  const say = (cls, text) => out.replaceChildren(h('p', { class: cls, text }));
  // The parts there are, in place of what was shown.
  const put = (...parts) => out.replaceChildren(...parts.filter(Boolean));

  async function run(sys, name, req) {
    const mine = turn;
    try {
      const r = await api.radioReference(sys.id, req, false);
      if (mine !== turn) return;
      reset();
      const said = r.file === 'talkgroups'
        ? `${plural(r.aliases_added.length, 'alias', 'aliases')} added to ${sys.label}${r.aliases_named.length ? `, ${plural(r.aliases_named.length, 'talkgroup')} named` : ''}`
        : `${sys.label}: ${plural(r.sites_added.length, 'site')} added, ${num(r.sites_updated.length)} given channels`;
      toast(said);
      if (r.file === 'sites') out.replaceChildren(h('p', null, `${said}. They are on `, h('a', { href: '#systems', text: 'Systems' }), '.'));
      done(r);
    } catch (e) {
      if (mine === turn) say('imp-bad', `${name}: ${e.message}`);
    }
  }

  function talkgroups(sys, name, csv, p) {
    const added = p.aliases_added;
    const off = added.filter(a => a.do_not_monitor).length;
    const skip = switchInput(`Never follow the ${plural(off, 'fully encrypted talkgroup')}`, true, () => marks());
    // Talkgroups an alias holds under their number only ("TG 300"): they take the file's name.
    const named = p.aliases_named.length;
    const go = h('button', {
      class: 'btn primary', type: 'button', disabled: !added.length && !named,
      text: `Add ${plural(added.length, 'alias', 'aliases')}${named ? `, name ${num(named)}` : ''}`,
    });
    const rows = added.slice(0, SAMPLE).map(a => {
      const mark = h('td');
      mark._off = a.do_not_monitor;
      return h('tr', null, h('td', { class: 'mono', text: String(a.ids[0].value) }), h('td', { text: a.name }), h('td', { text: a.group || '' }), mark);
    });
    function marks() {
      for (const r of rows) r.lastChild.replaceChildren(r.lastChild._off && skip.input.checked ? h('span', { class: 'badge enc', text: 'never followed' }) : '');
    }
    marks();
    go.addEventListener('click', () => {
      go.disabled = true;
      run(sys, name, { csv, encrypted_do_not_monitor: skip.input.checked });
    });
    const kept = p.talkgroups_kept.length;
    put(
      h('p', null, h('strong', { text: name }), `: ${plural(added.length + named + kept, 'talkgroup')}. `,
        `${num(added.length)} would become ${added.length === 1 ? 'an alias' : 'aliases'}; `,
        named ? `${num(named)} held under ${named === 1 ? 'its number' : 'their numbers'} would take ${named === 1 ? 'its name' : 'their names'}; ` : '',
        kept === 1 ? '1 already has an alias and stays as it is.' : `${num(kept)} already have an alias and stay as they are.`),
      h('div', { class: 'row' }, off ? skip.el : null, h('div', { class: 'spacer' }), go),
      added.length ? table(['Number', 'Name', 'Group', ''], rows) : null,
      added.length > SAMPLE ? h('p', { class: 'dim', text: `And ${num(added.length - SAMPLE)} more.` }) : null);
  }

  function sites(sys, name, csv, p) {
    const boxes = [];
    const row = (s, what) => {
      const box = h('input', { type: 'checkbox', 'aria-label': `Import ${s.site.label}` });
      box.checked = true;
      box.addEventListener('change', label);
      boxes.push([s.key, box]);
      // A site's channels are every frequency but its control channel, the alternates among them.
      const c = s.site.control;
      return h('tr', null, h('td', null, box), h('td', { text: s.site.label }), h('td', { class: 'mono', text: s.key }), h('td', { text: mhz(c.freq_hz) }),
        h('td', { text: num(c.alternates_hz.length) }), h('td', { text: num(s.site.channels_hz.length + 1) }), h('td', { text: what }));
    };
    const rows = [...p.sites_added.map(s => row(s, 'new')), ...p.sites_updated.map(s => row(s, 'gains channels'))];
    const go = h('button', { class: 'btn primary', type: 'button' });
    function label() {
      const n = boxes.filter(([, b]) => b.checked).length;
      go.textContent = `Import ${plural(n, 'site')}`;
      go.disabled = n === 0;
    }
    label();
    go.addEventListener('click', () => {
      go.disabled = true;
      run(sys, name, { csv, sites: boxes.filter(([, b]) => b.checked).map(([key]) => key) });
    });
    put(
      h('p', null, h('strong', { text: name }), `: ${num(p.sites_added.length)} new, ${num(p.sites_updated.length)} that gain channels`,
        p.skipped.length ? `, ${num(p.skipped.length)} left out.` : '.', rows.length ? '' : ' Nothing to import.'),
      rows.length ? h('div', { class: 'row' }, h('div', { class: 'spacer' }), go) : null,
      rows.length ? table(['', 'Site', 'Key', 'Control channel', 'Alternates', 'Frequencies', ''], rows) : null,
      p.skipped.length ? h('p', { class: 'dim', text: `Left out: ${p.skipped.join('; ')}` }) : null);
  }

  file.addEventListener('change', async () => {
    const f = file.files[0];
    const sys = system();
    if (!f || !sys) return;
    const mine = ++turn;
    say('dim', `Reading ${f.name}…`);
    try {
      const csv = await f.text();
      const p = await api.radioReference(sys.id, { csv }, true);
      if (mine !== turn) return;
      (p.file === 'talkgroups' ? talkgroups : sites)(sys, f.name, csv, p);
    } catch (e) {
      if (mine === turn) say('imp-bad', `${f.name}: ${e.message}`);
    }
  });

  function reset() {
    turn += 1;
    file.value = '';
    out.replaceChildren();
    const sys = system();
    into.textContent = sys ? sys.label : '';
  }

  return { el, reset };
}
