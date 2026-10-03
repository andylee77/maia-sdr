// Alerts-only listening. A lane's call plays when its talkgroup gave an alert tone (a console's
// warble or beep, a two-tone page; the radio finds them, see /ws/audio) within the window before
// the call began, or when the call gives one itself. Until then its audio is held back (the last
// 6 s), so a call that turns out to carry an alert plays from its start, tone included; the
// lane's next call drops what was held.
//
// The class is shared by the audio Worker (its source text is injected, see sources.js, so it may
// use no imports or globals) and the ScriptProcessor fallback. Times are Date.now() ms.

export class AlertGate {
  constructor() {
    this.on = false;
    this.windowMs = 120000;
    // Talkgroup -> when its window ends.
    this.until = new Map();
    this.lanes = [0, 1].map(() => ({ call: null, open: true, held: [] }));
  }

  setMode(on, windowMs) {
    this.on = !!on;
    this.windowMs = windowMs;
    if (!this.on) {
      for (const l of this.lanes) {
        l.open = true;
        l.held = [];
      }
    }
  }

  // A lane's audio now belongs to `call` on `tg`.
  call(lane, tg, call, now) {
    const l = this.lanes[lane];
    if (l.call === call) return;
    l.call = call;
    l.held = [];
    l.open = !this.on || (this.until.get(tg) || 0) > now;
  }

  // An alert tone in a lane's call opens its talkgroup for the window. Returns the call's audio
  // held back so far, to play now.
  alert(lane, tg, call, now) {
    this.until.set(tg, now + this.windowMs);
    const l = this.lanes[lane];
    if (l.call !== call || l.open) return [];
    l.open = true;
    const held = l.held;
    l.held = [];
    return held;
  }

  // A 20 ms block of a lane's audio: true to play it now, else it is held back.
  pass(lane, block) {
    const l = this.lanes[lane];
    if (l.open) return true;
    l.held.push(block);
    if (l.held.length > 300) l.held.shift();
    return false;
  }
}
