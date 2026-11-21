import init, { Emulator } from './pkg/gba_web.js';

const W = 240, H = 160;
const FRAME_MS = 1000 / 59.7275;

// Порядок = биты KEYINPUT: A B Select Start Right Left Up Down R L
const BUTTONS = ['A', 'B', 'Select', 'Start', 'Вправо', 'Влево', 'Вверх', 'Вниз', 'R', 'L'];

// key — KeyboardEvent.code (не зависит от раскладки), pad — индекс кнопки Gamepad API
const DEFAULTS = [
  { key: 'KeyX', pad: 1 },
  { key: 'KeyZ', pad: 0 },
  { key: 'ShiftRight', pad: 8 },
  { key: 'Enter', pad: 9 },
  { key: 'ArrowRight', pad: 15 },
  { key: 'ArrowLeft', pad: 14 },
  { key: 'ArrowUp', pad: 12 },
  { key: 'ArrowDown', pad: 13 },
  { key: 'KeyS', pad: 5 },
  { key: 'KeyA', pad: 4 },
];

const STORE = 'gba-binds-v1';
let binds = loadBinds();
let listening = null; // { i, kind: 'key' | 'pad' }

function loadBinds() {
  try {
    const b = JSON.parse(localStorage.getItem(STORE));
    if (Array.isArray(b) && b.length === BUTTONS.length) return b;
  } catch {}
  return structuredClone(DEFAULTS);
}
function saveBinds() {
  try { localStorage.setItem(STORE, JSON.stringify(binds)); } catch {}
}

// ---------- UI биндов ----------
const tbody = document.getElementById('binds');

const ARROWS = { ArrowRight: '→', ArrowLeft: '←', ArrowUp: '↑', ArrowDown: '↓' };
function keyName(code) {
  return ARROWS[code] ?? code.replace(/^Key|^Digit/, '');
}

function cell(i, kind) {
  const td = document.createElement('td');
  const b = document.createElement('button');
  const on = listening?.i === i && listening.kind === kind;
  const v = binds[i][kind];
  b.className = 'btn' + (on ? ' listening' : '');
  b.textContent = on ? 'Нажмите…' : v == null ? '—' : kind === 'key' ? keyName(v) : `Кнопка ${v}`;
  b.onclick = () => { listening = on ? null : { i, kind }; render(); };
  td.append(b);
  return td;
}

function render() {
  tbody.replaceChildren(...BUTTONS.map((name, i) => {
    const tr = document.createElement('tr');
    const th = document.createElement('th');
    th.textContent = name;
    tr.append(th, cell(i, 'key'), cell(i, 'pad'));
    return tr;
  }));
}

function assign(value) {
  const { i, kind } = listening;
  // один ввод — одна кнопка: снимаем его с остальных
  if (value != null) binds.forEach(b => { if (b[kind] === value) b[kind] = null; });
  binds[i][kind] = value;
  listening = null;
  saveBinds();
  render();
}

document.getElementById('reset').onclick = () => {
  binds = structuredClone(DEFAULTS);
  listening = null;
  saveBinds();
  render();
};

// ---------- Ввод ----------
const held = new Set();

addEventListener('keydown', e => {
  if (listening?.kind === 'key') {
    e.preventDefault();
    if (e.code === 'Escape') { listening = null; render(); }
    else if (e.code === 'Backspace' || e.code === 'Delete') assign(null);
    else assign(e.code);
    return;
  }
  held.add(e.code);
  if (binds.some(b => b.key === e.code)) e.preventDefault();
});
addEventListener('keyup', e => held.delete(e.code));
addEventListener('blur', () => held.clear());

function activePad() {
  return [...(navigator.getGamepads?.() ?? [])].find(p => p?.connected);
}

function readKeys(pad) {
  let mask = 0;
  binds.forEach((b, i) => {
    const kb = b.key != null && held.has(b.key);
    const gp = b.pad != null && pad?.buttons[b.pad]?.pressed;
    if (kb || gp) mask |= 1 << i;
  });
  return mask;
}

// ---------- Эмулятор ----------
const canvas = document.getElementById('screen');
const ctx = canvas.getContext('2d');
const statusEl = document.getElementById('status');
const romInput = document.getElementById('rom');

const wasm = await init();
let emu = null, running = false, last = performance.now(), acc = 0;

// ---------- Звук ----------
let audioCtx = null, audioNode = null, gainNode = null;
const volumeEl = document.getElementById('volume');
let volume = 0.7;
try { volume = +(localStorage.getItem('gba-volume') ?? 0.7); } catch {}
volumeEl.value = Math.round(volume * 100);
volumeEl.oninput = () => {
  volume = volumeEl.value / 100;
  if (gainNode) gainNode.gain.value = volume;
  try { localStorage.setItem('gba-volume', String(volume)); } catch {}
};

// AudioContext можно создавать только после действия пользователя — вызываем при выборе ROM
async function initAudio() {
  try {
    if (!audioCtx) {
      audioCtx = new AudioContext({ latencyHint: 'interactive' });
      await audioCtx.audioWorklet.addModule('audio-worklet.js');
      audioNode = new AudioWorkletNode(audioCtx, 'gba-audio', { outputChannelCount: [2] });
      gainNode = audioCtx.createGain();
      gainNode.gain.value = volume;
      audioNode.connect(gainNode).connect(audioCtx.destination);
      audioNode.port.onmessage = e => { window.__audioLevel = e.data.rms; };
    }
    if (audioCtx.state === 'suspended') await audioCtx.resume();
  } catch (err) {
    console.warn('звук недоступен', err);
  }
}

// ---------- Сохранения (SRAM/Flash/EEPROM) в localStorage ----------
let saveKey = null, frameCount = 0;
const toB64 = u8 => {
  let s = '';
  for (let i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
  return btoa(s);
};
const fromB64 = b64 => Uint8Array.from(atob(b64), c => c.charCodeAt(0));

function persistSave() {
  if (!emu || !saveKey) return;
  try {
    const data = emu.save_data();
    if (data.length) localStorage.setItem(saveKey, toB64(data));
  } catch (e) { console.warn('save failed', e); }
}
function restoreSave() {
  try {
    const b64 = localStorage.getItem(saveKey);
    if (b64) emu.load_save(fromB64(b64));
  } catch (e) { console.warn('load save failed', e); }
}
addEventListener('pagehide', persistSave);
document.addEventListener('visibilitychange', () => { if (document.hidden) persistSave(); });

romInput.onchange = async () => {
  const file = romInput.files[0];
  if (!file) return;
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    persistSave();
    initAudio();
    emu?.free();
    emu = new Emulator();
    const title = emu.load_rom(bytes);
    saveKey = `gba-save:${title}:${bytes.length}`;
    restoreSave();
    statusEl.textContent = title;
    running = true;
    acc = 0;
  } catch (err) {
    running = false;
    statusEl.textContent = String(err);
  }
};

function draw() {
  // память wasm может переехать при росте — берём вид заново каждый кадр
  const frame = new Uint8ClampedArray(wasm.memory.buffer, emu.frame_ptr(), W * H * 4);
  ctx.putImageData(new ImageData(frame, W, H), 0, 0);
}

function loop(t) {
  requestAnimationFrame(loop);
  const dt = Math.min(t - last, 100);
  last = t;

  const pad = activePad();
  if (listening?.kind === 'pad' && pad) {
    const idx = pad.buttons.findIndex(b => b.pressed);
    if (idx >= 0) assign(idx);
  }
  if (!running) return;

  acc += dt;
  let ran = false;
  while (acc >= FRAME_MS) {
    emu.set_keys(readKeys(pad));
    emu.run_frame();
    acc -= FRAME_MS;
    ran = true;
  }
  if (ran) {
    draw();
    const pcm = emu.take_audio(); // всегда забираем, чтобы очередь не росла
    if (audioNode && pcm.length) audioNode.port.postMessage(pcm, [pcm.buffer]);
  }
  // раз в секунду сбрасываем изменённое сохранение в localStorage
  if (++frameCount % 60 === 0 && emu.save_dirty()) persistSave();
}

render();
requestAnimationFrame(loop);
