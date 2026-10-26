'use strict';
const $ = (s) => document.querySelector(s);
let _prefill = {};
let _dir = '';

function fmtSize(b) {
  const value = Number(b);
  if (b == null || b === '' || !Number.isFinite(value) || value <= 0) return '—';
  const u = ['B', 'KB', 'MB', 'GB', 'TB']; let i = 0, n = value;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return n.toFixed(i >= 3 ? 2 : (i === 0 ? 0 : 1)) + ' ' + u[i];
}
function nameFromUrl(url) {
  try { const u = new URL(url); let n = decodeURIComponent(u.pathname.split('/').pop() || ''); return n.split('?')[0] || 'download'; }
  catch (e) { return 'download'; }
}
function parseHeaders(text) {
  const h = {};
  for (const line of (text || '').split('\n')) {
    const i = line.indexOf(':');
    if (i > 0) { const k = line.slice(0, i).trim(), v = line.slice(i + 1).trim(); if (k && v) h[k] = v; }
  }
  return Object.keys(h).length ? h : null;
}
// 文件大小/元数据探测：按协议分别处理 HTTP、BT、ED2K
let _kind = null;
let _torrentPath = null;
let _probeTimer = null;
let _probeSeq = 0;

function detectKind(url) {
  const u = (url || '').trim();
  if (/^ed2k:\/\//i.test(u)) return 'ed2k';
  if (/^magnet:/i.test(u)) return /xt=urn:ed2k:/i.test(u) ? 'ed2k' : 'bt';
  if (/^https?:\/\//i.test(u) && /\.torrent(\?|#|$)/i.test(u)) return 'bt';
  return 'http';
}
function parseEd2k(url) {
  const m = /^ed2k:\/\/\|file\|([^|]*)\|(\d+)\|([0-9a-fA-F]{32})\|/i.exec((url || '').trim());
  if (!m) return null;
  let name = m[1] || '';
  try { name = decodeURIComponent(name); } catch (e) {}
  return { name, size: Number(m[2]) || 0, hash: m[3].toLowerCase() };
}
function parseMagnetName(url) {
  const m = /[?&]dn=([^&]*)/i.exec(url || '');
  if (!m) return '';
  try { return decodeURIComponent(m[1].replace(/\+/g, ' ')); } catch (e) { return m[1]; }
}
function setKindUi(kind) {
  _kind = kind;
  // BT / ED2K 任务不提供高级选项，固定隐藏
  const compact = kind === 'bt' || kind === 'ed2k';
  if (compact) document.body.classList.add('advanced-hidden');
  const toggleAdvanced = $('#toggleAdvanced');
  if (toggleAdvanced) toggleAdvanced.style.display = compact ? 'none' : '';
  document.querySelectorAll('[data-http-only]').forEach((el) => {
    el.style.display = kind === 'http' ? '' : 'none';
  });
  const saveAs = $('#saveAs');
  if (saveAs) saveAs.readOnly = kind === 'ed2k';
  const browse = $('#browse');
  if (browse) browse.style.display = kind === 'ed2k' ? 'none' : '';
  requestAnimationFrame(applyAdaptiveHeight);
}

async function probeFileSize() {
  const url = $('#urlBox').value.trim();
  const kind = _kind || detectKind(url);
  if (kind === 'http' && !/^https?:\/\//i.test(url)) return;
  const seq = ++_probeSeq;
  if (kind === 'ed2k') {
    const info = parseEd2k(url);
    if (info && seq === _probeSeq) {
      $('#fileSize').textContent = fmtSize(info.size);
      const name = cleanTitle(info.name);
      if (name) $('#saveAs').value = _dir + sep(_dir) + name;
    } else {
      const m = /[?&]xl=(\d+)/i.exec(url || '');
      const size = m ? Number(m[1]) : 0;
      const name = cleanTitle(parseMagnetName(url));
      if (size) $('#fileSize').textContent = fmtSize(size);
      if (name) $('#saveAs').value = _dir + sep(_dir) + name;
    }
    return;
  }
  if (kind === 'bt' && /^magnet:/i.test(url)) {
    const name = cleanTitle(parseMagnetName(url));
    if (name) $('#saveAs').value = _dir + sep(_dir) + name;
    // magnet 可携带 xl（精确字节数）；没有 xl 时只能等 BT 元数据交换完成。
    const m = /[?&]xl=(\d+)/i.exec(url || '');
    const size = m ? Number(m[1]) : 0;
    if (size > 0) {
      _prefill.size = size;
      $('#fileSize').textContent = fmtSize(size);
    } else if (!_prefill.size) {
      $('#fileSize').textContent = '—';
    }
    return;
  }
  let res = null;
  try {
    if (kind === 'bt') {
      const body = url.startsWith('http') ? { url } : { path: _torrentPath || url };
      res = await window.api.probeTorrent(body);
      if (seq !== _probeSeq) return;
      if (res && res.ok) {
        if (res.total) _prefill.size = res.total;
        $('#fileSize').textContent = fmtSize(res.total);
        if (res.name) $('#saveAs').value = _dir + sep(_dir) + cleanTitle(res.name);
        return;
      }
    }
    const headers = parseHeaders($('#headers').value) || _prefill.headers || null;
    res = await window.api.probeSize(url, headers);
  } catch (e) {
    return;
  }
  if (seq !== _probeSeq) return;
  if (res && res.ok) {
    if (res.size) _prefill.size = res.size;
    $('#fileSize').textContent = fmtSize(res.size);
    if (res.filename) {
      const auto = _dir + sep(_dir) + nameFromUrl(url);
      if (!$('#saveAs').value || $('#saveAs').value === auto) {
        $('#saveAs').value = _dir + sep(_dir) + res.filename;
      }
    }
  }
}
function scheduleProbe(delay) {
  clearTimeout(_probeTimer);
  _probeTimer = setTimeout(probeFileSize, delay == null ? 600 : delay);
}

// 清理浏览器传入的网页标题："(153) 标题 - YouTube" → "标题"
function cleanTitle(name) {
  if (!name) return name;
  let n = String(name).trim();
  n = n.replace(/^\(\d+\+?\)\s*/, '');
  n = n.replace(/\s*[-–|]\s*(YouTube( Music)?|Bilibili|TikTok|Vimeo|Twitch|Instagram|Facebook|Dailymotion|Reddit|微博|小红书)\s*$/i, '');
  n = n.replace(/_哔哩哔哩_bilibili\s*$/i, '').replace(/[-_]\s*bilibili\s*$/i, '');
  n = n.replace(/\s*-\s*抖音\s*$/, '');
  n = n.replace(/\s+(on|\/)\s+X(:.*)?$/i, '').replace(/\s*\/\s*Twitter$/i, '');
  n = n.replace(/[\\/:*?"<>|\r\n\t]+/g, ' ').replace(/\s{2,}/g, ' ').trim();
  return n.slice(0, 150) || String(name).trim();
}
window.cleanTitle = cleanTitle;

async function applyPrefill(p) {
  _prefill = p || {};
  const url = _prefill.url || '';
  _torrentPath = _prefill.torrentPath || null;
  $('#urlBox').value = url || (_torrentPath || '');
  if (_prefill.filename) _prefill.filename = cleanTitle(_prefill.filename);
  let kind = _prefill.kind || detectKind(url || _torrentPath || '');
  if (_torrentPath) kind = 'bt';
  setKindUi(kind);
  const s = await getSettings();
  const defaultDir = s.defaultDir || (await window.api.defaultDir());
  // ED2K 由 aMule 统一使用默认下载目录；BT 也默认使用该目录，但允许本次任务另存为。
  _dir = kind === 'ed2k' ? defaultDir : (_prefill.dir || defaultDir);
  let name = _prefill.filename || '';
  if (!name && url) name = nameFromUrl(url);
  if (!name && _torrentPath) name = _torrentPath.split(/[\\/]/).pop().replace(/\.torrent$/i, '');
  $('#saveAs').value = name ? _dir + sep(_dir) + name : _dir + sep(_dir);
  $('#fileSize').textContent = fmtSize(_prefill.size);
  if (_torrentPath) scheduleProbe(100);
  else if (url && (!_prefill.size || Number(_prefill.size) <= 0)) scheduleProbe(250);
  if (_prefill.headers) {
    $('#headers').value = Object.entries(_prefill.headers).map(([k, v]) => `${k}: ${v}`).join('\n');
  }
  if (_prefill.autoStart) {
    setTimeout(() => {
      const btn = $('#start');
      if (btn && !btn.disabled) btn.click();
    }, 250);
  }
}
function sep(d) { return (d.endsWith('\\') || d.endsWith('/')) ? '' : '\\'; }

// When the user types/pastes a URL, update the suggested filename.
function onUrlChanged() {
  const url = $('#urlBox').value.trim();
  _prefill.url = url;
  _torrentPath = null;
  const kind = detectKind(url);
  setKindUi(kind);
  if (!url) return;
  let name = '';
  if (kind === 'ed2k') {
    const info = parseEd2k(url);
    name = info ? info.name : '';
  } else if (kind === 'bt' && /^magnet:/i.test(url)) {
    name = parseMagnetName(url);
  } else {
    name = nameFromUrl(url);
  }
  if (name) $('#saveAs').value = _dir + sep(_dir) + cleanTitle(name);
  scheduleProbe(600);
}

window.api.onPrefill(async (p) => {
  await applyPrefill(p);
  requestAnimationFrame(applyAdaptiveHeight);
});
$('#urlBox').addEventListener('input', onUrlChanged);
$('#urlBox').addEventListener('paste', () => setTimeout(onUrlChanged, 0));


$('#browse').onclick = async () => {
  const cur = $('#saveAs').value.split(/[\\/]/).pop();
  const picked = await window.api.chooseSaveAs(cur);
  if (picked) { $('#saveAs').value = picked; _dir = picked.replace(/[\\/][^\\/]*$/, ''); }
};

function collectOpts() {
  const full = $('#saveAs').value;
  const dir = full.replace(/[\\/][^\\/]*$/, '');
  const filename = full.split(/[\\/]/).pop();
  const url = $('#urlBox').value.trim();
  const kind = _kind || detectKind(url);
  const opts = {
    dir, filename, kind,
    maxRate: (parseInt($('#rate').value) || 0) * 1024,
    openProgress: true,
  };
  if (kind === 'bt') {
    if (_torrentPath) opts.torrentPath = _torrentPath;
    else opts.url = url;
  } else if (kind === 'ed2k') {
    opts.url = url;
    opts.ed2kLink = url;
  } else {
    opts.url = url;
    opts.headers = parseHeaders($('#headers').value) || _prefill.headers || null;
    opts.connections = parseInt($('#conn').value) || 8;
  }
  return opts;
}

async function getSettings() {
  try { return (await window.api.getSettings()) || {}; } catch (e) { return {}; }
}
$('#start').onclick = async () => {
  const url = $('#urlBox').value.trim();
  if (!url) { $('#urlBox').focus(); return; }
  const opts = collectOpts();
  const id = await window.api.add(opts);
  if (id === 'delegated' || id === 'existing_done') { closeWindow(); return; }
  if (id == null) return;
  window.api.openProgress(id);
  closeWindow();
};
$('#later').onclick = async () => {
  const url = $('#urlBox').value.trim();
  if (!url) { $('#urlBox').focus(); return; }
  const opts = collectOpts();
  opts.startPaused = true;
  if (opts.kind === 'bt' || opts.kind === 'ed2k') {
    const id = await window.api.add(opts);
    if (id === 'delegated' || id === 'existing_done') { closeWindow(); return; }
    if (id == null) return;
    closeWindow();
    return;
  }
  const id = await window.api.add(opts);
  if (id === 'delegated' || id === 'existing_done') { closeWindow(); return; }
  if (id == null) return;
  closeWindow();
};
function closeWindow() {
  const btn = $('#cancel');
  if (btn) btn.disabled = true;
  if (window.api && window.api.closeSelf) {
    window.api.closeSelf('newdownload');
  } else if (window.close) {
    window.close();
  }
}
$('#cancel').onclick = () => closeWindow();
// 新建下载窗口当前逻辑高度（与 Tauri 创建窗口时的初始高度保持一致）
let currentWinHeight = window.innerHeight || 257;

function measureContentHeight() {
  const scroll = document.querySelector('.scroll');
  const foot = document.querySelector('.foot');
  if (!scroll || !foot) return window.innerHeight || 257;
  const viewportHeight = document.documentElement.clientHeight || 0;
  // 逻辑窗口高度 - 页面可视高度 = 原生标题栏等非页面区域占用的高度
  const frameHeight = Math.max(0, currentWinHeight - viewportHeight);
  // .scroll 是 flex:1，直接读 scrollHeight 会被当前可视高度撑满，
  // 收起内容时无法得到真实内容高度。这里临时改为按内容撑开再测量。
  const prevFlex = scroll.style.flex;
  const prevHeight = scroll.style.height;
  const prevOverflow = scroll.style.overflowY;
  scroll.style.flex = '0 0 auto';
  scroll.style.height = 'auto';
  scroll.style.overflowY = 'visible';
  const contentHeight = scroll.getBoundingClientRect().height;
  scroll.style.flex = prevFlex;
  scroll.style.height = prevHeight;
  scroll.style.overflowY = prevOverflow;
  return Math.ceil(contentHeight + foot.offsetHeight + frameHeight);
}

function applyAdaptiveHeight() {
  if (!(window.api && window.api.resizeWindow)) return;
  const targetHeight = Math.max(180, Math.min(850, measureContentHeight()));
  currentWinHeight = targetHeight;
  window.api.resizeWindow('newdownload', 520, targetHeight).then((res) => {
    if (res && res.height) currentWinHeight = res.height;
  }).catch(() => {});
  // 窗口尺寸变化后滚动条/换行可能微调，再校正一次
  setTimeout(() => {
    if (!(window.api && window.api.resizeWindow)) return;
    const refined = Math.max(180, Math.min(850, measureContentHeight()));
    if (Math.abs(refined - currentWinHeight) > 2) {
      currentWinHeight = refined;
      window.api.resizeWindow('newdownload', 520, refined);
    }
  }, 180);
}

$('#toggleAdvanced').onclick = () => {
  const hidden = document.body.classList.toggle('advanced-hidden');
  $('#toggleAdvanced').textContent = hidden ? '>> 显示高级选项' : '<< 隐藏高级选项';
  // 弹窗高度随高级选项的显示/隐藏自动适配
  requestAnimationFrame(applyAdaptiveHeight);
};
// 首次打开（含无 prefill 场景）按实际内容校正窗口高度
requestAnimationFrame(applyAdaptiveHeight);

document.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && e.target.id !== 'urlBox') $('#start').click();
  if (e.key === 'Escape') closeWindow();
});
