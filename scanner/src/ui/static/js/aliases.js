// A system's aliases as the pages read them: the alias that names a talkgroup, and a
// talkgroup's name and colour on screen. How a talkgroup is treated (followed, its tuner, its
// priority, recorded) is the radio's to say: it pushes the live site's talkgroups (store.js).

// The alias of talkgroup `tg` in `sys`: the one naming it alone, else the first range holding it.
export function aliasOf(sys, tg) {
  let ranged = null;
  for (const a of (sys && sys.aliases) || []) {
    for (const id of a.ids || []) {
      if (id.type === 'talkgroup' && id.value === tg) return a;
      if (!ranged && id.type === 'talkgroup_range' && tg >= id.min && tg <= id.max) ranged = a;
    }
  }
  return ranged;
}

// A talkgroup's own name: its alias's, unless that is only its number ("TG 300", what an alias
// made from the live screen starts as).
export function nameOf(tg, name) {
  return name && name !== `TG ${tg}` ? name : null;
}

export const tgLabel = (tg, name) => nameOf(tg, name) || `TG ${tg}`;

// A talkgroup's colour: its alias's, else one from its number (a talkgroup keeps its colour).
export function colorOf(tg, alias) {
  return alias && alias.color ? alias.color : `hsl(${(tg * 47) % 360} 55% 58%)`;
}
