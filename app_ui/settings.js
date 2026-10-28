'use strict';
const $ = (s) => document.querySelector(s);
let current = {};

// 设置分类标签切换
function selectTab(name) {
  const tabs = document.querySelectorAll('.tab');
  const panes = document.querySelectorAll('.pane');
  let matched = false;
  tabs.forEach((tab) => {
    const active = tab.dataset.tab === name;
    tab.classList.toggle('active', active);
    tab.setAttribute('aria-selected', active ? 'true' : 'false');
    tab.tabIndex = active ? 0 : -1;
    if (active) matched = true;
  });
  if (!matched) return false;
  panes.forEach((pane) => {
    const active = pane.dataset.pane === name;
    pane.classList.toggle('active', active);
    pane.hidden = !active;
  });
  const activeTab = document.querySelector('.tab.active');
  if (activeTab && typeof activeTab.scrollIntoView === 'function') {
    activeTab.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  }
  if (name === 'bt' || name === 'ed2k') refreshEngineStatus();
  if (name === 'extension') refreshExtensionInfo();
  if (name === 'about') refreshAbout();
  return true;
}

let updateInfo = null;

function formatBytes(value) {
  const n = Number(value) || 0;
  if (n >= 1024 * 1024) return (n / (1024 * 1024)).toFixed(1) + ' MB';
  if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
  return n + ' B';
}

function renderUpdateStatus(status) {
  const st = status || {};
  const currentVersion = $('#currentVersion');
  if (currentVersion && st.currentVersion) currentVersion.textContent = st.currentVersion;
  const statusEl = $('#updateStatus');
  const barWrap = $('#updateBarWrap');
  const bar = $('#updateBar');
  const notesEl = $('#updateNotes');
  const installBtn = $('#installUpdate');
  if (!statusEl || !installBtn) return;

  const phase = st.phase || 'idle';
  let text = '尚未检查更新';
  let percent = 0;
  let showBar = false;
  if (phase === 'checking') text = '正在检查更新…';
  else if (phase === 'up_to_date') text = '当前已是最新版本';
  else if (phase === 'available') text = '发现新版本 v' + (st.version || '—');
  else if (phase === 'downloading') {
    showBar = true;
    percent = st.total ? Math.min(100, Math.round((Number(st.downloaded || 0) / Number(st.total)) * 100)) : 0;
    text = st.total
      ? '正在下载 v' + (st.version || '') + '  ' + percent + '%（' + formatBytes(st.downloaded) + ' / ' + formatBytes(st.total) + '）'
      : '正在下载 v' + (st.version || '') + '  ' + formatBytes(st.downloaded);
  } else if (phase === 'installing') text = '正在安装更新，应用即将重启…';
  else if (phase === 'error') text = '更新失败：' + (st.error || '未知错误');
  statusEl.textContent = text;
  statusEl.style.color = phase === 'error' ? '#b42318' : '#555';

  if (barWrap && bar) {
    barWrap.style.display = showBar ? 'block' : 'none';
    bar.style.width = percent + '%';
  }

  if (notesEl) {
    const notes = st.notes || '';
    notesEl.textContent = notes;
    notesEl.style.display = notes ? 'block' : 'none';
  }

  if (phase === 'available') {
    updateInfo = { version: st.version, currentVersion: st.currentVersion, notes: st.notes };
  } else if (phase === 'idle' || phase === 'up_to_date' || phase === 'error') {
    updateInfo = null;
  }
  installBtn.disabled = !(phase === 'available' && st.version);
}

async function refreshAbout() {
  const version = await window.api.getVersion();
  if ($('#currentVersion')) $('#currentVersion').textContent = version || '—';
  try {
    const status = await window.api.getUpdateStatus();
    renderUpdateStatus(status);
  } catch (_) {}
}

async function checkForUpdates() {
  const btn = $('#checkUpdate');
  const installBtn = $('#installUpdate');
  if (btn) btn.disabled = true;
  if (installBtn) installBtn.disabled = true;
  renderUpdateStatus({ phase: 'checking', currentVersion: $('#currentVersion').textContent });
  try {
    const result = await window.api.checkUpdate();
    if (result && result.available && result.update) {
      updateInfo = result.update;
      renderUpdateStatus({
        phase: 'available',
        currentVersion: result.update.currentVersion,
        version: result.update.version,
        notes: result.update.notes,
      });
    } else {
      renderUpdateStatus({ phase: 'up_to_date', currentVersion: $('#currentVersion').textContent });
    }
  } catch (err) {
    renderUpdateStatus({
      phase: 'error',
      currentVersion: $('#currentVersion').textContent,
      error: err && err.message ? err.message : String(err),
    });
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function installUpdateNow() {
  const version = updateInfo && updateInfo.version;
  if (!version) return;
  const installBtn = $('#installUpdate');
  if (installBtn) installBtn.disabled = true;
  try {
    await window.api.installUpdate(version);
    renderUpdateStatus({
      phase: 'downloading',
      currentVersion: $('#currentVersion').textContent,
      version,
      notes: updateInfo.notes,
    });
  } catch (err) {
    renderUpdateStatus({
      phase: 'error',
      currentVersion: $('#currentVersion').textContent,
      error: err && err.message ? err.message : String(err),
    });
  }
}
async function refreshEngineStatus() {
  let st = {};
  try { st = await window.api.engineStatus(); } catch (e) { st = {}; }
  const bt = st.bt || {};
  const ed = st.ed2k || {};
  const btEl = $('#btStatus');
  if (btEl) {
    btEl.textContent = bt.ready
      ? `运行中 · 端口 ${bt.listenPort || '—'} · DHT ${bt.dhtNodes == null ? '—' : bt.dhtNodes}`
      : '未启用';
  }
  const edEl = $('#ed2kStatus');
  if (edEl) {
    edEl.textContent = ed.ready
      ? `${ed.serverConnected ? '服务器已连接' : '服务器未连接'} · Kad ${ed.kadConnected ? '已连接' : '未连接'}${ed.lowId ? ' · Low ID' : ''}`
      : '未启用';
  }
}

function initTabs() {
  const tabs = Array.from(document.querySelectorAll('.tab'));
  tabs.forEach((tab, i) => {
    tab.onclick = () => selectTab(tab.dataset.tab);
    tab.onkeydown = (e) => {
      let next = null;
      if (e.key === 'ArrowRight') next = tabs[(i + 1) % tabs.length];
      else if (e.key === 'ArrowLeft') next = tabs[(i - 1 + tabs.length) % tabs.length];
      else if (e.key === 'Home') next = tabs[0];
      else if (e.key === 'End') next = tabs[tabs.length - 1];
      if (next) {
        e.preventDefault();
        if (selectTab(next.dataset.tab)) next.focus();
      }
    };
  });
  const initial = document.querySelector('.tab.active') || tabs[0];
  if (initial) selectTab(initial.dataset.tab);
}

const DEFAULTS = {
  defaultConnections: 8, maxConcurrent: 5, autoDownload: true, monitorClipboard: true, deleteTaskFiles: true,
  doneOpenFolder: true, doneOpenFile: false, doneCloseProgress: true, autoLaunch: true,
  fileAssociations: true,
  githubProxyEnabled: true, githubProxyUrl: '', githubProxyFallbackDirect: true,
  duplicateAction: 3, rememberDuplicateAction: false,
  fileConflictAction: 0, rememberFileConflictAction: false,
  bt: { enabled: true, listenPort: 25100, dhtEnabled: true, trackerEnabled: true, maxPeers: 200, seedAfterComplete: false },
  ed2k: { enabled: true, tcpPort: 4662, udpPort: 4672, ecPort: 14712, apiPort: 14713, serverMetUrl: 'https://upd.emule-security.org/server.met', nodesDatUrl: 'https://upd.emule-security.org/nodes.dat', shareCompleted: false, downloadLimitKB: 0 },
};

// 自定义 GitHub 代理地址只在“自定义…”时显示。
function syncGithubProxyCustom() {
  const choice = $('#githubProxyChoice');
  const row = $('#githubProxyCustomRow');
  if (!choice || !row) return;
  row.style.display = choice.value === '__custom__' ? 'flex' : 'none';
}

// 同步滑杆蓝色进度比例（兼容 WebKit / Firefox 渐变方案）
function updateRangeProgress(el) {
  if (!el) return;
  const min = Number(el.min) || 0;
  const max = Number(el.max) || 100;
  const value = Number(el.value);
  const p = max > min ? ((value - min) / (max - min)) * 100 : 0;
  el.style.setProperty('--p', p + '%');
}

function fill(s) {
  current = { ...s };
  $('#connRange').value = s.defaultConnections || 8;
  $('#connVal').textContent = s.defaultConnections || 8;
  updateRangeProgress($('#connRange'));
  $('#maxConcurrent').value = s.maxConcurrent || 5;
  $('#defaultDir').value = s.defaultDir || '';
  if ($('#autoDownload')) $('#autoDownload').checked = s.autoDownload !== false;
  if ($('#doneOpenFolder')) $('#doneOpenFolder').checked = !!s.doneOpenFolder;
  if ($('#doneOpenFile')) $('#doneOpenFile').checked = !!s.doneOpenFile;
  if ($('#doneCloseProgress')) $('#doneCloseProgress').checked = !!s.doneCloseProgress;
  if ($('#autoLaunch')) $('#autoLaunch').checked = s.autoLaunch !== false;
  if ($('#monitorClipboard')) $('#monitorClipboard').checked = s.monitorClipboard !== false;
  if ($('#deleteTaskFiles')) $('#deleteTaskFiles').checked = s.deleteTaskFiles !== false;
  if ($('#fileAssociations')) $('#fileAssociations').checked = s.fileAssociations !== false;
  if ($('#duplicateAction')) $('#duplicateAction').value = String(s.duplicateAction == null ? 3 : s.duplicateAction);
  if ($('#rememberDuplicateAction')) $('#rememberDuplicateAction').checked = !!s.rememberDuplicateAction;
  if ($('#fileConflictAction')) $('#fileConflictAction').value = String(s.fileConflictAction == null ? 0 : s.fileConflictAction);
  if ($('#rememberFileConflictAction')) $('#rememberFileConflictAction').checked = !!s.rememberFileConflictAction;
  syncRememberState();
  // 代理设置
  if ($('#proxyEnabled')) $('#proxyEnabled').checked = !!s.proxyEnabled;
  if ($('#proxyType')) $('#proxyType').value = s.proxyType || 'http';
  if ($('#proxyHost')) $('#proxyHost').value = s.proxyHost || '';
  if ($('#proxyPort')) $('#proxyPort').value = s.proxyPort || '';
  if ($('#proxyUser')) $('#proxyUser').value = s.proxyUser || '';
  if ($('#proxyPass')) $('#proxyPass').value = s.proxyPass || '';

  // GitHub 加速
  if ($('#githubProxyEnabled')) $('#githubProxyEnabled').checked = !!s.githubProxyEnabled;
  if ($('#githubProxyFallbackDirect')) {
    $('#githubProxyFallbackDirect').checked = s.githubProxyFallbackDirect === true;
  }
  const githubUrl = s.githubProxyUrl || '';
  const githubPresets = Array.isArray(s.githubProxyPresets) ? s.githubProxyPresets : [];
  const githubChoice = $('#githubProxyChoice');
  if (githubChoice) {
    githubChoice.querySelectorAll('option[data-preset]').forEach((node) => node.remove());
    const customOption = githubChoice.querySelector('option[value="__custom__"]');
    githubPresets.forEach((preset) => {
      const option = document.createElement('option');
      option.value = preset;
      option.dataset.preset = '1';
      option.textContent = preset.replace(/^https?:\/\//, '').replace(/\/$/, '');
      githubChoice.insertBefore(option, customOption);
    });
  }
  if ($('#githubProxyChoice')) {
    if (!githubUrl) {
      $('#githubProxyChoice').value = '';
    } else if (githubPresets.includes(githubUrl)) {
      $('#githubProxyChoice').value = githubUrl;
    } else {
      $('#githubProxyChoice').value = '__custom__';
    }
  }
  if ($('#githubProxyUrl')) {
    $('#githubProxyUrl').value = githubPresets.includes(githubUrl) ? '' : githubUrl;
  }
  syncGithubProxyCustom();

  const bt = s.bt || {};
  if ($('#btEnabled')) $('#btEnabled').checked = bt.enabled !== false;
  if ($('#btPort')) $('#btPort').value = bt.listenPort || 25100;
  if ($('#btMaxPeers')) $('#btMaxPeers').value = bt.maxPeers || 200;
  if ($('#btDht')) $('#btDht').checked = bt.dhtEnabled !== false;
  if ($('#btTracker')) $('#btTracker').checked = bt.trackerEnabled !== false;
  if ($('#btSeed')) $('#btSeed').checked = !!bt.seedAfterComplete;

  const ed = s.ed2k || {};
  if ($('#ed2kEnabled')) $('#ed2kEnabled').checked = ed.enabled !== false;
  if ($('#ed2kTcp')) $('#ed2kTcp').value = ed.tcpPort || 4662;
  if ($('#ed2kUdp')) $('#ed2kUdp').value = ed.udpPort || 4672;
  if ($('#ed2kEc')) $('#ed2kEc').value = ed.ecPort || 14712;
  if ($('#ed2kApi')) $('#ed2kApi').value = ed.apiPort || 14713;
  if ($('#ed2kServerMet')) $('#ed2kServerMet').value = ed.serverMetUrl || '';
  if ($('#ed2kNodesDat')) $('#ed2kNodesDat').value = ed.nodesDatUrl || '';
  if ($('#ed2kShare')) $('#ed2kShare').checked = !!ed.shareCompleted;
  if ($('#ed2kLimit')) $('#ed2kLimit').value = ed.downloadLimitKB || 0;
  refreshEngineStatus();
}

initTabs();
window.api.onSettingsLoad((s) => fill(s));
(async () => {
  const s = await window.api.getSettings();
  fill(s);
})();

// 浏览器扩展状态与安装引导入口
async function refreshExtensionInfo() {
  if (!document.getElementById('extensionStatus')) return;
  const info = await window.api.extensionInfo();
  const dirEl = document.getElementById('extensionDir');
  const statusEl = document.getElementById('extensionStatus');
  if (dirEl) dirEl.value = (info && info.dir) || '';
  if (statusEl) {
    const connected = !!(info && info.connected);
    statusEl.textContent = connected ? '已检测到扩展连接' : '尚未检测到扩展连接';
    statusEl.style.color = connected ? '#107c10' : '#8a6d3b';
  }
}

(function initExtensionSection() {
  if (!document.getElementById('extensionStatus')) return;
  refreshExtensionInfo();
  setInterval(() => {
    if (document.querySelector('.tab.active[data-tab="extension"]')) refreshExtensionInfo();
  }, 3000);
  const openDirBtn = document.getElementById('openExtensionDir');
  if (openDirBtn) {
    openDirBtn.onclick = () => {
      const dir = document.getElementById('extensionDir').value;
      if (dir) window.api.openFolder(dir);
    };
  }
  const guideBtn = document.getElementById('btnOpenExtensionGuide');
  if (guideBtn) guideBtn.onclick = () => window.api.openExtensionGuide();
})();

// “显示对话框并询问”时没有可记住的策略，禁用记住复选框。
function syncRememberState() {
  const sync = (select, remember) => {
    const sel = document.querySelector(select);
    const rem = document.querySelector(remember);
    if (!sel || !rem) return;
    const asking = Number(sel.value) === 0;
    rem.disabled = asking;
    if (asking) rem.checked = false;
  };
  sync('#duplicateAction', '#rememberDuplicateAction');
  sync('#fileConflictAction', '#rememberFileConflictAction');
}

if ($('#duplicateAction')) $('#duplicateAction').onchange = syncRememberState;
if ($('#fileConflictAction')) $('#fileConflictAction').onchange = syncRememberState;
if ($('#githubProxyChoice')) $('#githubProxyChoice').onchange = syncGithubProxyCustom;

$('#connRange').oninput = () => {
  $('#connVal').textContent = $('#connRange').value;
  updateRangeProgress($('#connRange'));
};

if ($('#checkUpdate')) $('#checkUpdate').onclick = checkForUpdates;
if ($('#installUpdate')) $('#installUpdate').onclick = installUpdateNow;
window.api.onUpdateStatus((status) => renderUpdateStatus(status));
window.api.onUpdateAvailable((info) => {
  if (!info) return;
  updateInfo = info;
  renderUpdateStatus({
    phase: 'available',
    currentVersion: info.currentVersion,
    version: info.version,
    notes: info.notes,
  });
});

$('#browseDir').onclick = async () => {
  const d = await window.api.settingsChooseDir();
  if (d) $('#defaultDir').value = d;
};
if ($('#ed2kReconnect')) $('#ed2kReconnect').onclick = async () => {
  const btn = $('#ed2kReconnect');
  const el = $('#ed2kStatus');
  if (btn) btn.disabled = true;
  if (el) el.textContent = '正在连接…';
  let lastError = '';
  try {
    const res = await window.api.ed2kReconnect();
    if (res && res.ok === false && res.error) lastError = res.error;
  } catch (err) {
    lastError = err && err.message ? err.message : String(err);
  }
  // 重建引擎通常需要几秒，轮询到就绪或超时，避免过早误显示“未启用”。
  const deadline = Date.now() + 30000;
  do {
    await refreshEngineStatus();
    if (el && el.textContent && el.textContent !== '未启用') break;
    await new Promise((resolve) => setTimeout(resolve, 1000));
  } while (Date.now() < deadline);
  if (el && el.textContent === '未启用' && lastError) {
    el.textContent = '未启用 · ' + lastError;
  }
  if (btn) btn.disabled = false;
};

function collect() {
  return {
    defaultConnections: Math.max(1, Math.min(parseInt($('#connRange').value) || 8, 32)),
    maxConcurrent: Math.max(1, Math.min(parseInt($('#maxConcurrent').value) || 5, 20)),
    defaultDir: $('#defaultDir').value,
    autoDownload: $('#autoDownload') ? $('#autoDownload').checked : true,
    doneOpenFolder: $('#doneOpenFolder') ? $('#doneOpenFolder').checked : false,
    doneOpenFile: $('#doneOpenFile') ? $('#doneOpenFile').checked : false,
    doneCloseProgress: $('#doneCloseProgress') ? $('#doneCloseProgress').checked : false,
    autoLaunch: $('#autoLaunch') ? $('#autoLaunch').checked : true,
    monitorClipboard: $('#monitorClipboard') ? $('#monitorClipboard').checked : true,
    deleteTaskFiles: $('#deleteTaskFiles') ? $('#deleteTaskFiles').checked : true,
    fileAssociations: $('#fileAssociations') ? $('#fileAssociations').checked : true,
    duplicateAction: $('#duplicateAction') ? parseInt($('#duplicateAction').value, 10) || 0 : 3,
    rememberDuplicateAction: $('#rememberDuplicateAction') ? $('#rememberDuplicateAction').checked : false,
    fileConflictAction: $('#fileConflictAction') ? parseInt($('#fileConflictAction').value, 10) || 0 : 0,
    rememberFileConflictAction: $('#rememberFileConflictAction') ? $('#rememberFileConflictAction').checked : false,
    proxyEnabled: $('#proxyEnabled') ? $('#proxyEnabled').checked : false,
    proxyType: $('#proxyType') ? $('#proxyType').value : 'http',
    proxyHost: $('#proxyHost') ? $('#proxyHost').value.trim() : '',
    proxyPort: $('#proxyPort') ? ($('#proxyPort').value.trim()) : '',
    proxyUser: $('#proxyUser') ? $('#proxyUser').value.trim() : '',
    proxyPass: $('#proxyPass') ? $('#proxyPass').value : '',
    githubProxyEnabled: $('#githubProxyEnabled') ? $('#githubProxyEnabled').checked : false,
    githubProxyUrl: (() => {
      const choice = $('#githubProxyChoice') ? $('#githubProxyChoice').value : '';
      if (choice === '__custom__') {
        return $('#githubProxyUrl') ? $('#githubProxyUrl').value.trim() : '';
      }
      return choice;
    })(),
    githubProxyFallbackDirect: $('#githubProxyFallbackDirect')
      ? $('#githubProxyFallbackDirect').checked
      : true,
    bt: {
      enabled: $('#btEnabled') ? $('#btEnabled').checked : true,
      listenPort: Math.max(1, Math.min(parseInt($('#btPort') ? $('#btPort').value : 25100) || 25100, 65535)),
      dhtEnabled: $('#btDht') ? $('#btDht').checked : true,
      trackerEnabled: $('#btTracker') ? $('#btTracker').checked : true,
      maxPeers: Math.max(1, Math.min(parseInt($('#btMaxPeers') ? $('#btMaxPeers').value : 200) || 200, 2000)),
      seedAfterComplete: $('#btSeed') ? $('#btSeed').checked : false,
    },
    ed2k: {
      enabled: $('#ed2kEnabled') ? $('#ed2kEnabled').checked : true,
      tcpPort: Math.max(1, Math.min(parseInt($('#ed2kTcp') ? $('#ed2kTcp').value : 4662) || 4662, 65535)),
      udpPort: Math.max(1, Math.min(parseInt($('#ed2kUdp') ? $('#ed2kUdp').value : 4672) || 4672, 65535)),
      ecPort: Math.max(1, Math.min(parseInt($('#ed2kEc') ? $('#ed2kEc').value : 14712) || 14712, 65535)),
      apiPort: Math.max(1, Math.min(parseInt($('#ed2kApi') ? $('#ed2kApi').value : 14713) || 14713, 65535)),
      serverMetUrl: $('#ed2kServerMet') ? $('#ed2kServerMet').value.trim() : '',
      nodesDatUrl: $('#ed2kNodesDat') ? $('#ed2kNodesDat').value.trim() : '',
      shareCompleted: $('#ed2kShare') ? $('#ed2kShare').checked : false,
      downloadLimitKB: Math.max(0, parseInt($('#ed2kLimit') ? $('#ed2kLimit').value : 0) || 0),
    },
  };
}

$('#save').onclick = async () => {
  const newSettings = collect();
  await window.api.saveSettings(newSettings);
  window.api.settingsClose();
};
$('#cancel').onclick = () => window.api.settingsClose();
$('#reset').onclick = () => fill({ ...current, ...DEFAULTS });

document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') window.api.settingsClose();
  if (e.key === 'Enter' && e.ctrlKey) $('#save').click();
});
