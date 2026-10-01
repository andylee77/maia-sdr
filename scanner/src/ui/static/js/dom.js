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
