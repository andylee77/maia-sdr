// Aliases: a system's alias list as a table, SDRTrunk's model. An alias has a name, a group, a
// colour and the talkgroups or radios it names (numbers and ranges); a talkgroup's alias also
// says how it is listened to: its priority, its tuner, its Record mark, and whether it is
// followed at all. The page shows the talkgroups' aliases or the radios', searched, sorted and
// filtered by group; an edit in a row is saved at once, and the picked rows are changed
// together. Beside the list: what was heard in the last week and has no alias yet, each to be
// named in place, and RadioReference's files (rr_import.js).
//
// Every change replaces the list on the radio, guarded by the list's revision: a list changed
// elsewhere since it was read is read again, not written over.

import { h, toast, setText, setClass, keyedList, iconButton, switchInput } from '../dom.js';
import { api } from '../api.js';
import { boardNow } from '../store.js';
import { ago, num } from '../format.js';
import { colorOf, LOWEST_PRIORITY, TUNERS } from '../aliases.js';
import { importPane } from './rr_import.js';

// The two lists: what an ID of each is called in an alias, how the other list's IDs are marked
// in a row's numbers, and the history's name for the number.
const KINDS = {
  talkgroups: { exact: 'talkgroup', range: 'talkgroup_range', word: 'tg', one: 'talkgroup', stat: 'tg', eg: '300, 310-319' },
  radios: { exact: 'radio', range: 'radio_range', word: 'radio', one: 'radio', stat: 'unit', eg: '1014, 3000000-3999999' },
};
const kindOf = type => (type.startsWith('talkgroup') ? KINDS.talkgroups : KINDS.radios);
const MAX_ID = 0xffffffff;
// Rows drawn at once; Show more draws as many again.
const PAGE = 200;
// What "heard, not named" reads from the history: the last week, the longest on the air first.
const HEARD_HOURS = 168;
const HEARD_MAX = 1000;
const ALL = '*';
const PRIORITY_IS = `A priority is 1 (the highest) to ${LOWEST_PRIORITY}, or empty for none`;
const NO_GROUP = '';
const GROUPS_ID = 'als-groups';

const isOf = (id, k) => id.type === k.exact || id.type === k.range;
const hasKind = (a, k) => a.ids.some(id => isOf(id, k));

// An alias's numbers as text: this list's bare ("300, 310-319"), the other list's marked
// ("radio 1014").
function idsText(a, k) {
  return a.ids.map(id => {
    const n = id.min === undefined ? String(id.value) : `${id.min}-${id.max}`;
    return isOf(id, k) ? n : `${kindOf(id.type).word} ${n}`;
  }).join(', ');
}

// That text back to IDs; throws what is wrong with it.
function parseIds(text, k) {
  const out = [];
  for (const token of text.split(/[,;\n]+/).map(t => t.trim()).filter(Boolean)) {
    const m = /^(tg|radio)?\s*(\d+)(?:\s*-\s*(\d+))?$/i.exec(token);
    if (!m) throw new Error(`"${token}" is not a number or a range (${k.eg})`);
    const kind = !m[1] ? k : m[1].toLowerCase() === 'tg' ? KINDS.talkgroups : KINDS.radios;
    const min = Number(m[2]);
    const max = m[3] === undefined ? min : Number(m[3]);
    if (min > MAX_ID || max > MAX_ID) throw new Error(`"${token}" is too large a number`);
    if (max < min) throw new Error(`"${token}": a range runs from its lower number to its higher`);
    out.push(max === min ? { type: kind.exact, value: min } : { type: kind.range, min, max });
  }
  if (!out.length) throw new Error('An alias names at least one number');
  return out;
}

// A key for each alias of a list: its IDs (the radio lets no two aliases name the same one), with
// a count behind any that repeat.
function keysOf(aliases) {
  const seen = new Map();
  return aliases.map(a => {
    const ids = JSON.stringify(a.ids);
    const n = seen.get(ids) || 0;
    seen.set(ids, n + 1);
    return n ? `${ids}#${n}` : ids;
  });
}

// The lowest number an alias names in a list: what its row sorts by, and its colour's seed.
function firstId(a, k) {
  let best = null;
  for (const id of a.ids) {
    if (!isOf(id, k)) continue;
    const n = id.min === undefined ? id.value : id.min;
    if (best === null || n < best) best = n;
  }
  return best;
}

// Whether a number has an alias: named by itself, or inside a range.
function coverOf(aliases, k) {
  const exact = new Set();
  const ranges = [];
  for (const a of aliases) {
    for (const id of a.ids) {
      if (id.type === k.exact) exact.add(id.value);
      else if (id.type === k.range) ranges.push(id);
    }
  }
  return n => exact.has(n) || ranges.some(r => n >= r.min && n <= r.max);
}

// An alias's talkgroups are followed: it is not switched off, and where the system follows only
// talkgroups with a priority, it has one.
const followedBy = (a, listening) => !a.do_not_monitor && (listening.follow_unmonitored || a.priority != null);

const text = (a, b) => a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });

// One of a few, as buttons: `set(value)` marks the picked one.
function segment(options, onPick, label) {
  const buttons = options.map(([, said]) => h('button', { type: 'button', text: said }));
  buttons.forEach((b, i) => b.addEventListener('click', () => onPick(options[i][0])));
  return {
    el: h('div', { class: 'seg', role: 'group', 'aria-label': label }, ...buttons),
    set(value) {
      buttons.forEach((b, i) => {
        const on = options[i][0] === value;
        setClass(b, 'on', on);
        b.setAttribute('aria-pressed', String(on));
      });
    },
    say(value, said) {
      setText(buttons[options.findIndex(o => o[0] === value)], said);
    },
  };
}

// A row's text field: it reads as text until pointed at. Enter keeps what was typed, Escape
// drops it.
function field(cls, label, attrs) {
  const input = h('input', { class: `als-in ${cls}`, type: 'text', spellcheck: 'false', autocomplete: 'off', 'aria-label': label, ...attrs });
  input.addEventListener('keydown', e => {
    if (e.key === 'Enter') input.blur();
    else if (e.key === 'Escape') {
      input.value = input.dataset.was || '';
      input.blur();
    }
  });
  return input;
}

// A field shows `value`, unless it is being typed in.
function show(input, value) {
  input.dataset.was = value;
  if (document.activeElement !== input && input.value !== value) input.value = value;
}

// What was typed is refused: the field shows what it had, though it may still have the focus.
function back(input, why) {
  input.value = input.dataset.was || '';
  toast(why, true);
}

export function mount(el) {
  // The configured systems (the store's), and the one whose aliases are shown: the live site's
  // until another is picked.
  let systems = null;
  let system = null;
  // The shown system's { aliases, listening, rev } as the radio last gave them, and a key per alias.
  let data = null;
  let keys = [];
  let failed = null;
  // What decides recording (the radio pushes it with the live site's talkgroups).
  let policy = null;
  let view = 'talkgroups';
  // The aliases, or what was heard and has none.
  let named = true;
  let sort = { by: 'name', down: false };
  let shown = PAGE;
  // The picked rows' keys, and the rows the filters leave.
  const ticked = new Set();
  let matching = [];
  // What the history heard at the shown system in the last week, per list: { system, items, error }.
  const heard = { talkgroups: null, radios: null };
  // Reads and writes go one at a time, each on what the one before left. A step that fails says
  // so in the console and the next one still runs.
  let queue = Promise.resolve();
  function enqueue(step) {
    const run = queue.then(step);
    queue = run.catch(e => console.error(e));
    return run;
  }

  const pick = h('select', { class: 'input als-system', 'aria-label': 'The system whose aliases are shown' });
  const total = h('span', { class: 'dim' });
  const views = segment([['talkgroups', 'Talkgroups'], ['radios', 'Radios'], ['import', 'Import']], v => {
    view = v;
    shown = PAGE;
    ticked.clear();
    draw();
  }, 'What is shown');
  const note = h('div', { class: 'calls-empty', hidden: true });

  // A system's listening settings: what its talkgroups with no priority get.
  const li = {
    follow: segment([[true, 'Followed'], [false, 'Not followed']], v => setListening({ follow_unmonitored: v }), 'Talkgroups with no priority'),
    tuner: segment(TUNERS.map(([speaker, , said]) => [speaker, said]), v => setListening({ unmonitored_speaker: v }), 'Their tuner'),
    preempt: switchInput('A higher priority takes a tuner from a lower one', true, v => setListening({ preempt: v })),
    rec: h('span', { class: 'dim als-policy' }),
  };
  const listening = h('div', { class: 'als-strip' },
    h('span', { class: 'dim', text: 'Talkgroups with no priority' }), li.follow.el, h('span', { class: 'dim', text: 'on' }), li.tuner.el,
    li.preempt.el, h('div', { class: 'spacer' }), li.rec);

  const mode = segment([[true, 'Aliases'], [false, 'Heard, not named']], v => {
    named = v;
    shown = PAGE;
    ticked.clear();
    draw();
  }, 'The aliases, or what was heard and has none');
  const search = h('input', { class: 'input als-find', type: 'search', placeholder: 'Find', 'aria-label': 'Find by name, group or number' });
  const groupSel = h('select', { class: 'input als-group', 'aria-label': 'Show one group' });
  const again = h('button', { class: 'btn small', type: 'button', text: 'Read again', title: 'Read the last week from the history again' });
  const tools = h('div', { class: 'als-strip' }, mode.el, search, groupSel, h('div', { class: 'spacer' }), again);
  const groupList = h('datalist', { id: GROUPS_ID });
  search.addEventListener('input', () => {
    shown = PAGE;
    draw();
  });
  groupSel.addEventListener('change', () => {
    shown = PAGE;
    draw();
  });
  again.addEventListener('click', () => readHeard(true));

  // The table: a heading row for each of its two contents, then the rows.
  const tickAll = h('input', { type: 'checkbox', 'aria-label': 'Pick every alias shown' });
  const sorter = (by, said) => {
    const b = h('button', { class: 'als-sort', type: 'button', text: said, dataset: { by } });
    b.addEventListener('click', () => {
      sort = { by, down: sort.by === by ? !sort.down : false };
      draw();
    });
    return b;
  };
  const sorters = [sorter('name', 'Name'), sorter('ids', 'Numbers'), sorter('group', 'Group'), sorter('priority', 'Priority')];
  const recHead = h('span', { class: 'als-tg mid', text: 'Rec' });
  const head = h('div', { class: 'als-row als-head' }, tickAll, h('span'), sorters[0], sorters[1], sorters[2],
    h('span', { class: 'als-tg' }, sorters[3]),
    h('span', { class: 'als-tg mid', text: 'Tuner', title: 'The traffic tuner that follows it: either, or always the same one' }),
    recHead, h('span', { class: 'als-tg mid', text: 'Listen' }), h('span'));
  const heardHead = h('div', { class: 'als-row als-head als-heard' },
    h('span', { text: 'Number' }), h('span', { text: 'Calls this week' }), h('span', { text: 'Last heard' }), h('span', { text: 'Its name' }), h('span'));
  const rowsEl = h('div');
  const heardEl = h('div');
  const more = h('button', { class: 'btn small als-more', type: 'button' });
  const empty = h('div', { class: 'calls-empty' });
  const table = h('div', { class: 'als-table' }, head, heardHead, rowsEl, heardEl);
  const body = h('div', { class: 'als-body' }, table, more, empty);
  more.addEventListener('click', () => {
    shown += PAGE;
    draw();
  });
  tickAll.addEventListener('change', () => {
    for (const x of matching) {
      if (tickAll.checked) ticked.add(x.key);
      else ticked.delete(x.key);
    }
    draw();
  });

  // The foot: a new alias while no row is picked, else the picked rows' controls.
  const btn = (said, cls = '') => h('button', { class: `btn small ${cls}`.trim(), type: 'button', text: said });
  const add = {
    name: h('input', { class: 'input als-add-name', type: 'text', placeholder: 'Name', maxlength: '60', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'A new alias: its name' }),
    ids: h('input', { class: 'input als-add-ids', type: 'text', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'A new alias: the numbers it names' }),
    go: btn('Add', 'primary'),
  };
  const addForm = h('div', { class: 'row' }, h('span', { class: 'dim', text: 'New alias' }), add.name, add.ids, add.go);
  const bk = {
    count: h('strong'),
    group: h('input', { class: 'input als-bulk-group', type: 'text', placeholder: 'Group', list: GROUPS_ID, 'aria-label': 'A group for the picked aliases' }),
    setGroup: btn('Set group'),
    pri: h('input', { class: 'input tgl-pri', type: 'number', min: '1', max: String(LOWEST_PRIORITY), step: '1', placeholder: 'none', 'aria-label': 'A priority for the picked aliases, 1 the highest' }),
    setPri: btn('Set priority'),
    tuner: segment(TUNERS.map(([speaker, , said]) => [speaker, said]), speaker => bulk(() => ({ speaker })), 'A tuner for the picked aliases'),
    rec: btn('Record'),
    noRec: btn('No record'),
    on: btn('Listen'),
    off: btn('Skip'),
    color: h('input', { type: 'color', value: '#35c8c0', 'aria-label': 'A colour for the picked aliases' }),
    noColor: btn('No colour'),
    drop: btn('Remove', 'danger'),
    clear: btn('Clear', 'quiet'),
  };
  const tgOnly = (...nodes) => h('span', { class: 'row als-tg' }, ...nodes);
  const bulkBar = h('div', { class: 'row', hidden: true }, bk.count,
    h('span', { class: 'row' }, bk.group, bk.setGroup), tgOnly(bk.pri, bk.setPri), tgOnly(bk.tuner.el), tgOnly(bk.rec, bk.noRec), tgOnly(bk.on, bk.off),
    h('span', { class: 'row' }, h('label', { class: 'als-swatch', title: 'A colour for the picked aliases' }, bk.color), bk.noColor),
    h('div', { class: 'spacer' }), bk.drop, bk.clear);
  const heardHint = h('div', { class: 'dim', hidden: true });
  const foot = h('div', { class: 'als-foot' }, addForm, bulkBar, heardHint);

  const imp = importPane({
    system: () => systems && systems.find(x => x.id === system),
    done(result) {
      load();
      if (result.file !== 'talkgroups') return;
      view = 'talkgroups';
      named = true;
      draw();
    },
  });

  const card = h('section', { class: 'card flush als-card', 'aria-label': 'Aliases' },
    h('div', { class: 'card-head' }, h('h2', { text: 'Aliases' }), pick, total, h('div', { class: 'spacer' }), views.el),
    note, listening, tools, body, foot, imp.el, groupList);
  el.append(h('div', { class: 'aliases' }, card));

  pick.addEventListener('change', () => choose(pick.value));

  function choose(id) {
    system = id;
    data = null;
    keys = [];
    failed = null;
    shown = PAGE;
    ticked.clear();
    heard.talkgroups = heard.radios = null;
    imp.reset();
    draw();
    load();
  }

  function setData(got) {
    const same = !failed && data && data.rev === got.rev && JSON.stringify(data.listening) === JSON.stringify(got.listening);
    failed = null;
    if (same) return;
    data = got;
    keys = keysOf(got.aliases);
    for (const k of [...ticked]) if (!keys.includes(k)) ticked.delete(k);
    draw();
  }

  // The shown system's aliases, read behind any write under way.
  function load() {
    return enqueue(async () => {
      const id = system;
      if (!id) return;
      try {
        const got = await api.aliases(id);
        if (id === system) setData(got);
      } catch (e) {
        if (id !== system) return;
        failed = e.message;
        draw();
      }
    });
  }

  // One change to the list: `change(aliases)` gives the list as it is to be, or throws what is
  // wrong. The radio answers with the list it then holds; a refusal is shown and the rows go
  // back to what it holds.
  function mutate(change, done) {
    return enqueue(async () => {
      const id = system;
      if (!data || !id) return false;
      let next;
      try {
        next = change(data.aliases);
      } catch (e) {
        toast(e.message, true);
        draw();
        return false;
      }
      try {
        const got = await api.setAliases(id, next, data.rev);
        if (id !== system) return false;
        setData(got);
        if (done) toast(done);
        return true;
      } catch (e) {
        if (id !== system) return false;
        toast(e.status === 409 ? 'The aliases were changed elsewhere: they are read again, and this change was not made' : e.message, true);
        try {
          setData(await api.aliases(id));
        } catch { /* the next change of the systems reads them */ }
        draw();
        return false;
      }
    });
  }

  // One alias, found again by its key in the list as it is when the change runs.
  const patch = (key, change) => mutate(list => {
    const i = keysOf(list).indexOf(key);
    if (i < 0) throw new Error('That alias is gone: the list was changed elsewhere');
    return list.map((a, j) => (j === i ? { ...a, ...change(a) } : a));
  });

  // The picked aliases, each changed by `change(alias)`.
  function bulk(change, done) {
    const want = new Set(ticked);
    return mutate(list => {
      const ks = keysOf(list);
      return list.map((a, i) => (want.has(ks[i]) ? { ...a, ...change(a) } : a));
    }, done);
  }

  // Followed again: where the system follows only talkgroups with a priority, one with none
  // gets the lowest (as the radio does for one talkgroup's `follow`).
  const listenOn = a => ({ do_not_monitor: false, priority: a.priority ?? (data.listening.follow_unmonitored ? null : LOWEST_PRIORITY) });

  function addAlias(k, name, ids) {
    // A talkgroup keeps the tuner it had with no alias.
    const speaker = k === KINDS.talkgroups ? data.listening.unmonitored_speaker : 'both';
    return mutate(list => [...list, { name, ids, priority: null, do_not_monitor: false, record: false, speaker }], `${name} added`);
  }

  function setListening(change) {
    enqueue(async () => {
      const id = system;
      if (!data || !id) return;
      try {
        const got = await api.setListening(id, { ...data.listening, ...change });
        if (id === system) setData(got);
      } catch (e) {
        toast(e.message, true);
        draw();
      }
    });
  }

  // A typed priority: a number, null for none, or undefined when it is neither.
  function priorityOf(input) {
    const typed = input.value.trim();
    const p = Number(typed);
    if (!typed) return null;
    return Number.isInteger(p) && p >= 1 && p <= LOWEST_PRIORITY ? p : undefined;
  }

  function createRow() {
    const c = {
      tick: h('input', { type: 'checkbox', 'aria-label': 'Pick this alias' }),
      color: h('input', { type: 'color', 'aria-label': 'Its colour' }),
      name: field('als-name', 'Its name', { maxlength: '60' }),
      ids: field('mono', 'The numbers it names'),
      group: field('', 'Its group', { list: GROUPS_ID }),
      pri: field('', 'Its priority, 1 the highest', { type: 'number', min: '1', max: String(LOWEST_PRIORITY), step: '1', placeholder: 'none' }),
      tuner: h('button', { class: 'tgl-chip', type: 'button' }),
      rec: h('button', { class: 'tgl-rec', type: 'button' }),
      on: h('input', { type: 'checkbox', role: 'switch' }),
      drop: iconButton('trash', 'Remove this alias', { cls: 'quiet danger' }),
    };
    c.swatch = h('label', { class: 'als-swatch' }, c.color);
    const row = h('div', { class: 'als-row' }, c.tick, c.swatch, c.name, c.ids, c.group,
      h('span', { class: 'als-tg' }, c.pri), h('span', { class: 'als-tg mid' }, c.tuner), h('span', { class: 'als-tg mid' }, c.rec),
      h('label', { class: 'switch als-tg' }, c.on), c.drop);
    row._c = c;
    const mine = change => patch(row._x.key, change);
    c.tick.addEventListener('change', () => {
      if (c.tick.checked) ticked.add(row._x.key);
      else ticked.delete(row._x.key);
      draw();
    });
    c.color.addEventListener('change', () => {
      const color = c.color.value;
      mine(() => ({ color }));
    });
    c.name.addEventListener('change', () => {
      const name = c.name.value.trim();
      if (name) mine(() => ({ name }));
      else back(c.name, 'An alias needs a name');
    });
    c.ids.addEventListener('change', () => {
      const [typed, k] = [c.ids.value, KINDS[view]];
      mine(() => ({ ids: parseIds(typed, k) }));
    });
    c.group.addEventListener('change', () => {
      const group = c.group.value.trim() || null;
      mine(() => ({ group }));
    });
    c.pri.addEventListener('change', () => {
      const priority = priorityOf(c.pri);
      if (priority === undefined) back(c.pri, PRIORITY_IS);
      else mine(() => ({ priority }));
    });
    c.tuner.addEventListener('click', () => {
      const i = TUNERS.findIndex(t => t[0] === row._x.a.speaker);
      mine(() => ({ speaker: TUNERS[(i + 1) % TUNERS.length][0] }));
    });
    c.rec.addEventListener('click', () => mine(a => ({ record: !a.record })));
    c.on.addEventListener('change', () => {
      const on = c.on.checked;
      mine(a => (on ? listenOn(a) : { do_not_monitor: true }));
    });
    c.drop.addEventListener('click', () => {
      const { a, key } = row._x;
      if (!confirm(`Remove the alias ${a.name}?`)) return;
      mutate(list => {
        const ks = keysOf(list);
        return list.filter((_, i) => ks[i] !== key);
      }, `${a.name} removed`);
    });
    return row;
  }

  function updateRow(row, x) {
    const c = row._c;
    const { a, key } = x;
    const k = KINDS[view];
    row._x = x;
    const on = ticked.has(key);
    c.tick.checked = on;
    setClass(row, 'sel', on);
    c.swatch.style.background = colorOf(firstId(a, k) || 0, a);
    setClass(c.swatch, 'auto', !a.color);
    c.swatch.title = a.color ? `Its colour, ${a.color}` : 'No colour of its own: pick one';
    // A colour picker opens on a grey, so any colour picked is a change.
    c.color.value = a.color || '#808080';
    show(c.name, a.name);
    show(c.ids, idsText(a, k));
    show(c.group, a.group || '');
    const followed = k !== KINDS.talkgroups || followedBy(a, data.listening);
    setClass(row, 'off', !followed);
    if (k !== KINDS.talkgroups) return;
    show(c.pri, a.priority == null ? '' : String(a.priority));
    const [, chip, , full] = TUNERS.find(t => t[0] === a.speaker) || TUNERS[0];
    setText(c.tuner, chip);
    c.tuner.title = `${a.name} is followed by ${full}`;
    c.tuner.setAttribute('aria-label', c.tuner.title);
    setClass(c.tuner, 'set', a.speaker !== 'both');
    setClass(c.rec, 'on', a.record);
    c.rec.title = a.record ? `${a.name} is marked to be recorded` : `Mark ${a.name} to be recorded`;
    c.rec.setAttribute('aria-label', c.rec.title);
    c.rec.setAttribute('aria-pressed', String(a.record));
    c.on.checked = followed;
    c.on.setAttribute('aria-label', `Listen to ${a.name}`);
    c.on.parentElement.title = followed ? `${a.name} is listened to` : a.do_not_monitor ? `${a.name} is switched off` : `${a.name} has no priority, and this system follows only talkgroups with one`;
  }

  // The history's week for the list shown, read when "heard, not named" is first opened for a
  // system, and again when asked.
  function readHeard(fresh) {
    const id = system;
    const kind = view;
    if (!KINDS[kind] || (!fresh && heard[kind] && heard[kind].system === id)) return;
    heard[kind] = { system: id, items: null, error: null };
    const got = (items, error) => {
      if (!heard[kind] || heard[kind].system !== id || system !== id) return;
      heard[kind] = { system: id, items, error };
      draw();
    };
    api.activity(kind, `system=${encodeURIComponent(id)}&hours=${HEARD_HOURS}&limit=${HEARD_MAX}`).then(r => got(r.items, null), e => got([], e.message));
  }

  function createHeard() {
    const c = {
      n: h('span', { class: 'mono' }),
      calls: h('span'),
      last: h('span', { class: 'dim' }),
      name: h('input', { class: 'input', type: 'text', placeholder: 'Name', maxlength: '60', spellcheck: 'false', autocomplete: 'off' }),
      go: btn('Add'),
    };
    const row = h('div', { class: 'als-row als-heard' }, c.n, c.calls, c.last, c.name, c.go);
    row._c = c;
    const name = async () => {
      const typed = c.name.value.trim();
      if (!typed) {
        c.name.focus();
        return;
      }
      // The next number's name is typed next.
      const next = row.nextElementSibling;
      const k = KINDS[view];
      if (await addAlias(k, typed, [{ type: k.exact, value: row._n }]) && next && next.isConnected) next._c.name.focus();
    };
    c.go.addEventListener('click', name);
    c.name.addEventListener('keydown', e => {
      if (e.key === 'Enter') name();
    });
    return row;
  }

  function updateHeard(row, item, k, now) {
    const c = row._c;
    row._n = item[k.stat];
    setText(c.n, `${k === KINDS.talkgroups ? 'TG' : 'Radio'} ${item[k.stat]}`);
    setText(c.calls, item.encrypted && item.encrypted === item.calls ? `${num(item.calls)}, all encrypted` : num(item.calls));
    setText(c.last, ago(now - item.last_ms));
    c.name.setAttribute('aria-label', `A name for ${c.n.textContent}`);
  }

  function drawHead(sys) {
    const options = (systems || []).map(s => `${s.id}\n${s.label}`).join('\n');
    if (pick.dataset.from !== options) {
      pick.dataset.from = options;
      pick.replaceChildren(...(systems || []).map(s => h('option', { value: s.id, text: s.label })));
    }
    pick.hidden = !sys;
    views.el.hidden = !sys;
    if (sys) pick.value = sys.id;
    views.set(view);
    const count = k => (data ? data.aliases.filter(a => hasKind(a, k)).length : null);
    const said = (title, n) => (n === null ? title : `${title} · ${num(n)}`);
    views.say('talkgroups', said('Talkgroups', count(KINDS.talkgroups)));
    views.say('radios', said('Radios', count(KINDS.radios)));
    setText(total, data ? `${num(data.aliases.length)} alias${data.aliases.length === 1 ? '' : 'es'}` : '');
  }

  function drawListening() {
    const l = data.listening;
    li.follow.set(l.follow_unmonitored);
    li.tuner.set(l.unmonitored_speaker);
    li.preempt.input.checked = l.preempt;
    li.follow.el.title = l.follow_unmonitored
      ? 'A talkgroup with no priority, or no alias, is followed at the lowest priority'
      : 'Only talkgroups whose alias has a priority are followed';
    setText(li.rec, !policy ? '' : !policy.enabled ? 'Recording is off: the Rec marks wait' : policy.every_call ? 'Every followed call is recorded: the Rec marks apply once Record is on Marked' : 'Recorded: the talkgroups marked Rec');
    recHead.title = 'Marked to be recorded. Now’s Record says whether the marks decide: Off, Marked, or Every call';
  }

  function drawAliases(k, mine) {
    const q = search.value.trim().toLowerCase();
    const g = groupSel.value;
    // A number typed finds the alias whose range holds it, too.
    const n = /^\d+$/.test(q) ? Number(q) : null;
    const match = ({ a }) => (g === ALL || (a.group || NO_GROUP) === g)
      && (!q || a.name.toLowerCase().includes(q) || (a.group || '').toLowerCase().includes(q) || idsText(a, k).includes(q) || (n !== null && coverOf([a], k)(n)));
    const by = {
      name: (x, y) => text(x.a.name, y.a.name),
      ids: (x, y) => firstId(x.a, k) - firstId(y.a, k),
      group: (x, y) => text(x.a.group || '￿', y.a.group || '￿'),
      priority: (x, y) => (x.a.priority ?? LOWEST_PRIORITY + 1) - (y.a.priority ?? LOWEST_PRIORITY + 1),
    }[sort.by];
    const way = sort.down ? -1 : 1;
    matching = mine.filter(match).sort((x, y) => way * by(x, y) || text(x.a.name, y.a.name) || firstId(x.a, k) - firstId(y.a, k));
    keyedList(rowsEl, matching.slice(0, shown), x => x.key, createRow, updateRow);
    more.hidden = matching.length <= shown;
    setText(more, `Show ${num(Math.min(PAGE, matching.length - shown))} more of ${num(matching.length)}`);
    setText(empty, mine.length ? (matching.length ? '' : 'No alias matches.') : `No ${k.one} has an alias yet. Add one below, name one that was heard, or import a file.`);
    const picked = matching.filter(x => ticked.has(x.key)).length;
    tickAll.checked = picked > 0 && picked === matching.length;
    tickAll.indeterminate = picked > 0 && picked < matching.length;
    tickAll.disabled = !matching.length;
    for (const b of sorters) {
      const on = b.dataset.by === sort.by;
      setClass(b, 'on', on);
      setClass(b, 'down', on && sort.down);
      b.setAttribute('aria-sort', on ? (sort.down ? 'descending' : 'ascending') : 'none');
    }
  }

  // What the history heard of the list shown that no alias names; null until it is read.
  function unnamed(k) {
    const hd = heard[view];
    if (!hd || hd.system !== system || !hd.items || hd.error) return null;
    const covered = coverOf(data.aliases, k);
    return hd.items.filter(i => !covered(i[k.stat]));
  }

  function drawHeard(k) {
    readHeard(false);
    const hd = heard[view];
    const q = search.value.trim();
    const all = unnamed(k) || [];
    const rows = all.filter(i => !q || String(i[k.stat]).includes(q));
    const now = boardNow();
    keyedList(heardEl, rows.slice(0, shown), i => i[k.stat], createHeard, (row, i) => updateHeard(row, i, k, now));
    more.hidden = rows.length <= shown;
    setText(more, `Show ${num(Math.min(PAGE, rows.length - shown))} more of ${num(rows.length)}`);
    setText(empty, !hd || hd.items === null ? 'Reading the last week…'
      : hd.error ? `The history could not be read: ${hd.error}`
        : all.length ? (rows.length ? '' : 'No number matches.')
          : `Every ${k.one} heard in the last week has an alias.`);
    setText(heardHint, `Heard at this system's sites in the last week with no alias, the longest on the air first. A name typed here makes one${hd && hd.items && hd.items.length >= HEARD_MAX ? ` (the first ${num(HEARD_MAX)} are read)` : ''}.`);
  }

  function draw() {
    const sys = systems && systems.find(x => x.id === system);
    drawHead(sys);
    const lists = !!sys && !!data && view !== 'import';
    imp.el.hidden = !sys || view !== 'import';
    for (const x of [tools, body, foot]) x.hidden = !lists;
    listening.hidden = !lists || view !== 'talkgroups';
    setText(note, !systems ? '' : !sys ? 'No system is configured yet. Systems finds the ones on the air.'
      : view === 'import' ? '' : failed ? `The aliases could not be read: ${failed}` : data ? '' : 'Reading…');
    note.hidden = !note.textContent;
    if (!lists) return;
    const k = KINDS[view];
    const mine = data.aliases.map((a, i) => ({ a, key: keys[i] })).filter(x => hasKind(x.a, k));
    // The groups in use: what the filter offers and what a group field suggests.
    const groups = [...new Set(mine.map(x => x.a.group).filter(Boolean))].sort(text);
    const from = groups.join('\n');
    if (groupSel.dataset.from !== from) {
      const was = groupSel.value || ALL;
      groupSel.dataset.from = from;
      groupSel.replaceChildren(h('option', { value: ALL, text: 'Every group' }), ...groups.map(g => h('option', { value: g, text: g })),
        h('option', { value: NO_GROUP, text: 'No group' }));
      groupSel.value = was === ALL || was === NO_GROUP || groups.includes(was) ? was : ALL;
      groupList.replaceChildren(...groups.map(g => h('option', { value: g })));
    }
    mode.set(named);
    mode.say(true, `Aliases · ${num(mine.length)}`);
    table.className = `als-table ${view}${named ? '' : ' heard'}`;
    card.dataset.list = view;
    rowsEl.hidden = head.hidden = !named;
    heardEl.hidden = heardHead.hidden = named;
    groupSel.hidden = !named || !groups.length;
    again.hidden = named;
    if (view === 'talkgroups') drawListening();
    if (named) drawAliases(k, mine);
    else drawHeard(k);
    const left = unnamed(k);
    mode.say(false, left ? `Heard, not named · ${num(left.length)}` : 'Heard, not named');
    empty.hidden = !empty.textContent;
    // The foot.
    const picked = named ? ticked.size : 0;
    addForm.hidden = !named || picked > 0;
    bulkBar.hidden = picked === 0;
    heardHint.hidden = named;
    add.ids.placeholder = `Numbers: ${k.eg}`;
    setText(bk.count, `${num(picked)} picked`);
    bk.tuner.set(null);
  }

  // The foot's controls.
  const addNew = async () => {
    const name = add.name.value.trim();
    if (!name) {
      add.name.focus();
      return;
    }
    let ids;
    try {
      ids = parseIds(add.ids.value, KINDS[view]);
    } catch (e) {
      toast(e.message, true);
      add.ids.focus();
      return;
    }
    if (!await addAlias(KINDS[view], name, ids)) return;
    add.name.value = add.ids.value = '';
    add.name.focus();
  };
  add.go.addEventListener('click', addNew);
  for (const x of [add.name, add.ids]) {
    x.addEventListener('keydown', e => {
      if (e.key === 'Enter') addNew();
    });
  }
  bk.setGroup.addEventListener('click', () => bulk(() => ({ group: bk.group.value.trim() || null })));
  bk.setPri.addEventListener('click', () => {
    const priority = priorityOf(bk.pri);
    if (priority === undefined) toast(PRIORITY_IS, true);
    else bulk(() => ({ priority }));
  });
  bk.rec.addEventListener('click', () => bulk(() => ({ record: true })));
  bk.noRec.addEventListener('click', () => bulk(() => ({ record: false })));
  bk.on.addEventListener('click', () => bulk(listenOn));
  bk.off.addEventListener('click', () => bulk(() => ({ do_not_monitor: true })));
  bk.color.addEventListener('change', () => {
    const color = bk.color.value;
    bulk(() => ({ color }));
  });
  bk.noColor.addEventListener('click', () => bulk(() => ({ color: null })));
  bk.clear.addEventListener('click', () => {
    ticked.clear();
    draw();
  });
  bk.drop.addEventListener('click', () => {
    const want = new Set(ticked);
    if (!confirm(`Remove the ${want.size} picked alias${want.size === 1 ? '' : 'es'}?`)) return;
    mutate(list => {
      const ks = keysOf(list);
      return list.filter((_, i) => !want.has(ks[i]));
    }, `${num(want.size)} removed`);
  });

  // The store's systems last read from.
  let seen = null;

  return {
    update(s) {
      const p = (s.talkgroups && s.talkgroups.recording) || null;
      const moved = !policy !== !p || (p && (p.enabled !== policy.enabled || p.every_call !== policy.every_call));
      policy = p;
      if (moved && data && !listening.hidden) drawListening();
      if (!s.systems || s.systems === seen) return;
      // The systems changed, here or from another page: the picked one's aliases are read again.
      seen = systems = s.systems;
      if (systems.some(x => x.id === system)) {
        draw();
        load();
        return;
      }
      const live = s.talkgroups && s.talkgroups.system;
      const first = systems.find(x => x.id === live) || systems[0];
      if (first) choose(first.id);
      else {
        system = data = null;
        draw();
      }
    },
    unmount() {},
  };
}
