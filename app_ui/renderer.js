'use strict';
const tasks = new Map();
let selectedId = null;
let currentCat = 'all';

const $ = (s) => document.querySelector(s);
const taskBody = $('#taskBody');

function esc(value) {
  return String(value == null ? '' : value).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

// ---- column resize (drag the name column header edge) ----
function initColResize() {
  const KEY = 'fdm_col_widths_v3';
  const FIXED_WIDTHS = {
    size: 76,
    status: 88,
    progress: 150,
    speed: 82,
    eta: 82,
  };
  const MIN_NAME_WIDTH = 100;
  const table = document.querySelector('table.tasks');
  if (!table) return;
  const contentEl = document.querySelector('.content');

  let saved = {};
  try { saved = JSON.parse(localStorage.getItem(KEY) || '{}'); } catch (e) {}
  let nameWidth = Number(saved.name);
  if (!Number.isFinite(nameWidth) || nameWidth <= 0) nameWidth = 0;

  const ths = [...table.querySelectorAll('thead th')];
  const nameTh = ths.find((th) => th.dataset.col === 'name');
  const fixedTotal = () => Object.values(FIXED_WIDTHS).reduce((a, b) => a + b, 0);
  const minNameWidth = () => {
    const avail = contentEl ? contentEl.clientWidth : 0;
    return avail > 0 ? Math.max(MIN_NAME_WIDTH, avail - fixedTotal() - 1) : MIN_NAME_WIDTH;
  };

  // 其他列固定，名称列至少填满内容区的剩余宽度；拖大后表格改为横向滚动。
  function syncTableWidth() {
    const shownName = Math.max(minNameWidth(), nameWidth);
    for (const th of ths) {
      const key = th.dataset.col;
      if (!key) continue;
      const w = key === 'name' ? shownName : FIXED_WIDTHS[key];
      if (Number.isFinite(w)) th.style.width = w + 'px';
    }
    table.style.width = (fixedTotal() + shownName) + 'px';
  }

  window.addEventListener('resize', syncTableWidth);
  if (window.ResizeObserver && contentEl) {
    let raf = 0;
    new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(syncTableWidth);
    }).observe(contentEl);
  }

  if (nameTh) {
    const handle = document.createElement('span');
    handle.className = 'col-resizer';
    nameTh.appendChild(handle);
    let startX = 0, startW = 0, dragging = false;
    handle.addEventListener('mousedown', (e) => {
      e.preventDefault(); e.stopPropagation();
      startX = e.clientX;
      startW = nameTh.getBoundingClientRect().width;
      dragging = true;
      document.body.style.cursor = 'col-resize';
      document.body.style.userSelect = 'none';
      document.body.style.webkitUserSelect = 'none';
      const onMove = (ev) => {
        if (!dragging) return;
        nameWidth = Math.round(Math.max(minNameWidth(), startW + (ev.clientX - startX)));
        syncTableWidth();
      };
      const onUp = () => {
        document.removeEventListener('mousemove', onMove);
        document.removeEventListener('mouseup', onUp);
        if (!dragging) return;
        dragging = false;
        document.body.style.cursor = '';
        document.body.style.userSelect = '';
        document.body.style.webkitUserSelect = '';
        try {
          const cur = JSON.parse(localStorage.getItem(KEY) || '{}');
          cur.name = nameWidth;
          localStorage.setItem(KEY, JSON.stringify(cur));
        } catch (e2) {}
        syncTableWidth();
      };
      document.addEventListener('mousemove', onMove);
      document.addEventListener('mouseup', onUp);
    });
  }
  syncTableWidth();
}
initColResize();

// ---- right-click context menu on task rows ----
const ctxMenu = () => $('#ctxMenu');
function hideCtx() { const m = ctxMenu(); if (m) m.style.display = 'none'; }

// 右键菜单只作用于当前右键点击的任务。
function ctxTargets(id) {
  return [id];
}

function buildCtxItems(ids) {
  const ts = ids.map((i) => tasks.get(i)).filter(Boolean);
  if (!ts.length) return [];
  const multi = ts.length > 1;
  const canResume = ts.some((t) => ['paused', 'error', 'queued'].includes(t.status));
  const canPause = ts.some((t) => ['downloading', 'connecting', 'waiting', 'queued', 'merging'].includes(t.status));
  const allDone = ts.every((t) => t.status === 'done');
  const one = ts.length === 1 ? ts[0] : null;
  const n = multi ? `(${ts.length} 项)` : '';

  const items = [];
  items.push({
    label: `开始/继续下载 ${n}`.trim(), disabled: !canResume,
    act: () => ids.forEach((i) => window.api.resume(i)),
  });
  items.push({
    label: `暂停 ${n}`.trim(), disabled: !canPause,
    act: () => ids.forEach((i) => window.api.pause(i)),
  });
  items.push({ sep: true });
  if (one) {
    items.push({
      label: '打开文件', disabled: one.status !== 'done',
      act: () => window.api.openFile(one.dir + '/' + one.filename),
    });
    items.push({ label: '打开所在文件夹', act: () => window.api.openFolder(one.dir) });
    items.push({
      label: '查看进度窗口', disabled: one.status === 'done',
      act: () => window.api.openProgress(one.id),
    });
    items.push({ sep: true });
    items.push({ label: '复制下载地址', act: () => navigator.clipboard.writeText(one.url || '') });
    items.push({ sep: true });
  }
  items.push({
    label: multi ? `删除这 ${ts.length} 个任务…` : '删除任务…',
    act: () => deleteTasks(ids),
  });
  items.push({ sep: true });
  items.push({
    label: '删除全部已完成任务…', disabled: ![...tasks.values()].some((t) => t.status === 'done'),
    act: () => clearCompletedTasks(),
  });
  return items;
}

function showCtx(x, y, ids) {
  const m = ctxMenu();
  const items = buildCtxItems(ids);
  if (!items.length) return;
  m.innerHTML = '';
  for (const it of items) {
    if (it.sep) { const d = document.createElement('div'); d.className = 'sep'; m.appendChild(d); continue; }
    const d = document.createElement('div');
    d.className = 'mi' + (it.disabled ? ' disabled' : '') + (it.danger ? ' danger' : '');
    d.textContent = it.label;
    if (!it.disabled) d.onclick = () => { hideCtx(); it.act(); };
    m.appendChild(d);
  }
  // Measure offscreen, then clamp inside the viewport. Coordinates are
  // sanitized: a NaN/undefined x or y would otherwise leave the menu parked
  // at -9999px (invisible) instead of next to the cursor.
  m.style.display = 'block';
  m.style.visibility = 'hidden';
  m.style.left = '-9999px';
  m.style.top = '0px';
  const r = m.getBoundingClientRect();
  const px = Number.isFinite(x) ? x : 0;
  const py = Number.isFinite(y) ? y : 0;
  const left = Math.max(0, Math.min(px, window.innerWidth - r.width - 4));
  const top = Math.max(0, Math.min(py, window.innerHeight - r.height - 4));
  m.style.left = left + 'px';
  m.style.top = top + 'px';
  m.style.visibility = 'visible';
}

// Only a left-click outside the menu closes it. Listening to every 'click'
// caught the same right-click that opened the menu and hid it immediately.
document.addEventListener('mousedown', (e) => {
  if (e.button === 2) return;              // right-click is handled by oncontextmenu
  if (e.target.closest('#ctxMenu')) return; // let menu item clicks through
  hideCtx();
});
document.addEventListener('contextmenu', (e) => {
  if (!e.target.closest('#taskBody tr')) hideCtx();
});
window.addEventListener('blur', hideCtx);
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') hideCtx(); });

async function shouldDeleteFiles() {
  try {
    const s = await window.api.getSettings();
    return !s || s.deleteTaskFiles !== false;
  } catch (_) {
    return true;
  }
}

async function deleteTasks(ids) {
  const targets = [...new Set(ids)].filter((id) => tasks.has(id));
  if (!targets.length) return;

  const del = await shouldDeleteFiles();
  const ok = await window.api.confirm(
    del
      ? `确定删除选中的 ${targets.length} 个任务？\n\n将同时删除已下载的文件，此操作不可恢复。`
      : `确定删除选中的 ${targets.length} 个任务？\n\n仅从任务列表移除，不会删除已下载的文件。`,
    'FDM',
    'question'
  );
  if (!ok) return;

  // rows are dropped by the 'task:remove' event, not locally (avoids the race)
  for (const id of targets) await window.api.remove(id, del);
  updateToolbar();
}

async function clearCompletedTasks() {
  const done = [...tasks.values()].filter((t) => t.status === 'done');
  if (!done.length) { await window.api.message('没有已完成的任务', 'FDM', 'info'); return; }
  const del = await shouldDeleteFiles();
  const ok = await window.api.confirm(
    del
      ? `确定删除已完成的 ${done.length} 个任务？\n\n将同时删除已下载的文件，此操作不可恢复。`
      : `确定从列表移除已完成的 ${done.length} 个任务？\n\n不会删除已下载的文件。`,
    'FDM',
    'question'
  );
  if (!ok) return;
  for (const t of done) await window.api.remove(t.id, del);
  render();
  updateToolbar();
}

function fmtSize(b) {
  if (b == null) return '—';
  const u = ['B', 'KB', 'MB', 'GB', 'TB']; let i = 0; let n = b;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return n.toFixed(n >= 100 || i === 0 ? 0 : 1) + ' ' + u[i];
}
function fmtSpeed(b) { return b > 0 ? fmtSize(b) + '/s' : ''; }
function fmtStreamSize(t) {
  // For HLS/DASH, total is segment count; show downloaded bytes if known
  if (t.bytes) return fmtSize(t.bytes);
  if (t.total) return t.downloaded + '/' + t.total + ' 段';
  return '—';
}
function fmtEta(t) {
  if (t == null || t <= 0 || !isFinite(t)) return '';
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = Math.floor(t % 60);
  return (h ? h + ':' : '') + String(m).padStart(2, '0') + ':' + String(s).padStart(2, '0');
}
const STATUS_TEXT = { queued: '排队中', connecting: '连接中', waiting: '等待数据源', downloading: '接收数据', paused: '已暂停', done: '完成', error: '错误', merging: '合并中', seeding: '做种中' };

function categoryOf(name) {
  const ext = (name.split('.').pop() || '').toLowerCase();
  if (['mp4', 'mkv', 'avi', 'mov', 'flv', 'webm', 'ts', 'm4v'].includes(ext)) return 'video';
  if (['mp3', 'flac', 'wav', 'aac', 'm4a', 'ogg'].includes(ext)) return 'music';
  if (['exe', 'msi', 'dmg', 'apk', 'deb', 'rpm', 'appimage'].includes(ext)) return 'program';
  if (['zip', 'rar', '7z', 'tar', 'gz', 'xz', 'bz2'].includes(ext)) return 'archive';
  if (['pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'txt', 'epub'].includes(ext)) return 'document';
  return 'other';
}
function matchCat(t) {
  if (currentCat === 'all') return true;
  if (currentCat === 'downloading') return ['downloading', 'connecting', 'waiting', 'queued', 'paused'].includes(t.status);
  if (currentCat === 'done') return t.status === 'done';
  // BT / ED2K 任务统一归入「其他」，不再单独分类
  if (t.kind === 'bt' || t.kind === 'ed2k') return currentCat === 'other';
  return categoryOf(t.filename || '') === currentCat;
}

function render() {
  taskBody.innerHTML = '';
  let shown = 0, totalSpeed = 0;
  // 新添加的任务 ID 更大，放在列表顶部（IDM 风格：最新任务在最上面）
  const ordered = Array.from(tasks.values()).sort((a, b) => b.id - a.id);
  for (const t of ordered) {
    if (t.status === 'downloading') totalSpeed += t.speed || 0;
    if (!matchCat(t)) continue;
    shown++;
    // 已完成任务一律显示满格：历史任务的 progress 可能因分块元数据不完整而停在 98% 左右。
    const pct = t.status === 'done'
      ? 100
      : Math.round(Math.max(0, Math.min(1, t.progress || 0)) * 100);
    const eta = t.status === 'downloading' && t.speed > 0 && t.total
      ? (t.total - t.downloaded) / t.speed : null;
    const tr = document.createElement('tr');
    if (t.id === selectedId) tr.className = 'selected';
    tr.dataset.id = t.id;
    const stCls = 'st-' + t.status;
    const kindBadge = t.kind === 'hls' ? '<span class="type-badge type-hls">HLS</span>'
      : t.kind === 'dash' ? '<span class="type-badge type-dash">DASH</span>'
      : t.kind === 'bt' ? '<span class="type-badge type-bt">BT</span>'
      : t.kind === 'ed2k' ? '<span class="type-badge type-ed2k">ED2K</span>' : '';
    const peerInfo = t.kind === 'bt'
      ? ((t.peers || 0) + ' 节点')
      : (t.kind === 'ed2k' ? ((t.sources || 0) + ' 源') : '');
    tr.innerHTML = `
      <td title="${esc(t.filename || '')}">${esc(t.filename || '(获取中...)')}${kindBadge}${peerInfo ? `<span class="peer-info">${esc(peerInfo)}</span>` : ''}</td>
      <td>${t.kind === 'hls' || t.kind === 'dash' ? fmtStreamSize(t) : fmtSize(t.total)}</td>
      <td class="${esc(stCls)}" title="${esc(t.status === 'error' && t.error ? t.error : '')}">${esc(STATUS_TEXT[t.status] || t.status)}</td>
      <td><div class="pbar"><i style="width:${pct}%"></i><b>${t.status === 'done' ? '100%' : pct + '%'}</b></div></td>
      <td>${(t.status === 'downloading' || t.status === 'seeding') ? fmtSpeed(t.speed || 0) : ''}</td>
      <td>${fmtEta(eta)}</td>`;
    tr.onclick = () => { selectedId = t.id; render(); updateToolbar(); };
    tr.ondblclick = () => {
      if (t.status === 'done') window.api.openFile(t.dir + '/' + t.filename);
      else window.api.openProgress(t.id);
    };
    tr.oncontextmenu = (e) => {
      e.preventDefault();
      // Stop here: the document-level contextmenu listener would otherwise
      // treat this as a click-outside and hide the menu we're about to show.
      e.stopPropagation();
      const ids = ctxTargets(t.id);
      // Highlight the row without a full render() — rebuilding rows here would
      // destroy this very element mid-event.
      selectedId = t.id;
      [...taskBody.children].forEach((row) => row.classList.remove('selected'));
      tr.classList.add('selected');
      updateToolbar();
      showCtx(e.clientX, e.clientY, ids);
    };
    taskBody.appendChild(tr);
  }
  $('#emptyHint').style.display = shown ? 'none' : 'block';
  $('#statTotal').textContent = tasks.size + ' 个任务';
  $('#statSpeed').textContent = '总速度: ' + (fmtSpeed(totalSpeed) || '0 B/s');
}

function updateToolbar() {
  const t = tasks.get(selectedId);
  const has = !!t;
  $('#btnResume').disabled = !(t && (t.status === 'paused' || t.status === 'error'));
  $('#btnPause').disabled = !(t && ['downloading', 'connecting', 'waiting', 'queued', 'seeding'].includes(t.status));
  $('#btnDelete').disabled = !has;
  $('#btnDeleteAll').disabled = tasks.size === 0;
  $('#btnFolder').disabled = !has;
}

// 从剪贴板添加：剪贴板里是链接就直接带入「下载文件信息」
async function addFromClipboard() {
  let text = '';
  try { text = ((await window.api.clipboardRead()) || '').trim(); } catch (e) {}
  if (/^(https?:\/\/|magnet:|ed2k:\/\/)/i.test(text)) window.api.newDownload({ url: text });
  else window.api.newDownload({});
}
// 快捷键（与 Electron 版菜单一致）：Ctrl+N 新建、Ctrl+V 从剪贴板添加、Ctrl+, 设置、F5 刷新
document.addEventListener('keydown', (e) => {
  const inInput = /^(INPUT|TEXTAREA|SELECT)$/.test((e.target && e.target.tagName) || '');
  if (e.ctrlKey && !e.shiftKey && !e.altKey) {
    const k = e.key.toLowerCase();
    if (k === 'n') { e.preventDefault(); window.api.newDownload({}); }
    else if (k === 'v' && !inInput) { e.preventDefault(); addFromClipboard(); }
    else if (k === ',') { e.preventDefault(); window.api.openSettings(); }
  } else if (e.key === 'F5') { e.preventDefault(); location.reload(); }
});

function resumeAll() { for (const t of tasks.values()) if (['paused','error','queued'].includes(t.status)) window.api.resume(t.id); }
function pauseAll() { for (const t of tasks.values()) if (['downloading','connecting','waiting','queued','seeding'].includes(t.status)) window.api.pause(t.id); }

// Escape hatch for tasks that refuse to disappear (e.g. an entry corrupted by
// an older build). Clears everything from the list unconditionally.
async function clearAllTasks() {
  if (!tasks.size) { await window.api.message('列表已经是空的', 'FDM', 'info'); return; }
  const del = await shouldDeleteFiles();
  const ok = await window.api.confirm(
    del
      ? `确定删除全部 ${tasks.size} 个任务？\n\n将同时删除已下载的文件，此操作不可恢复。`
      : `确定清空全部 ${tasks.size} 个任务？\n\n仅从任务列表移除，不会删除已下载的文件。`,
    'FDM',
    'question'
  );
  if (!ok) return;
  await window.api.removeAll(del);
  tasks.clear(); selectedId = null;
  render(); updateToolbar();
}

// native menu events (from application menu)
window.api.onMenu((m) => {
  if (m === 'resumeAll') resumeAll();
  else if (m === 'pauseAll') pauseAll();
  else if (m === 'resumeSelected') selectedId && window.api.resume(selectedId);
  else if (m === 'pauseSelected') selectedId && window.api.pause(selectedId);
  else if (m === 'deleteSelected') $('#btnDelete').click();
  else if (m === 'clearAll') clearAllTasks();
  else if (m === 'refresh') location.reload();
});
$('#btnResume').onclick = () => selectedId && window.api.resume(selectedId);
$('#btnPause').onclick = () => selectedId && window.api.pause(selectedId);
$('#btnPauseAll').onclick = () => { for (const t of tasks.values()) if (['downloading', 'connecting', 'waiting', 'queued', 'seeding'].includes(t.status)) window.api.pause(t.id); };
$('#btnDelete').onclick = () => selectedId && deleteTasks([selectedId]);
$('#btnDeleteAll').onclick = () => clearAllTasks();
$('#btnFolder').onclick = () => { const t = tasks.get(selectedId); if (t) window.api.openFolder(t.dir); };

// categories
document.querySelectorAll('.cat').forEach((el) => {
  el.onclick = () => {
    document.querySelectorAll('.cat').forEach((c) => c.classList.remove('active'));
    el.classList.add('active'); currentCat = el.dataset.cat; render();
  };
});

// ---- add modal ----
const modal = $('#addModal');
let _pendingKind = null;
let _pendingHeaders = null;
async function openAdd(prefill = {}) {
  $('#inUrl').value = prefill.url || '';
  $('#inName').value = prefill.filename || '';
  $('#inConn').value = 8; $('#inRate').value = 0;
  $('#inDir').value = prefill.dir || (await window.api.defaultDir());
  $('#inHeaders').value = prefill.headers ? Object.entries(prefill.headers).map(([k, v]) => `${k}: ${v}`).join('\n') : '';
  _pendingKind = prefill.kind || null;
  _pendingHeaders = prefill.headers || null;
  const kindLabel = { hls: 'HLS 视频流', dash: 'DASH 视频流', bt: 'BT 任务', ed2k: 'ED2K 任务' }[_pendingKind];
  $('#kindBadge').textContent = kindLabel || '';
  $('#kindBadge').style.display = kindLabel ? 'inline-block' : 'none';
  modal.classList.remove('hidden');
  $('#inUrl').focus();
}
// 暴露给 Tauri 垫片：api.newDownload 直接复用内联对话框
window.openAdd = openAdd;

function parseHeaders(text) {
  const h = {};
  for (const line of (text || '').split('\n')) {
    const idx = line.indexOf(':');
    if (idx > 0) {
      const k = line.slice(0, idx).trim();
      const v = line.slice(idx + 1).trim();
      if (k && v) h[k] = v;
    }
  }
  return Object.keys(h).length ? h : null;
}
$('#btnAdd').onclick = () => window.api.newDownload({});

$('#btnCancel').onclick = () => modal.classList.add('hidden');
$('#btnBrowse').onclick = async () => { const d = await window.api.chooseDir(); if (d) $('#inDir').value = d; };
$('#btnStart').onclick = async () => {
  const url = $('#inUrl').value.trim();
  if (!url) { await window.api.message('请输入 URL', 'FDM', 'info'); return; }
  const customHeaders = parseHeaders($('#inHeaders').value);
  const opts = {
    url, dir: $('#inDir').value, filename: $('#inName').value.trim() || null,
    kind: _pendingKind, headers: customHeaders || _pendingHeaders,
    connections: parseInt($('#inConn').value) || 8,
    maxRate: (parseInt($('#inRate').value) || 0) * 1024,
  };
  const id = await window.api.add(opts);
  if (id === 'delegated' || id === 'existing_done') { modal.classList.add('hidden'); return; }
  if (id == null) return;
  modal.classList.add('hidden');
  window.api.openProgress(id);   // IDM 式弹出进度窗口
};

// ---- IPC events ----
window.api.onTaskLog((p) => { console.log(p.line); });
window.api.onTaskRemove((p) => {
  tasks.delete(p.id);
  if (selectedId === p.id) selectedId = null;
  render(); updateToolbar();
});
window.api.onTaskUpdate((s) => {
  tasks.set(s.id, s);
  if (s.status === 'error' && s.error) console.error(`[任务 ${s.id}] 错误: ${s.error}`);
  render(); updateToolbar();
});
window.api.onTaskInfo((p) => {
  const info = p.info || {};
  if (info.chosen) {
    console.log(`[任务 ${p.id}] 选中清晰度: ${info.chosen.resolution || info.chosen.bandwidth} (共 ${info.variants.length} 个可选)`);
  }
  if (info.warning) {
    console.warn(`[任务 ${p.id}] ${info.warning}`);
  }
});
// initial load
(async () => {
  for (const s of await window.api.list()) tasks.set(s.id, s); render(); updateToolbar();
})();

// 自定义标题栏窗口控制
(function () {
  const min = document.getElementById('winMin');
  const max = document.getElementById('winMax');
  const close = document.getElementById('winClose');
  if (min) min.addEventListener('click', () => window.api.winMinimize && window.api.winMinimize());
  if (max) max.addEventListener('click', () => window.api.winMaximize && window.api.winMaximize());
  if (close) close.addEventListener('click', () => window.api.winClose && window.api.winClose());
})();

// 顶部"选项"按钮：打开设置窗口
(function () {
  const btnOpt = document.getElementById('btnOptions');
  if (!btnOpt) return;
  btnOpt.addEventListener('click', () => {
    if (window.api && window.api.openSettings) window.api.openSettings();
  });
})();
