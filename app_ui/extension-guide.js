'use strict';

const $ = (selector) => document.querySelector(selector);
const PAGES = {
  chrome: { page: 'chrome://extensions', label: '打开 Chrome 扩展页' },
  edge: { page: 'edge://extensions', label: '打开 Edge 扩展页' },
  chromium: {
    page: '浏览器扩展管理页',
    label: '查看扩展页地址',
    manual: '请在浏览器地址栏输入扩展管理页：Brave 为 brave://extensions，Vivaldi 为 vivaldi://extensions，Opera 为 opera://extensions，多数 Chromium 浏览器为 chrome://extensions。',
  },
};

let browser = 'chrome';
let extensionDir = '';

function setBrowser(name) {
  browser = PAGES[name] ? name : 'chrome';
  if ($('#browserSelect')) $('#browserSelect').value = browser;
  const spec = PAGES[browser];
  if ($('#extPageText')) $('#extPageText').textContent = spec.page;
  if ($('#openExtensionsPage')) $('#openExtensionsPage').textContent = spec.label;
  if ($('#pageHint')) {
    $('#pageHint').textContent = '';
    $('#pageHint').style.color = '#8a6d3b';
  }
}

function setStatus(connected) {
  const dot = $('#statusDot');
  const text = $('#statusText');
  if (!dot || !text) return;
  if (connected) {
    dot.className = 'dot ok';
    text.textContent = '已检测到扩展连接';
    text.style.color = '#107c10';
  } else {
    dot.className = 'dot warn';
    text.textContent = '尚未检测到扩展连接';
    text.style.color = '#8a6d3b';
  }
}

async function refreshInfo() {
  try {
    const info = await window.api.extensionInfo();
    if (info && info.dir) {
      extensionDir = info.dir;
      if ($('#extensionPath')) $('#extensionPath').value = info.dir;
    }
    setStatus(!!(info && info.connected));
  } catch (e) {
    setStatus(false);
  }
}

async function copyExtensionPath() {
  if (!extensionDir) return;
  let copied = false;
  try {
    await navigator.clipboard.writeText(extensionDir);
    copied = true;
  } catch (e) {
    try {
      const input = $('#extensionPath');
      input.focus();
      input.select();
      copied = document.execCommand('copy');
    } catch (e2) {}
  }
  const btn = $('#copyPath');
  if (!btn) return;
  const old = btn.textContent;
  btn.textContent = copied ? '已复制' : '复制失败';
  setTimeout(() => { btn.textContent = old; }, 1200);
}

if ($('#browserSelect')) {
  $('#browserSelect').onchange = () => setBrowser($('#browserSelect').value);
}

if ($('#openExtensionsPage')) {
  $('#openExtensionsPage').onclick = async () => {
    const hint = $('#pageHint');
    if (!hint) return;
    const spec = PAGES[browser];
    if (spec.manual) {
      hint.textContent = spec.manual;
      hint.style.color = '#8a6d3b';
      return;
    }
    const ok = await window.api.openBrowserExtensions(browser);
    if (ok) {
      hint.textContent = '';
    } else {
      hint.textContent = '未能自动启动浏览器，请手动在地址栏输入 ' + spec.page;
      hint.style.color = '#b42318';
    }
  };
}

if ($('#copyPath')) $('#copyPath').onclick = copyExtensionPath;

if ($('#openFolder')) {
  $('#openFolder').onclick = () => {
    if (extensionDir) window.api.openFolder(extensionDir);
  };
}

if ($('#closeBtn')) $('#closeBtn').onclick = () => window.api.closeSelf('extension_guide');

document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') window.api.closeSelf('extension_guide');
});

function fitWindow() {
  if (window.fitPopupToContent) requestAnimationFrame(() => window.fitPopupToContent('extension_guide', 560));
}

// 首次打开时先用固定内容高度，避免窗口先按 400px 显示再收缩。
if (window.fitPopupToContent) window.fitPopupToContent('extension_guide', 560, { initialHeight: 400 });
// 状态检测完成后内容高度可能变化，再校正一次。
setTimeout(fitWindow, 220);

if ($('#browserSelect')) $('#browserSelect').addEventListener('change', fitWindow);
refreshInfo().then(fitWindow);
setInterval(refreshInfo, 2000);
fitWindow();
