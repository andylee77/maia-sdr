// The Aircraft module's Map page: the aircraft heard that have said where they are, each drawn
// at its place and pointing along its track, with the way it has come since the page was
// opened. The map moves and zooms (drag, the wheel, the buttons, the arrow keys); Fit brings
// every aircraft into view and keeps them there until the map is moved. A click picks an
// aircraft, and what it last said is written under the map.
//
// The map's own drawing is a grid of latitude and longitude and a scale: the radio holds no
// map. "Map tiles" lays OpenStreetMap's tiles under it, which this browser then fetches from
// the Internet; it is off until switched on, and the choice is this browser's. There are no
// range rings: the radio does not know where it is.

import { h, toast, setText, setClass, icon } from '../dom.js';
import { api } from '../api.js';
import { want, boardNow } from '../store.js';
import { ago, num, DASH } from '../format.js';

const TILE = 256;
const [MIN_ZOOM, MAX_ZOOM] = [3, 13];
const TILES = 'https://tile.openstreetmap.org/{z}/{x}/{y}.png';
// Places kept of an aircraft's way while the page is open.
const TRAIL = 240;
// How near a click must be to an aircraft to pick it, pixels.
const NEAR = 16;
const KEPT = { tiles: 'aircraft.map.tiles', view: 'aircraft.map.view' };
// The grid's steps, degrees: the first that puts its lines 90 pixels apart or more is used.
const GRID = [0.05, 0.1, 0.25, 0.5, 1, 2, 5, 10, 20];
// The scale's lengths, nautical miles.
const SCALE = [0.5, 1, 2, 5, 10, 20, 50, 100, 200, 500];

// Web Mercator: a place in pixels of the whole world at a zoom, and back.
const world = zoom => TILE * 2 ** zoom;
const toPx = (lat, lon, zoom) => [(lon + 180) / 360 * world(zoom), (1 - Math.asinh(Math.tan(lat * Math.PI / 180)) / Math.PI) / 2 * world(zoom)];
const toPlace = (x, y, zoom) => [Math.atan(Math.sinh(Math.PI * (1 - 2 * y / world(zoom)))) * 180 / Math.PI, x / world(zoom) * 360 - 180];

function kept(key) {
  try { return localStorage.getItem(key); } catch { return null; }
}
function keep(key, value) {
  try { localStorage.setItem(key, value); } catch { /* this browser keeps nothing */ }
}

const degrees = (v, pos, neg, step) => `${Math.abs(v).toFixed(step < 1 ? (step < 0.1 ? 2 : 1) : 0)}°${v < 0 ? neg : pos}`;

function details(a) {
  const parts = [a.callsign || a.icao, a.callsign ? a.icao : null, `${a.link} MHz`,
    a.altitude_ft === null ? null : `${num(a.altitude_ft)} ft`,
    a.speed_kt === null ? null : `${num(Math.round(a.speed_kt))} kt on ${String(Math.round(a.track_deg) % 360).padStart(3, '0')}°`,
    a.vertical_fpm ? `${a.vertical_fpm > 0 ? '+' : '−'}${num(Math.abs(a.vertical_fpm))} ft/min` : null,
    a.lat === null ? 'no place said' : `${a.lat.toFixed(4)}, ${a.lon.toFixed(4)}`,
    `heard ${ago(Math.max(0, boardNow() - a.last_unix_ms))}`];
  return parts.filter(Boolean).join(' · ');
}

export function mount(el) {
  let view = null;
  let gone = false;
  // The middle of the map, its zoom, and whether it keeps every aircraft in view.
  let center = { lat: 0, lon: 0 };
  let zoom = MIN_ZOOM;
  let fitting = true;
  let tilesOn = kept(KEPT.tiles) === 'on';
  let picked = null;
  // Each aircraft's way since the page was opened: [lat, lon], oldest first.
  const trails = new Map();
  try {
    const was = JSON.parse(kept(KEPT.view) || 'null');
    if (was && Number.isFinite(was.lat) && Number.isFinite(was.lon) && Number.isFinite(was.zoom)) {
      center = { lat: was.lat, lon: was.lon };
      zoom = Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, Math.round(was.zoom)));
      fitting = was.fitting !== false;
    }
  } catch { /* kept by another version: the map starts on the aircraft */ }

  // ── The bar: how many are on the map, and the map's controls ──
  const count = h('span', { class: 'dim', 'aria-live': 'polite' });
  const fitBtn = h('button', { class: 'btn small', type: 'button', title: 'Bring every aircraft into view, and keep them there', text: 'Fit' });
  const outBtn = h('button', { class: 'btn small', type: 'button', 'aria-label': 'Zoom out', title: 'Zoom out', text: '−' });
  const inBtn = h('button', { class: 'btn small', type: 'button', 'aria-label': 'Zoom in', title: 'Zoom in', text: '+' });
  const tilesBtn = h('button', { class: 'btn small', type: 'button', 'aria-pressed': 'false',
    title: 'Lay OpenStreetMap\'s map under the aircraft. This browser fetches it from the Internet (tile.openstreetmap.org), which learns what part of the map is looked at.' },
  icon('card', 14), h('span', { text: 'Map tiles' }));
  const bar = h('div', { class: 'map-bar' }, count, h('div', { class: 'spacer' }), fitBtn, outBtn, inBtn, tilesBtn);

  // ── The map: the tiles when they are on, the drawing over them ──
  const tiles = h('div', { class: 'map-tiles', 'aria-hidden': 'true' });
  const canvas = h('canvas', { class: 'map-canvas' });
  const credit = h('div', { class: 'map-credit', text: '© OpenStreetMap contributors' });
  const map = h('div', { class: 'map-view', tabindex: '0', role: 'application', 'aria-label': 'The aircraft heard, on a map' }, tiles, canvas, credit);
  const says = h('p', { class: 'card-note map-says' });
  const wrap = h('section', { class: 'card map-page' }, bar, map, says);
  el.append(wrap);
  const ctx = canvas.getContext('2d');
  // The map's inside, which the drawing and the tiles fill, and a pointer's place in it.
  let [width, height] = [0, 0];
  const pointer = e => {
    const box = map.getBoundingClientRect();
    return [e.clientX - box.left - map.clientLeft, e.clientY - box.top - map.clientTop];
  };

  const placed = () => (view ? view.aircraft.filter(a => a.lat !== null) : []);
  // A place on the screen, and the place at a point of the screen.
  const middle = () => toPx(center.lat, center.lon, zoom);
  const onScreen = (lat, lon) => {
    const [mx, my] = middle();
    const [x, y] = toPx(lat, lon, zoom);
    return [x - mx + width / 2, y - my + height / 2];
  };
  const atScreen = (x, y) => {
    const [mx, my] = middle();
    return toPlace(mx + x - width / 2, my + y - height / 2, zoom);
  };

  // Where the map is, and whether it follows the aircraft, is kept for the next visit.
  const keepView = () => keep(KEPT.view, JSON.stringify({ lat: center.lat, lon: center.lon, zoom, fitting }));
  function moveTo(lat, lon, z) {
    zoom = Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, z));
    center = { lat: Math.max(-85, Math.min(85, lat)), lon: ((lon + 540) % 360) - 180 };
    keepView();
  }

  // Every aircraft in view: the closest zoom that holds them all with a margin.
  function fit() {
    const all = placed();
    if (!all.length || !width) return;
    const lats = all.map(a => a.lat);
    const lons = all.map(a => a.lon);
    const [south, north, west, east] = [Math.min(...lats), Math.max(...lats), Math.min(...lons), Math.max(...lons)];
    let z = MAX_ZOOM;
    for (; z > MIN_ZOOM; z--) {
      const [x0, y0] = toPx(north, west, z);
      const [x1, y1] = toPx(south, east, z);
      if (x1 - x0 <= width - 120 && y1 - y0 <= height - 120) break;
    }
    const [x0, y0] = toPx(north, west, z);
    const [x1, y1] = toPx(south, east, z);
    const [lat, lon] = toPlace((x0 + x1) / 2, (y0 + y1) / 2, z);
    moveTo(lat, lon, z);
  }

  function zoomBy(by, x = width / 2, y = height / 2) {
    const z = Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, zoom + by));
    if (z === zoom) return;
    // The place under the pointer stays under it.
    const [lat, lon] = atScreen(x, y);
    const [px, py] = toPx(lat, lon, z);
    const [clat, clon] = toPlace(px - (x - width / 2), py - (y - height / 2), z);
    fitting = false;
    moveTo(clat, clon, z);
    draw();
  }

  function layTiles() {
    credit.hidden = !tilesOn;
    if (!tilesOn || !width) {
      tiles.replaceChildren();
      return;
    }
    const [mx, my] = middle();
    const [left, top] = [mx - width / 2, my - height / 2];
    const across = 2 ** zoom;
    const want = new Map();
    for (let tx = Math.floor(left / TILE); tx * TILE < left + width; tx++) {
      for (let ty = Math.floor(top / TILE); ty * TILE < top + height; ty++) {
        if (ty < 0 || ty >= across) continue;
        // East of the date line the world begins again.
        const x = ((tx % across) + across) % across;
        want.set(`${zoom}/${tx}/${ty}`, { src: TILES.replace('{z}', zoom).replace('{x}', x).replace('{y}', ty), left: tx * TILE - left, top: ty * TILE - top });
      }
    }
    for (const img of Array.from(tiles.children)) {
      const to = want.get(img.dataset.key);
      if (!to) img.remove();
      else {
        img.style.transform = `translate(${Math.round(to.left)}px, ${Math.round(to.top)}px)`;
        want.delete(img.dataset.key);
      }
    }
    for (const [key, to] of want) {
      const img = h('img', { alt: '', draggable: 'false', referrerpolicy: 'origin', dataset: { key } });
      img.style.transform = `translate(${Math.round(to.left)}px, ${Math.round(to.top)}px)`;
      img.src = to.src;
      tiles.append(img);
    }
  }

  function draw() {
    if (gone || !width) return;
    const css = getComputedStyle(map);
    const colour = name => css.getPropertyValue(name).trim();
    const [text, dim, line, accent, warn] = ['--text', '--text-dim', '--line-strong', '--accent', '--warn'].map(colour);
    layTiles();
    ctx.setTransform(devicePixelRatio, 0, 0, devicePixelRatio, 0, 0);
    ctx.clearRect(0, 0, width, height);
    ctx.font = '12px system-ui, sans-serif';
    ctx.lineWidth = 1;

    // The grid: lines of latitude and longitude, named at the map's left and top.
    const [north, west] = atScreen(0, 0);
    const [south, east] = atScreen(width, height);
    const step = GRID.find(s => s / (east - west) * width >= 90) || GRID[GRID.length - 1];
    ctx.strokeStyle = line;
    ctx.fillStyle = dim;
    ctx.globalAlpha = tilesOn ? 0.35 : 0.7;
    ctx.beginPath();
    for (let lon = Math.ceil(west / step) * step; lon <= east; lon += step) {
      const [x] = onScreen(center.lat, lon);
      ctx.moveTo(Math.round(x) + 0.5, 0);
      ctx.lineTo(Math.round(x) + 0.5, height);
    }
    for (let lat = Math.ceil(south / step) * step; lat <= north; lat += step) {
      const [, y] = onScreen(lat, center.lon);
      ctx.moveTo(0, Math.round(y) + 0.5);
      ctx.lineTo(width, Math.round(y) + 0.5);
    }
    ctx.stroke();
    ctx.globalAlpha = 1;
    if (!tilesOn) {
      for (let lon = Math.ceil(west / step) * step; lon <= east; lon += step) ctx.fillText(degrees(lon, 'E', 'W', step), onScreen(center.lat, lon)[0] + 4, 14);
      for (let lat = Math.ceil(south / step) * step; lat <= north; lat += step) {
        // The scale has the map's bottom corner.
        const [, y] = onScreen(lat, center.lon);
        if (y < height - 36) ctx.fillText(degrees(lat, 'N', 'S', step), 4, y - 4);
      }
    }

    // The scale: a round number of nautical miles, at the map's middle.
    const metres = 40075016.686 * Math.cos(center.lat * Math.PI / 180) / world(zoom);
    const miles = SCALE.filter(n => n * 1852 / metres <= 160).pop() || SCALE[0];
    const long = miles * 1852 / metres;
    ctx.strokeStyle = text;
    ctx.fillStyle = text;
    ctx.beginPath();
    ctx.moveTo(12.5, height - 22);
    ctx.lineTo(12.5, height - 14.5);
    ctx.lineTo(12.5 + long, height - 14.5);
    ctx.lineTo(12.5 + long, height - 22);
    ctx.stroke();
    ctx.fillText(`${miles} NM`, 18, height - 20);

    // Each aircraft's way, then the aircraft over the ways.
    const all = placed();
    ctx.lineWidth = 1.5;
    for (const a of all) {
      const way = trails.get(a.icao);
      if (!way || way.length < 2) continue;
      ctx.strokeStyle = a.icao === picked ? text : dim;
      ctx.globalAlpha = a.icao === picked ? 0.9 : 0.5;
      ctx.beginPath();
      way.forEach(([lat, lon], k) => {
        const [x, y] = onScreen(lat, lon);
        if (k) ctx.lineTo(x, y);
        else ctx.moveTo(x, y);
      });
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
    const shown = [];
    for (const a of all) {
      const [x, y] = onScreen(a.lat, a.lon);
      if (x < -20 || y < -20 || x > width + 20 || y > height + 20) continue;
      const mine = a.icao === picked;
      shown.push({ a, x, y, mine });
      ctx.fillStyle = mine ? text : a.link === '978' ? warn : accent;
      ctx.strokeStyle = colour('--bg');
      ctx.lineWidth = 1;
      ctx.save();
      ctx.translate(x, y);
      ctx.beginPath();
      if (a.track_deg === null) ctx.arc(0, 0, 4.5, 0, 2 * Math.PI);
      else {
        // An arrowhead along its track.
        ctx.rotate(a.track_deg * Math.PI / 180);
        ctx.moveTo(0, -9);
        ctx.lineTo(6, 7);
        ctx.lineTo(0, 3.5);
        ctx.lineTo(-6, 7);
        ctx.closePath();
      }
      ctx.fill();
      ctx.stroke();
      ctx.restore();
    }
    // Their names: the picked aircraft's first, then each that has room beside it, with no
    // other aircraft and no other name there.
    const named = shown.map(s => [s.x - 8, s.y - 8, 16, 16]);
    ctx.lineWidth = 3;
    ctx.strokeStyle = colour('--bg');
    for (const { a, x, y, mine } of [...shown.filter(s => s.mine), ...shown.filter(s => !s.mine)]) {
      const name = `${a.callsign || a.icao}${a.altitude_ft === null ? '' : ` ${Math.round(a.altitude_ft / 100)}`}`;
      const box = [x + 9, y - 9, ctx.measureText(name).width + 5, 16];
      if (!mine && named.some(n => box[0] < n[0] + n[2] && n[0] < box[0] + box[2] && box[1] < n[1] + n[3] && n[1] < box[1] + box[3])) continue;
      named.push(box);
      ctx.strokeText(name, x + 10, y + 4);
      ctx.fillStyle = mine ? text : dim;
      ctx.fillText(name, x + 10, y + 4);
    }
    const drawn = shown.length;

    const listed = view ? view.aircraft.length : 0;
    setText(count, !view ? '' : `${num(all.length)} of ${num(listed)} aircraft have said where they are${view.state === 'listening' ? '' : ' · not listening: another module has the radio'}`);
    const one = view && view.aircraft.find(a => a.icao === picked);
    setText(says, one ? details(one) : all.length ? 'Click an aircraft for what it last said. A name\'s number is its altitude in hundreds of feet; a name is left out where another is in its way. A way is drawn from when this page was opened.' : 'No aircraft has said where it is yet.');
    setClass(tilesBtn, 'lit', tilesOn);
    tilesBtn.setAttribute('aria-pressed', String(tilesOn));
    Object.assign(map.dataset, { zoom: String(zoom), center: `${center.lat.toFixed(4)},${center.lon.toFixed(4)}`, aircraft: String(drawn), placed: String(all.length), fitting: String(fitting), tiles: String(tiles.children.length), picked: picked || '' });
  }

  // What the radio pushed: each aircraft's way grows where it moved, and the gone are dropped.
  function heard() {
    const here = new Set();
    for (const a of placed()) {
      here.add(a.icao);
      const way = trails.get(a.icao) || [];
      const last = way[way.length - 1];
      if (!last || last[0] !== a.lat || last[1] !== a.lon) way.push([a.lat, a.lon]);
      if (way.length > TRAIL) way.shift();
      trails.set(a.icao, way);
    }
    for (const icao of Array.from(trails.keys())) if (!here.has(icao)) trails.delete(icao);
    if (picked && !view.aircraft.some(a => a.icao === picked)) picked = null;
    if (fitting) fit();
    draw();
  }

  // ── Moving the map ──
  let drag = null;
  map.addEventListener('pointerdown', e => {
    if (e.button !== 0) return;
    drag = { x: e.clientX, y: e.clientY, moved: false, from: middle() };
    map.setPointerCapture(e.pointerId);
  });
  map.addEventListener('pointermove', e => {
    if (!drag) return;
    const [dx, dy] = [e.clientX - drag.x, e.clientY - drag.y];
    if (!drag.moved && Math.hypot(dx, dy) < 4) return;
    drag.moved = true;
    fitting = false;
    const [lat, lon] = toPlace(drag.from[0] - dx, drag.from[1] - dy, zoom);
    moveTo(lat, lon, zoom);
    draw();
  });
  map.addEventListener('pointerup', e => {
    if (!drag) return;
    const was = drag;
    drag = null;
    if (was.moved) return;
    // A click: the aircraft nearest it, if one is near.
    const [x, y] = pointer(e);
    let best = null;
    for (const a of placed()) {
      const [ax, ay] = onScreen(a.lat, a.lon);
      const far = Math.hypot(ax - x, ay - y);
      if (far <= NEAR && (!best || far < best.far)) best = { far, icao: a.icao };
    }
    picked = best ? best.icao : null;
    draw();
  });
  map.addEventListener('pointercancel', () => { drag = null; });
  map.addEventListener('wheel', e => {
    e.preventDefault();
    zoomBy(e.deltaY < 0 ? 1 : -1, ...pointer(e));
  }, { passive: false });
  map.addEventListener('keydown', e => {
    const step = { ArrowLeft: [-80, 0], ArrowRight: [80, 0], ArrowUp: [0, -80], ArrowDown: [0, 80] }[e.key];
    if (step) {
      const [mx, my] = middle();
      const [lat, lon] = toPlace(mx + step[0], my + step[1], zoom);
      fitting = false;
      moveTo(lat, lon, zoom);
      draw();
    } else if (e.key === '+' || e.key === '=') zoomBy(1);
    else if (e.key === '-') zoomBy(-1);
    else return;
    e.preventDefault();
  });
  inBtn.addEventListener('click', () => zoomBy(1));
  outBtn.addEventListener('click', () => zoomBy(-1));
  fitBtn.addEventListener('click', () => {
    fitting = true;
    keepView();
    fit();
    draw();
  });
  tilesBtn.addEventListener('click', () => {
    tilesOn = !tilesOn;
    keep(KEPT.tiles, tilesOn ? 'on' : 'off');
    draw();
  });

  const sized = new ResizeObserver(() => {
    [width, height] = [map.clientWidth, map.clientHeight];
    canvas.width = Math.round(width * devicePixelRatio);
    canvas.height = Math.round(height * devicePixelRatio);
    if (fitting) fit();
    draw();
  });
  sized.observe(map);

  const stop = want({ adsb: true }, m => {
    view = m.adsb;
    heard();
  });
  api.adsb().then(v => {
    if (gone || (view && view.rev >= v.rev)) return;
    view = v;
    heard();
  }).catch(e => { if (!gone) toast(e.message, true); });

  return {
    // The ages in the picked aircraft's line move on between two pushes.
    update() { if (picked) draw(); },
    unmount() {
      gone = true;
      stop();
      sized.disconnect();
    },
  };
}
