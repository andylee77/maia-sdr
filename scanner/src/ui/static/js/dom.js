// Tiny DOM helpers. Everything is built with createElement +
// textContent, never innerHTML with data, so names and aliases coming
// from the radio can never inject markup.

// h('div', {class: 'x', onclick: fn, dataset: {k: 'v'}}, child, 'text', ...)
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v === undefined || v === null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k === 'text') el.textContent = v;
      else if (k === 'dataset') Object.assign(el.dataset, v);
      else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
      else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
      else if (v === true) el.setAttribute(k, '');
      else el.setAttribute(k, String(v));
    }
  }
  append(el, children);
  return el;
}

// Line icons drawn on a 24-unit grid in the text colour.
const SVG = 'http://www.w3.org/2000/svg';
const ICONS = {
  play: [['path', { d: 'M8 5.5v13l10.5-6.5z', fill: 'currentColor', stroke: 'none' }]],
  stop: [['rect', { x: '6.5', y: '6.5', width: '11', height: '11', rx: '1.5', fill: 'currentColor', stroke: 'none' }]],
  trash: [['path', { d: 'M4 7h16M9 7V4.5h6V7M6.5 7l1 13h9l1-13M10 11v5.5M14 11v5.5' }]],
  tune: [['path', { d: 'M4 7h3M11 7h9M4 17h9M17 17h3' }], ['circle', { cx: '9', cy: '7', r: '2' }], ['circle', { cx: '15', cy: '17', r: '2' }]],
  download: [['path', { d: 'M12 4v11M7 10.5l5 5 5-5M5 20h14' }]],
  speaker: [['path', { d: 'M4 9.5h3.5L12 5.5v13l-4.5-4H4zM15.5 9a4.5 4.5 0 010 6M18 6.5a8 8 0 010 11' }]],
  muted: [['path', { d: 'M4 9.5h3.5L12 5.5v13l-4.5-4H4zM16 9.5l5 5M21 9.5l-5 5' }]],
  bell: [['path', { d: 'M6.5 16.5V11a5.5 5.5 0 0111 0v5.5l1.5 2h-14zM10 20.5a2 2 0 004 0' }]],
  info: [['circle', { cx: '12', cy: '12', r: '9' }], ['path', { d: 'M12 11v6M12 7.5v.5' }]],
};

export function icon(name) {
  const make = (tag, attrs) => {
    const el = document.createElementNS(SVG, tag);
    for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
    return el;
  };
  const svg = make('svg', {
    viewBox: '0 0 24 24', width: '16', height: '16', fill: 'none', stroke: 'currentColor',
    'stroke-width': '2', 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true',
  });
  for (const [tag, attrs] of ICONS[name]) svg.appendChild(make(tag, attrs));
  return svg;
}

// A button that is one symbol; its name is the tooltip and the label.
export function iconButton(name, label, { cls = '', onclick, disabled } = {}) {
  return h('button', { class: `btn small icon ${cls}`.trim(), type: 'button', title: label, 'aria-label': label, onclick, disabled }, icon(name));
}

function append(el, children) {
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    if (Array.isArray(c)) append(el, c);
    else if (c instanceof Node) el.appendChild(c);
    else el.appendChild(document.createTextNode(String(c)));
  }
}

// Set text only when it changed (avoids needless layout).
export function setText(el, text) {
  const t = text === null || text === undefined ? '' : String(text);
  if (el && el.textContent !== t) el.textContent = t;
}

export function setClass(el, name, on) {
  if (el) el.classList.toggle(name, !!on);
}

export function clear(el) {
  while (el && el.firstChild) el.removeChild(el.firstChild);
}

// Keyed list update: keeps existing rows (and any <audio> in them)
// whose key is still present, creates new ones, removes gone ones and
// re-orders to match `items`. create(item) -> element; update(el, item).
export function keyedList(container, items, keyFn, create, update) {
  const existing = new Map();
  for (const el of Array.from(container.children)) {
    if (el.dataset && el.dataset.key !== undefined) existing.set(el.dataset.key, el);
    else el.remove();
  }
  let prev = null;
  for (const item of items) {
    const key = String(keyFn(item));
    let el = existing.get(key);
    if (el) existing.delete(key);
    else {
      el = create(item);
      el.dataset.key = key;
    }
    update(el, item);
    const want = prev ? prev.nextSibling : container.firstChild;
    if (el !== want) container.insertBefore(el, want);
    prev = el;
  }
  for (const stale of existing.values()) stale.remove();
}

// A table with a header row.
export function table(head, rows) {
  return h('div', { class: 'table-wrap' },
    h('table', { class: 'table' },
      h('thead', null, h('tr', null, ...head.map(t => h('th', { text: t })))),
      h('tbody', null, ...rows)));
}

// Card with a header (title + optional right-side nodes) and a body.
export function card(title, opts = {}) {
  const right = h('div', { class: 'row' });
  const head = h('div', { class: 'card-head' }, h('h2', { text: title }), h('div', { class: 'spacer' }), right);
  const body = h('div', { class: 'card-body' });
  const el = h('section', { class: 'card' + (opts.flush ? ' flush' : '') + (opts.class ? ' ' + opts.class : '') }, head, body);
  return { el, head, right, body };
}

let toastTimer = null;
export function toast(msg, isError = false) {
  const el = document.getElementById('toast');
  if (!el) return;
  el.textContent = msg;
  el.classList.toggle('error', isError);
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, isError ? 6000 : 3000);
}

// Checkbox rendered as a switch; returns {el, input}.
export function switchInput(labelText, checked, onChange) {
  const input = h('input', { type: 'checkbox', role: 'switch' });
  input.checked = !!checked;
  input.addEventListener('change', () => onChange(input.checked));
  return { el: h('label', { class: 'switch' }, input, h('span', { text: labelText })), input };
}
