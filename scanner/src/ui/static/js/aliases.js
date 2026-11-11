// A system's aliases as the pages read them: the alias of a talkgroup, what it means for
// following (as the follower decides it), and a talkgroup's name and colour on screen.

const known = v => v !== null && v !== undefined;

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

// How `sys` treats talkgroup `tg`: followed or not, where it plays, and its priority (null:
// none). `needsPriority`: the system follows only talkgroups that have one.
export function routeOf(sys, tg) {
  const a = aliasOf(sys, tg);
  const l = (sys && sys.listening) || { follow_unmonitored: true, unmonitored_speaker: 'both' };
  const priority = a && known(a.priority) ? a.priority : null;
  return {
    alias: a,
    followed: !(a && a.do_not_monitor) && (l.follow_unmonitored || priority !== null),
    speaker: a ? a.speaker : l.unmonitored_speaker,
    priority,
    needsPriority: !l.follow_unmonitored,
  };
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
