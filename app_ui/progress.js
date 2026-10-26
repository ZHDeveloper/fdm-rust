'use strict';
const $ = (s) => document.querySelector(s);
let taskId = null;
let snap = null;

function esc(value) {
  return String(value == null ? '' : value).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

// 从当前 URL 参数提前解析 taskId，确保在各种异步状态下都能准确定位窗口
(() => {
  try {
    const m = RegExp('[?&]taskId=([^&]*)').exec(window.location.search);
    if (m) taskId = Number(m[1]);
  } catch (e) {}
})();

// 公共关闭进度窗口函数
function closeProgressWindow() {
  const target = taskId != null ? `prog_${taskId}` : 'progress';
  if (window.api && window.api.closeSelf) {
    window.api.closeSelf(target);
  } else if (window.api && window.api.winClose) {
    window.api.winClose(target);
  } else if (window.close) {
    window.close();
  }
}

function fmtSize(b) {
  if (b == null) return '—';
  const u = ['B', 'KB', 'MB', 'GB', 'TB']; let i = 0, n = b;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return n.toFixed(i >= 3 ? 3 : (i === 0 ? 0 : 2)) + ' ' + u[i];
}
function fmtSpeed(b) { return (b > 0 ? fmtSize(b) + '/秒' : '0 B/秒'); }
function fmtEta(sec) {
  if (sec == null || !isFinite(sec) || sec <= 0) return '—';
  const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = Math.floor(sec % 60);
  const parts = [];
  if (h) parts.push(h + ' 时');
  if (h || m) parts.push(m + ' 分');
  parts.push(s + ' 秒');
  return parts.join(' ');
}
const STATUS_TEXT = {
  queued: '排队中...', connecting: '正在连接...', waiting: '等待数据源...', downloading: '正在接收数据...',
  merging: '正在合并...', paused: '已暂停', done: '下载完成', error: '错误', seeding: '做种/共享中',
};

window.api.onProgressInit((d) => {
  taskId = d.taskId;
  if (d.snap) { snap = d.snap; render(); }
});
window.api.onTaskUpdate((s) => {
  if (taskId == null || s.id !== taskId) return;
  snap = s; render();
});
window.api.onTaskRemove((p) => {
  const removedId = p && typeof p === 'object' ? p.id : p;
  if (taskId != null && removedId === taskId) {
    closeProgressWindow();
  }
});

function shortUrl(url) {
  if (!url) return '';
  const raw = String(url);
  // magnet: 的 URL.origin 在浏览器里就是字符串 "null"，不能直接拼；
  // ed2k:// 更不是标准 URL，会抛异常。这里单独给 P2P 链接做友好显示。
  if (raw.toLowerCase().startsWith('magnet:')) {
    let name = '';
    try { name = new URL(raw).searchParams.get('dn') || ''; } catch (e) {}
    const m = /urn:btih:([0-9a-z]+)/i.exec(raw);
    let s = '磁力链接 · ' + (name || (m ? m[1] : ''));
    if (!name && !m) s = raw;
    return s.length > 90 ? s.slice(0, 87) + '…' : s;
  }
  if (raw.toLowerCase().startsWith('ed2k://')) {
    const m = /\|file\|([^|]*)\|/.exec(raw);
    let s = 'ED2K · ' + (m && m[1] ? m[1] : raw);
    return s.length > 90 ? s.slice(0, 87) + '…' : s;
  }
  try {
    const u = new URL(raw);
    // strip query/signature entirely; show host + path only (like IDM图3)
    let s = u.origin === 'null' ? (u.protocol + u.pathname) : (u.origin + u.pathname);
    if (s.length > 90) s = s.slice(0, 87) + '…';
    return s;
  } catch (e) {
    return raw.split('?')[0].slice(0, 90);
  }
}

function render() {
  if (!snap) return;
  const isP2P = snap.kind === 'bt' || snap.kind === 'ed2k';
  const blockLabel = $('#blockLabel');
  const blocks = $('#blocks');
  const threads = document.querySelector('.sec-threads');
  const toggle = $('#toggleDetails');
  // 单连接（含 EdgeOne WAF 单线程）不显示分块进度条。
  const singleStream = !isP2P && (!snap.segments || snap.segments.length <= 1);
  if (blockLabel) blockLabel.style.display = (isP2P || singleStream) ? 'none' : '';
  if (blocks) blocks.style.display = (isP2P || singleStream) ? 'none' : '';
  if (threads) threads.style.display = isP2P ? 'none' : '';
  if (toggle) toggle.style.display = isP2P ? 'none' : '';
  const urlEl = $('#url');
  urlEl.textContent = shortUrl(snap.url);
  urlEl.title = snap.url || '';
  const st = $('#stStatus');
  st.textContent = snap.status === 'error' && snap.error ? ('错误：' + snap.error) : (STATUS_TEXT[snap.status] || snap.status);
  st.className = 'v ' + (snap.status === 'error' ? 'err' : 'status');

  const total = snap.total;
  const done = snap.kind === 'hls' || snap.kind === 'dash' ? (snap.bytes || 0) : (snap.downloaded || 0);
  const pct = total ? Math.min(100, (done / total) * 100) : (snap.progress || 0) * 100;

  $('#stSize').textContent = fmtSize(total);
  $('#stDone').textContent = fmtSize(done) + (total ? `（${pct.toFixed(2)} %）` : '');
  $('#stSpeed').textContent = snap.status === 'downloading'
    ? fmtSpeed(snap.speed || 0)
    : (snap.status === 'seeding' ? ('上传 ' + fmtSpeed(snap.uploadSpeed || 0)) : '');
  const eta = (snap.speed > 0 && total) ? (total - done) / snap.speed : null;
  $('#stEta').textContent = fmtEta(eta);
  $('#stResume').textContent = snap.kind === 'http'
    ? (snap.segments && snap.segments.length > 1 ? '能够' : (snap.segments && snap.segments[0] && Number.isFinite(snap.segments[0].end) ? '能够' : '未知'))
    : (snap.kind === 'bt' || snap.kind === 'ed2k' ? '引擎托管（支持续传）' : '流式（合并后完成）');

  $('#ovBar').style.width = pct + '%';
  $('#ovPct').textContent = pct.toFixed(0) + '%';

  // title: brand + domain, then progress like IDM ("TDM Fast ·(tdmfast.com) 10% filename")
  document.title = "FDM · " + pct.toFixed(0) + "% " + (snap.filename || "");

  drawBlocks();
  renderThreads();
  updateButtons();
}

// ---- Layer 2: canvas block progress ----
function drawBlocks() {
  if (snap && (snap.kind === 'bt' || snap.kind === 'ed2k')) return;
  const canvas = $('#blocks');
  const ctx = canvas.getContext('2d');
  // match canvas resolution to display size for crisp rendering
  const cssW = canvas.clientWidth || 640;
  const dpr = window.devicePixelRatio || 1;
  if (canvas.width !== Math.floor(cssW * dpr)) { canvas.width = Math.floor(cssW * dpr); }
  const W = canvas.width, H = canvas.height;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.clearRect(0, 0, W, H);

  const total = snap.total;
  const segs = snap.segments || [];
  if (!total || !segs.length) {
    // no segment info (stream): show a single overall fill
    const p = snap.progress || 0;
    ctx.fillStyle = '#eceff1'; ctx.fillRect(0, 0, W, H);
    ctx.fillStyle = '#4a90d9'; ctx.fillRect(0, 0, W * p, H);
    return;
  }

  // background
  ctx.fillStyle = '#eceff1';
  ctx.fillRect(0, 0, W, H);

  for (const s of segs) {
    const x0 = Math.floor((s.start / total) * W);
    const x1 = Math.ceil(((s.end + 1) / total) * W);
    const segW = Math.max(1, x1 - x0);
    // downloaded portion within this segment
    const doneBytes = Math.max(0, Math.min((s.cur ?? s.start) - s.start, s.end - s.start + 1));
    const doneW = Math.round((doneBytes / (s.end - s.start + 1)) * segW);

    // undownloaded part of the segment: light
    ctx.fillStyle = '#dfe4e8';
    ctx.fillRect(x0, 0, segW, H);
    // downloaded part: blue gradient
    const grad = ctx.createLinearGradient(0, 0, 0, H);
    grad.addColorStop(0, '#5db0ff'); grad.addColorStop(1, '#2f7be0');
    ctx.fillStyle = s.done ? '#2e7d32' : grad;
    ctx.fillRect(x0, 0, doneW, H);
    // thread start marker (red tick like IDM)
    ctx.fillStyle = '#e53935';
    ctx.fillRect(x0, 0, 2, H);
    // segment divider
    ctx.fillStyle = '#b7bcc0';
    ctx.fillRect(x1 - 1, 0, 1, H);
  }
  // outer border
  ctx.strokeStyle = '#b7bcc0';
  ctx.strokeRect(0.5, 0.5, W - 1, H - 1);
}

function renderThreads() {
  if (snap && (snap.kind === 'bt' || snap.kind === 'ed2k')) return;
  const tb = $('#threadBody');
  const segs = snap.segments || [];
  tb.innerHTML = '';
  const title = $('#threadTitle');
  const blockLabel = $('#blockLabel');
  const colN = $('#threadColN');
  if (snap.kind === 'bt' && Array.isArray(snap.files) && snap.files.length) {
    if (title) title.textContent = '文件明细';
    if (blockLabel) blockLabel.textContent = '整体下载进度';
    if (colN) colN.textContent = 'N.';
    snap.files.forEach((f, i) => {
      const dl = f.progress || 0;
      const tr = document.createElement('tr');
      tr.innerHTML = `<td class="col-n">${i + 1}</td><td class="col-dl">${fmtSize(dl)} / ${fmtSize(f.length)}</td><td title="${esc(f.path || '')}">${esc(f.path || '')}</td>`;
      tb.appendChild(tr);
    });
    return;
  }
  if (snap.kind === 'ed2k') {
    if (title) title.textContent = '分块明细';
    if (blockLabel) blockLabel.textContent = '各分块的下载进度';
    if (colN) colN.textContent = 'N.';
    if (segs.length) {
      segs.forEach((s, i) => {
        const dl = Math.max(0, (s.cur ?? s.start) - s.start);
        const info = s.done ? '完成' : (snap.status === 'downloading' ? '等待源/下载中…' : (snap.status === 'waiting' ? '等待数据源…' : (snap.status === 'paused' ? '已暂停' : '等待')));
        const tr = document.createElement('tr');
        tr.innerHTML = `<td class="col-n">${i + 1}</td><td class="col-dl">${fmtSize(dl)}</td><td>${info}</td>`;
        tb.appendChild(tr);
      });
      return;
    }
    const tr = document.createElement('tr');
    tr.innerHTML = `<td class="col-n">1</td><td class="col-dl">${fmtSize(snap.downloaded || 0)}</td><td>源 ${snap.sources || 0} · ${STATUS_TEXT[snap.status] || snap.status}</td>`;
    tb.appendChild(tr);
    return;
  }
  const connectionCount = snap.connections || 0;
  if (title) title.textContent = connectionCount > 0 ? `线程明细（${connectionCount} 个连接）` : '线程明细';
  if (blockLabel) blockLabel.textContent = '各连接的起始位置以及下载进度';
  if (colN) colN.textContent = 'N.';
  // 工作窃取会产生更多分片，但线程明细只按真实连接聚合，避免连接数虚高。
  const groups = new Map();
  segs.forEach((s, i) => {
    const hasWorker = Number.isInteger(s.worker);
    const key = hasWorker ? s.worker : `seg-${i}`;
    if (!groups.has(key)) {
      groups.set(key, { index: hasWorker ? s.worker : i, downloaded: 0, active: false });
    }
    const group = groups.get(key);
    group.downloaded += Math.max(0, (s.cur ?? s.start) - s.start);
    if (!s.done) group.active = true;
  });
  Array.from(groups.values())
    .sort((a, b) => a.index - b.index)
    .forEach((group) => {
      const info = group.active
        ? (snap.status === 'downloading' ? '正在接收数据...' : (snap.status === 'paused' ? '已暂停' : '等待'))
        : '完成';
      const tr = document.createElement('tr');
      tr.innerHTML = `<td class="col-n">${group.index + 1}</td><td class="col-dl">${fmtSize(group.downloaded)}</td><td>${info}</td>`;
      tb.appendChild(tr);
    });
}

function updateButtons() {
  const b = $('#btnPauseResume');
  const c = $('#btnCancel');
  if (['downloading', 'connecting', 'waiting', 'queued', 'merging', 'seeding'].includes(snap.status)) {
    b.textContent = '暂停'; b.disabled = false;
  } else if (['paused', 'error'].includes(snap.status)) {
    b.textContent = '继续'; b.disabled = false;
  } else if (snap.status === 'done') {
    b.textContent = '打开'; b.disabled = false;
    c.textContent = '关闭';
  } else {
    c.textContent = '取消';
  }
}

$('#btnPauseResume').onclick = async () => {
  if (!snap) return;
  if (['downloading', 'connecting', 'waiting', 'queued', 'merging'].includes(snap.status)) {
    window.api.pause(taskId);
  } else if (['paused', 'error'].includes(snap.status)) {
    window.api.resume(taskId);
  } else if (snap.status === 'done') {
    const p = snap.filepath || (snap.dir ? (snap.dir + '/' + snap.filename) : snap.filename);
    if (window.api && window.api.openFile && p) {
      try {
        await window.api.openFile(p);
      } catch (e) {}
    }
    closeProgressWindow();
  }
};
$('#btnCancel').onclick = () => closeProgressWindow();

$('#toggleDetails').onclick = () => {
  document.body.classList.toggle('details-hidden');
  $('#toggleDetails').textContent = document.body.classList.contains('details-hidden') ? '>> 显示细节' : '<< 隐藏细节';
  if (!document.body.classList.contains('details-hidden')) setTimeout(drawBlocks, 0);
};

window.addEventListener('resize', () => { if (snap) drawBlocks(); });
