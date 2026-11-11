// The Access card (the radio's Settings): who may reach this radio. With no password set it is
// open and the card offers to set the administrator's; after that it shows who is signed in
// here, and to the administrator the passwords, the tokens for tools, the browsers signed in and
// the names the radio answers to. A listener sees only that they are one.

import { h, toast, table } from '../dom.js';
import { api } from '../api.js';
import { dayTime } from '../format.js';
import { row, foot, section } from './settings_parts.js';

const LEVEL = { admin: 'Administrator', listener: 'Listener' };
const MIN = 8;

const secret = label => h('input', { class: 'input', type: 'password', autocomplete: 'new-password', 'aria-label': label });

// The new password typed twice; throws what is wrong with it.
function typedTwice(first, again) {
  if (first.value.length < MIN) throw new Error(`A password is at least ${MIN} characters`);
  if (first.value !== again.value) throw new Error('The two do not match');
  return first.value;
}

// A browser as a few words: what it calls itself is long.
function browser(agent) {
  const name = ['Firefox', 'Edg', 'Chrome', 'Safari', 'curl', 'python'].find(n => agent.includes(n));
  const where = ['Windows', 'Android', 'iPhone', 'iPad', 'Mac OS', 'Linux'].find(n => agent.includes(n));
  return name ? `${name === 'Edg' ? 'Edge' : name}${where ? ` on ${where}` : ''}` : agent.slice(0, 40);
}

// `reload()` reads the page's settings again; `signedOut()` is called when this browser's
// session ended here.
export function accessCard(reload) {
  const sec = section('Access');
  const body = h('div');
  sec.body.append(body);
  // Kept across the card's redraws: a token just made is shown until the page is left.
  let made = null;

  const act = async (work, done) => {
    try {
      await work();
      if (done) toast(done);
      reload();
    } catch (e) {
      toast(e.message, true);
    }
  };

  function open() {
    const first = secret('New password');
    const again = secret('The password again');
    const set = h('button', { class: 'btn primary', type: 'button', text: 'Set the password' });
    set.addEventListener('click', () => act(async () => {
      await api.setPassword({ level: 'admin', new: typedTwice(first, again) });
      // This browser is signed in by it: the page starts again as the administrator's.
      location.reload();
    }));
    return [
      h('p', { class: 'access-state warn', text: 'No password is set: anyone who can reach this radio on the network can listen to it and change it.' }),
      row('The administrator\'s password', first, `At least ${MIN} characters.`),
      row('…again', again),
      foot('Once it is set, every browser and tool signs in. A password that is lost: put an empty file named scanner-reset-access on the card and restart the radio.', set),
    ];
  }

  function listener() {
    return [h('p', { class: 'access-state', text: 'Signed in as a listener: you see and hear this radio. Its administrator changes it.' })];
  }

  function admin(a) {
    const out = [h('p', { class: 'access-state', text: 'Signed in as the administrator.' })];

    // The administrator's password.
    const current = h('input', { class: 'input', type: 'password', autocomplete: 'current-password', 'aria-label': 'The password now' });
    const first = secret('New administrator\'s password');
    const again = secret('The new password again');
    const change = h('button', { class: 'btn', type: 'button', text: 'Change' });
    change.addEventListener('click', () => act(() => api.setPassword({ level: 'admin', current: current.value, new: typedTwice(first, again) }), 'Password changed; the other browsers signed in with the old one are signed out'));
    out.push(
      h('h3', { class: 'access-part', text: 'The administrator\'s password' }),
      row('The password now', current),
      row('The new one', [first, again], 'Typed twice.'),
      foot('', change));

    // The listener's.
    const lFirst = secret('Listener\'s password');
    const lAgain = secret('The listener\'s password again');
    const lSet = h('button', { class: 'btn', type: 'button', text: a.listener ? 'Change' : 'Set' });
    lSet.addEventListener('click', () => act(() => api.setPassword({ level: 'listener', new: typedTwice(lFirst, lAgain) }), a.listener ? 'The listener\'s password changed' : 'The listener\'s password is set'));
    const lRemove = h('button', { class: 'btn danger', type: 'button', text: 'Remove', hidden: !a.listener });
    lRemove.addEventListener('click', () => {
      if (confirm('Remove the listener\'s password? Listeners signed in are signed out.')) act(() => api.setPassword({ level: 'listener', new: null }), 'The listener\'s password is removed');
    });
    out.push(
      h('h3', { class: 'access-part', text: 'A listener' }),
      row(a.listener ? 'A new password for listeners' : 'A password for listeners', [lFirst, lAgain], 'Whoever signs in with it sees and hears this radio, and changes nothing.'),
      foot(a.listener ? 'A listener\'s password is set.' : 'None is set: only the administrator signs in.', lRemove, lSet));

    // Tokens.
    const label = h('input', { class: 'input', type: 'text', maxlength: 60, placeholder: 'what uses it', 'aria-label': 'Token\'s name' });
    const level = h('select', { class: 'input', 'aria-label': 'Token\'s level' }, h('option', { value: 'listener', text: 'Listener: reads' }), h('option', { value: 'admin', text: 'Administrator: everything' }));
    const make = h('button', { class: 'btn', type: 'button', text: 'Make a token' });
    make.addEventListener('click', () => act(async () => {
      made = await api.createToken(label.value, level.value);
    }));
    out.push(h('h3', { class: 'access-part', text: 'Tokens for tools' }));
    if (made) {
      const shown = h('input', { class: 'input mono access-token', type: 'text', readonly: true, value: made.token, 'aria-label': 'The new token' });
      shown.addEventListener('focus', () => shown.select());
      out.push(h('div', { class: 'access-made' },
        h('p', { class: 'card-note', text: `The token for "${made.label}", shown this once: copy it now. A tool sends it as the header Authorization: Bearer <token>.` }), shown));
    }
    if (a.tokens.length) {
      out.push(table(['For', 'Level', 'Made', ''], a.tokens.map(t => h('tr', null,
        h('td', { text: t.label }), h('td', { text: LEVEL[t.level] }), h('td', { text: dayTime(t.created_unix_ms) }),
        h('td', { class: 'files-get' }, h('button', {
          class: 'btn small danger', type: 'button', text: 'Revoke',
          onclick: () => { if (confirm(`Revoke the token for "${t.label}"? What uses it is shut out at once.`)) act(() => api.revokeToken(t.id), 'Token revoked'); },
        }))))));
    }
    out.push(row('A new token', [label, level, make], 'For a script, the bench, an agent or the panel: it reaches the radio without a password.'));

    // Browsers signed in.
    const others = a.sessions.filter(s => !s.current).length;
    out.push(
      h('h3', { class: 'access-part', text: 'Browsers signed in' }),
      table(['Browser', 'As', 'Since', ''], a.sessions.map(s => h('tr', null,
        h('td', { text: browser(s.agent), title: s.agent }), h('td', { text: LEVEL[s.level] }), h('td', { text: dayTime(s.created_unix_ms) }),
        h('td', { class: 'files-get' }, s.current ? h('span', { class: 'dim', text: 'this one' })
          : h('button', { class: 'btn small', type: 'button', text: 'Sign out', onclick: () => act(() => api.endSession(s.id), 'Signed out') }))))),
      foot('A browser stays signed in until it signs out, its password changes, or it is signed out here.',
        h('button', { class: 'btn', type: 'button', text: 'Sign out the others', disabled: !others, onclick: () => act(() => api.endOtherSessions(), 'The others are signed out') })));

    // Names.
    const names = h('input', { class: 'input access-names', type: 'text', value: a.names.join(', '), placeholder: 'none', 'aria-label': 'Other names' });
    const saveNames = h('button', { class: 'btn', type: 'button', text: 'Save' });
    saveNames.addEventListener('click', () => act(() => api.setNames(names.value.split(',').map(n => n.trim()).filter(Boolean)), 'Names saved'));
    out.push(
      h('h3', { class: 'access-part', text: 'Names' }),
      row('Other names this radio answers to', [names, saveNames],
        `It answers to its addresses${a.host_name ? `, to ${a.host_name} and ${a.host_name}.local` : ''}. A request sent to any other name is refused, so a web page elsewhere cannot point a name at it. Names are separated by commas.`));

    // Open again.
    const proof = h('input', { class: 'input', type: 'password', autocomplete: 'current-password', 'aria-label': 'The administrator\'s password, to remove it' });
    const remove = h('button', { class: 'btn danger', type: 'button', text: 'Remove the passwords' });
    remove.addEventListener('click', () => {
      if (!confirm('Remove every password, session and token? Anyone who can reach this radio can then listen to it and change it.')) return;
      act(async () => {
        await api.setPassword({ level: 'admin', current: proof.value, new: null });
        location.reload();
      });
    });
    out.push(
      h('h3', { class: 'access-part', text: 'Open the radio again' }),
      row('The administrator\'s password', [proof, remove], 'Removes every password, session and token.'));
    return out;
  }

  return {
    el: sec.el,
    show({ me, access }) {
      // What is being typed is not thrown away by a read that changes nothing.
      const key = JSON.stringify([me, access, made && made.id]);
      if (key === body.dataset.key) return;
      body.dataset.key = key;
      body.replaceChildren(...(!me || !me.protected ? open() : me.level === 'admin' && access ? admin(access) : listener()));
    },
  };
}
