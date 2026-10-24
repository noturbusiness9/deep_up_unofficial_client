const tauri = window.__TAURI__;
const invoke = tauri ? tauri.core.invoke : async () => { throw 'Нет Tauri (открыто в браузере)'; };
const $ = (id) => document.getElementById(id);

const MIN_S = 3600, MAX_S = 86400, SNAP_S = 65;
const STATE_RU = { active: 'Играет', paused: 'Пауза', awaiting: 'Ожидание', unknown: '?' };

const store = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch {} },
};

const ui = {
  program: store.get('program', 'b1'),
  power: store.get('power', 'medium'),
  learned: store.get('learned', []), // [{ id, name }] — режимы, выученные из официального приложения
  wake: store.get('wake', '07:30'),
  connected: false,
  state: null,
};

function allPrograms() {
  const basic = BASIC_PROGRAMS.map((p) => ({ ...p, learned: false }));
  const learned = ui.learned
    .filter((l) => !BASIC_PROGRAMS.some((b) => b.id === l.id))
    .map((l) => ({ key: `u${l.id}`, id: l.id, name: l.name, icon: 'custom', learned: true }));
  return [...basic, ...learned];
}
function programByKey(k) { return allPrograms().find((p) => p.key === k); }
function programById(id) { return allPrograms().find((p) => p.id === id); }

// Как в официальном приложении: пользователь задаёт время подъёма (остановки),
// длительность = stop − now; в пределах 65 с от границ 1 ч / 24 ч — прижать к границе.
function wakeToSeconds(hhmm, now = new Date()) {
  const [h, m] = hhmm.split(':').map(Number);
  const stop = new Date(now);
  stop.setHours(h, m, 0, 0);
  if (stop <= now) stop.setDate(stop.getDate() + 1);
  let s = Math.floor((stop - now) / 1000);
  if (Math.abs(s - MIN_S) < SNAP_S) s = MIN_S;
  if (Math.abs(s - MAX_S) < SNAP_S) s = MAX_S;
  return { seconds: s, stop };
}

function fmtDur(s) {
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
  return h ? `${h} ч ${String(m).padStart(2, '0')} мин` : `${m} мин`;
}
const fmtTime = (d) => d.toLocaleTimeString('ru-RU', { hour: '2-digit', minute: '2-digit' });

function renderGrid() {
  $('grid').innerHTML = allPrograms().map((p) => `
    <button class="prog" data-k="${p.key}">
      ${p.learned ? `<i class="forget" data-forget="${p.id}">×</i>` : ''}
      <img src="icons/${p.icon}.svg" alt="" /><span>${escapeHtml(p.name)}</span>
    </button>`).join('');
  $('grid').onclick = (e) => {
    const f = e.target.closest('[data-forget]');
    if (f) { forget(Number(f.dataset.forget)); return; }
    const b = e.target.closest('.prog');
    if (!b) return;
    ui.program = b.dataset.k; store.set('program', ui.program); renderSelection();
  };
}

function escapeHtml(t) {
  return String(t).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

function forget(id) {
  ui.learned = ui.learned.filter((l) => l.id !== id);
  store.set('learned', ui.learned);
  if (!programByKey(ui.program)) ui.program = 'b1';
  renderGrid(); renderSelection(); renderLearn();
}

// Режим обучения: на кубике играет режим, которого нет в списке (запущен из
// официального приложения) — предлагаем сохранить его ID под своим названием.
function renderLearn() {
  const s = ui.state;
  const unknown = s && (s.state === 'active' || s.state === 'paused') && s.program_id > 0
    && s.program_id <= 99 && !programById(s.program_id);
  $('learnBox').hidden = !unknown;
  if (unknown) $('learnId').textContent = s.program_id;
}

function saveLearned() {
  const id = ui.state && ui.state.program_id;
  const name = $('learnName').value.trim() || `Режим ${id}`;
  if (!id) return;
  ui.learned = [...ui.learned.filter((l) => l.id !== id), { id, name }];
  store.set('learned', ui.learned);
  $('learnName').value = '';
  renderGrid(); renderState(ui.state);
}

function renderSelection() {
  const playingId = ui.state && ui.state.state !== 'awaiting' ? ui.state.program_id : null;
  document.querySelectorAll('.prog').forEach((b) => {
    b.classList.toggle('sel', b.dataset.k === ui.program);
    b.classList.toggle('playing', programByKey(b.dataset.k).id === playingId);
  });
  document.querySelectorAll('#power button').forEach((b) => b.classList.toggle('on', b.dataset.v === ui.power));
  renderDuration();
}

function renderDuration() {
  const { seconds } = wakeToSeconds(ui.wake);
  const el = $('duration');
  const ok = seconds >= MIN_S && seconds <= MAX_S;
  el.classList.toggle('bad', !ok);
  el.textContent = ok ? fmtDur(seconds) : `${fmtDur(seconds)} — меньше часа, кубик не примет`;
  $('startBtn').disabled = !ok || !ui.connected;
}

function setConn(kind, text) {
  $('connDot').className = 'dot ' + (kind || '');
  $('connText').textContent = text;
  $('connBtn').textContent = ui.phase === 'paused' ? 'Подключить' : ui.phase === 'waiting' ? 'Повторить' : 'Отдать телефону';
  $('connBtn').disabled = kind === 'busy';
}

function showError(msg) {
  $('error').hidden = !msg;
  $('error').textContent = msg || '';
}

function renderBattery(b) {
  if (!b || b.level == null) return;
  $('battery').hidden = false;
  $('battText').textContent = `${b.level}%`;
  $('battFill').style.width = `${b.level}%`;
  $('battFill').style.background = b.level <= 15 ? 'var(--bad)' : b.level <= 35 ? 'var(--warn)' : 'var(--ok)';
  $('battCharging').hidden = !b.charging;
}

function renderState(s) {
  ui.state = s;
  $('raw').textContent = s ? s.raw : '';
  const active = s && (s.state === 'active' || s.state === 'paused');
  $('nowBox').hidden = !active;
  $('pauseBtn').hidden = !active;
  $('stopBtn').hidden = !active;
  if (active) {
    const p = programById(s.program_id);
    $('nowIcon').src = `icons/${p ? p.icon : 'custom'}.svg`;
    $('nowName').textContent = p ? p.name : `ID ${s.program_id}`;
    $('nowState').textContent = STATE_RU[s.state];
    // проверено на кубике: байты 4..7 — total, 8..11 — elapsed (секунды)
    const total = s.a, elapsed = s.b;
    const left = Math.max(0, total - elapsed);
    $('nowBar').style.width = `${s.percent}%`;
    $('nowSub').textContent = total
      ? `${s.percent}% · осталось ${fmtDur(left)} · до ${fmtTime(new Date(Date.now() + left * 1000))}`
      : `${s.percent}%`;
    $('pauseBtn').textContent = s.state === 'paused' ? 'Продолжить' : 'Пауза';
  }
  $('startBtn').textContent = active ? 'Перезапустить' : 'Запустить';
  renderLearn();
  renderSelection();
}

// Соединением управляет фоновый менеджер в Rust; окно только рисует его статус.
function renderStatus(st) {
  ui.phase = st.phase;
  ui.connected = st.phase === 'connected';
  const info = st.info || {};
  if (info.name) $('devName').textContent = info.name;
  if (st.phase === 'connected') {
    setConn('on', [info.model, info.firmware && `fw ${info.firmware}`].filter(Boolean).join(' · ') || 'Подключено');
  } else if (st.phase === 'searching') {
    setConn('busy', 'Ищу кубик…');
  } else if (st.phase === 'waiting') {
    setConn('', st.retry_in ? `Не в сети · повтор через ${st.retry_in} с` : 'Не в сети');
  } else {
    setConn('', 'Отключён — кубик свободен для телефона');
  }
  renderBattery(ui.connected ? st.battery : null);
  if (!ui.connected) $('battery').hidden = true;
  renderState(ui.connected ? st.state : null);
}

async function act(fn) {
  showError('');
  try { await fn(); } catch (e) { showError(String(e)); }
}

function start() {
  const p = programByKey(ui.program);
  const { seconds } = wakeToSeconds(ui.wake); // пересчитываем в момент нажатия, как приложение
  act(() => invoke('play', { programId: p.id, power: POWER[ui.power], side: 1, seconds }));
}

function bindSeg(id, key) {
  $(id).onclick = (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    ui[key] = b.dataset.v; store.set(key, ui[key]); renderSelection();
  };
}

renderGrid();
bindSeg('power', 'power');
$('learnSave').onclick = saveLearned;
$('learnName').onkeydown = (e) => { if (e.key === 'Enter') saveLearned(); };
$('wake').value = ui.wake;
$('wake').oninput = (e) => { ui.wake = e.target.value || ui.wake; store.set('wake', ui.wake); renderDuration(); };
$('connBtn').onclick = () => act(() => invoke('set_auto', { on: ui.phase === 'paused' || ui.phase === 'waiting' }));
$('startBtn').onclick = start;
$('stopBtn').onclick = () => act(() => invoke('stop'));
$('pauseBtn').onclick = () => {
  if (ui.state && ui.state.state === 'paused') start(); else act(() => invoke('pause'));
};
renderSelection();
setInterval(renderDuration, 30_000);

if (tauri) {
  const sync = () => invoke('status').then(renderStatus).catch(() => {});
  tauri.event.listen('cube-status', (e) => renderStatus(e.payload));
  window.addEventListener('focus', sync);
  window.addEventListener('keydown', (e) => { if (e.key === 'Escape') tauri.window.getCurrentWindow().hide(); });
  sync();
}
