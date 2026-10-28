/**
 * FDM 浏览器下载助手 - 后台服务 (Background Service Worker)
 * 遵循 Chromium Manifest V3 规范，兼容 Google Chrome / Microsoft Edge 等 Chromium 浏览器
 *
 * 设计原则：扩展启用后即自动接管所有 HTTP/HTTPS 下载。按 FDM 设置决定后续行为：
 * 开启「自动下载链接」时直接创建任务并打开进度窗口；关闭时打开「新建任务」窗口。
 * FDM 未启动时静默回退浏览器原生下载，绝不阻塞用户。
 */

const FDM_HOST = '127.0.0.1';
const FDM_PORT = 25432;
const PROXY_PORT = 25433;
const STATUS_ALARM = 'fdm-status-check';
const PROXY_STORAGE_KEY = 'fdmProxyState';

function originalProxyString(original) {
  const v = original && typeof original === 'object' ? original : null;
  if (!v || v.mode !== 'fixed_servers' || !v.rules) return 'DIRECT';
  const kinds = { http: 'PROXY', https: 'PROXY', socks4: 'SOCKS', socks5: 'SOCKS5' };
  const single = v.rules.singleProxy;
  if (single && single.host) {
    return `${kinds[single.scheme] || 'PROXY'} ${single.host}:${single.port || 80}`;
  }
  return 'DIRECT';
}

function proxyPac(original) {
  // shortcut: 只继承 fixed_servers 单一代理；system/pac 模式回落 DIRECT，需要时再解析系统代理。
  const fallback = originalProxyString(original);
  const httpTarget = `PROXY 127.0.0.1:${PROXY_PORT}; ${fallback}`;
  return `var FDM_HTTP='${httpTarget}';
var FDM_OTHER='${fallback}';
function FindProxyForURL(url, host) {
  if (host === '127.0.0.1' || host === 'localhost' || isPlainHostName(host)) return 'DIRECT';
  if (url.substring(0, 5) === 'http:') return FDM_HTTP;
  return FDM_OTHER;
}`;
}

function proxyGet() {
  return new Promise((resolve) => {
    chrome.proxy.settings.get({ incognito: false }, (details) => {
      void chrome.runtime.lastError;
      resolve(details && details.value ? details.value : null);
    });
  });
}

function proxySet(value) {
  return new Promise((resolve, reject) => {
    chrome.proxy.settings.set({ value, scope: 'regular' }, () => {
      const err = chrome.runtime.lastError;
      if (err) reject(new Error(err.message));
      else resolve();
    });
  });
}

function proxyClear() {
  return new Promise((resolve, reject) => {
    chrome.proxy.settings.clear({ scope: 'regular' }, () => {
      const err = chrome.runtime.lastError;
      if (err) reject(new Error(err.message));
      else resolve();
    });
  });
}

let proxyBusy = false;

// PAC 是全局浏览器设置：原始配置必须落盘，Service Worker 重启后才能恢复。
async function applyProxy(enabled, systemProxy) {
  if (!chrome.proxy || !chrome.proxy.settings || proxyBusy) return;
  proxyBusy = true;
  try {
    const stored = await chrome.storage.local.get(PROXY_STORAGE_KEY);
    const state = stored[PROXY_STORAGE_KEY] || null;
    if (enabled) {
      const current = await proxyGet();
      const pacData = current && current.pacScript && current.pacScript.data;
      if (pacData && pacData.includes(`127.0.0.1:${PROXY_PORT}`)) {
        // 自家 PAC 已在，但 storage 可能被清空：补一条可恢复记录，否则永远清不掉。
        if (!state) {
          await chrome.storage.local.set({ [PROXY_STORAGE_KEY]: { active: true, original: null } });
        }
        return;
      }
      const mode = current && current.mode;
      if (mode && mode !== 'system' && mode !== 'direct') {
        console.warn('[FDM Ext] 浏览器已配置代理，跳过接管:', mode);
        return;
      }
      if (systemProxy) {
        console.warn('[FDM Ext] 检测到系统代理，跳过 HTTP 抓包:', systemProxy);
        return;
      }
      const original = state && state.active ? state.original : current;
      await proxySet({ mode: 'pac_script', pacScript: { data: proxyPac(original) } });
      await chrome.storage.local.set({
        [PROXY_STORAGE_KEY]: { active: true, original: original || null }
      });
      return;
    }
    if (!state || !state.active) return;
    if (state.original) {
      await proxySet(state.original);
    } else {
      await proxyClear();
    }
    await chrome.storage.local.remove(PROXY_STORAGE_KEY);
  } catch (e) {
    console.warn('[FDM Ext] 代理设置失败:', e);
  } finally {
    proxyBusy = false;
  }
}

let isOnline = false;
let serverInfo = null;

// ==================== Service Worker 冷启动就绪闸门 ====================
// MV3 Service Worker 空闲约 30 秒即被 Chrome 挂起，唤醒时全局变量重置
// （isOnline = false）。所有接管判定必须先 await ensureReady()，避免冷启动误判离线。
let readyPromise = null;
let lastReadyAt = 0;
const READY_TTL_MS = 30000;

function ensureReady(force = false) {
  if (readyPromise) return readyPromise;
  const now = Date.now();
  if (!force && lastReadyAt && (now - lastReadyAt) < READY_TTL_MS) {
    return Promise.resolve();
  }
  readyPromise = (async () => {
    try {
      await checkServerStatus();
    } catch (e) {
      console.warn('[FDM Ext] 就绪探测失败:', e);
    } finally {
      lastReadyAt = Date.now();
      readyPromise = null;
    }
  })();
  return readyPromise;
}

// 记录正在向 FDM 上报处理中的 URL，防止瞬发并发重复触发
const inFlightUrls = new Set();
// 记录最近接管的 URL 与时间戳，用于极短防抖（例如 800ms 内快速双击）
const recentIntercepts = new Map(); // url -> timestamp
const DEBOUNCE_MS = 800;

// ==================== 服务端连通性与状态监控 ====================

function getServerBaseUrl() {
  return `http://${FDM_HOST}:${FDM_PORT}`;
}

async function checkServerStatus() {
  const url = `${getServerBaseUrl()}/api/browser/status`;
  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 2000);
    const resp = await fetch(url, {
      signal: controller.signal,
      headers: { 'X-FDM-Extension': '1' }
    });
    clearTimeout(timeoutId);

    if (resp.ok) {
      const data = await resp.json();
      isOnline = true;
      serverInfo = data;
      applyProxy(!!(data && data.proxy_ready), data && data.system_proxy);
      updateBadgeOnline(data);
      return { online: true, data };
    }
  } catch (e) {
    // 请求超时或服务未启动
  }

  isOnline = false;
  serverInfo = null;
  applyProxy(false, null);
  updateBadgeOffline();
  return { online: false, data: null };
}

function updateBadgeOnline(info) {
  try {
    chrome.action.setIcon({
      path: {
        16: 'icons/icon16.png',
        32: 'icons/icon32.png',
        48: 'icons/icon48.png',
        128: 'icons/icon128.png'
      }
    });

    const activeCount = info && typeof info.active_tasks === 'number' ? info.active_tasks : 0;
    if (activeCount > 0) {
      chrome.action.setBadgeText({ text: String(activeCount) });
      chrome.action.setBadgeBackgroundColor({ color: '#1657c4' });
    } else {
      chrome.action.setBadgeText({ text: '' });
    }
    chrome.action.setTitle({
      title: `FDM 在线 · 活跃任务: ${activeCount} · 总速度: ${(info && info.total_speed_str) || '0 B/s'}`
    });
  } catch (e) {
    // 忽略异常
  }
}

function updateBadgeOffline() {
  try {
    chrome.action.setIcon({
      path: {
        16: 'icons/icon16_gray.png',
        32: 'icons/icon32_gray.png',
        48: 'icons/icon48_gray.png',
        128: 'icons/icon128_gray.png'
      }
    });
    chrome.action.setBadgeText({ text: '' });
    chrome.action.setTitle({ title: 'FDM 未连接（下载将由浏览器原生处理）' });
  } catch (e) {
    // 忽略异常
  }
}

// ==================== 下载接管核心逻辑 ====================

function isInterceptableUrl(url) {
  return typeof url === 'string'
    && (url.startsWith('http://') || url.startsWith('https://'));
}

async function getCookiesForUrl(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (cookies && cookies.length > 0) {
      return cookies.map(c => `${c.name}=${c.value}`).join('; ');
    }
  } catch (e) {
    console.warn('[FDM Ext] 读取 Cookie 失败:', e);
  }
  return '';
}

async function sendTaskToFDM(payload) {
  const endpoint = `${getServerBaseUrl()}/api/browser/intercept`;
  try {
    const resp = await fetch(endpoint, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-FDM-Extension': '1' },
      body: JSON.stringify(payload)
    });
    if (resp.ok) {
      return await resp.json();
    }
  } catch (e) {
    console.error('[FDM Ext] 上报任务给 FDM 失败:', e);
  }
  return null;
}

// 后端成功创建任务，或成功拉起「新建任务」窗口，都视为接管成功。
function isHandledByFDM(result) {
  // 只要请求成功返回 ok:true，就说明 FDM 已经接管（创建、复用、跳过或拉起窗口）。
  // 不再依赖具体 action 字符串，避免新增结果类型时旧逻辑把下载放回浏览器。
  return !!result && result.ok === true;
}

chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  const url = item.finalUrl || item.url || '';

  console.debug('[FDM Ext] 下载项', {
    id: item.id,
    url: item.url,
    finalUrl: item.finalUrl,
    filename: item.filename,
    fileSize: item.fileSize,
    referrer: item.referrer,
    byExtensionId: item.byExtensionId || null
  });

  // 仅对 http/https 申请异步接管：返回 true 后 Chrome 会暂停原生下载决策，
  // 直到我们显式调用 suggest() 或取消该项，避免「边落盘边改名」的竞态。
  if (!isInterceptableUrl(url)) {
    return; // blob:, data:, file: 等本地伪协议直接放行给浏览器
  }

  const settle = async () => {
    try {
      // 冷启动就绪：先完成本地服务状态探测，再做接管判定
      await ensureReady();

      if (!isOnline || !isInterceptableUrl(url)) {
        // FDM 未启动：兜底放行，绝不影响浏览器正常下载
        doSuggest(suggest, item);
        return;
      }

      // 一次性表单 POST 不能重放：交给浏览器原生下载，FDM 不接管。
      if (isRecentFormPost(url)) {
        doSuggest(suggest, item);
        return;
      }

      const now = Date.now();
      const isRecent = recentIntercepts.has(url) && (now - recentIntercepts.get(url) < DEBOUNCE_MS);
      const isInFlight = inFlightUrls.has(url);

      if (isInFlight || isRecent) {
        // 同 URL 正在接管中或极短间隔内重复：完成原生决策后异步取消该项，
        // 避免在 onDeterminingFilename 阶段直接 cancel() 被判为 ERR_BLOCKED_BY_CLIENT。
        abortBrowserDownload(suggest, item);
        return;
      }

      inFlightUrls.add(url);
      recentIntercepts.set(url, now);

      // 定期清理过期的防抖记录，防止 Map 累积
      if (recentIntercepts.size > 200) {
        for (const [u, t] of recentIntercepts.entries()) {
          if (now - t > 10000) {
            recentIntercepts.delete(u);
          }
        }
      }

      try {
        const cookieHeader = await getCookiesForUrl(item.finalUrl || item.url);
        const headers = {
          'User-Agent': navigator.userAgent
        };
        if (item.referrer) {
          headers['Referer'] = item.referrer;
        }
        if (cookieHeader) {
          headers['Cookie'] = cookieHeader;
        }

        const payload = {
          url: item.finalUrl || item.url,
          filename: item.filename || '',
          fileSize: item.fileSize || 0,
          referrer: item.referrer || '',
          headers: headers,
          confirm: true
        };

        const result = await sendTaskToFDM(payload);
        if (isHandledByFDM(result)) {
          // 接管成功：先让 Chrome 完成下载决策，再取消并清理原生下载项。
          abortBrowserDownload(suggest, item);
        } else {
          // 未能打开新建任务窗口：解除防抖并兜底放行原生下载
          recentIntercepts.delete(url);
          doSuggest(suggest, item);
        }
      } catch (e) {
        console.error('[FDM Ext] 拦截流程异常:', e);
        recentIntercepts.delete(url);
        doSuggest(suggest, item); // 兜底放行，绝不让用户下载被卡死
      } finally {
        inFlightUrls.delete(url);
      }
    } catch (e) {
      console.error('[FDM Ext] 接管流程异常，兜底放行:', e);
      doSuggest(suggest, item);
    }
  };

  // 显式声明异步接管：Chrome 会等待 suggest() 或 cancel() 才结束下载项决策
  settle();
  return true;
});

/**
 * 安全地建议浏览器继续原生下载。
 * 我们返回 true 之后，必须调用 suggest() 或 cancel() 才能结束该下载项的决策；
 * 若下载项已被取消导致 suggest 失效，则忽略其错误。
 */
function doSuggest(suggest, item) {
  try {
    const name = item && typeof item.filename === 'string'
      ? item.filename.split(/[\\/]/).pop()
      : '';
    if (name) suggest({ filename: name, conflictAction: 'uniquify' });
    else suggest();
  } catch (e) {
    console.warn('[FDM Ext] suggest 失败:', e);
  }
}

/**
 * 结束 onDeterminingFilename 的异步决策后，取消并清理浏览器原生下载项。
 *
 * Chrome 要求返回 true 的监听器最终必须调用 suggest()。若直接调用
 * downloads.cancel() 而不 suggest()，底层请求会被判为 ERR_BLOCKED_BY_CLIENT，
 * 点击下载链接的标签页会跳到 chrome-error://chromewebdata/ 错误页。
 * 因此先 suggest() 放行决策，再在下一轮事件循环撤销下载项。
 */
function abortBrowserDownload(suggest, item) {
  doSuggest(suggest, item);
  const id = item && typeof item.id === 'number' ? item.id : null;
  if (id === null) return;
  setTimeout(() => {
    try {
      chrome.downloads.cancel(id, () => {
        void chrome.runtime.lastError;
        try {
          chrome.downloads.erase({ id }, () => { void chrome.runtime.lastError; });
        } catch (e) {}
      });
    } catch (e) {}
  }, 0);
}

// ==================== 右键上下文菜单 ====================

function setupContextMenus() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: 'fdm_download_link',
      title: '使用 FDM 下载此链接',
      contexts: ['link']
    });

    chrome.contextMenus.create({
      id: 'fdm_download_media',
      title: '使用 FDM 下载媒体文件',
      contexts: ['image', 'video', 'audio']
    });
  });
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  let targetUrl = '';

  if (info.menuItemId === 'fdm_download_link') {
    targetUrl = info.linkUrl;
  } else if (info.menuItemId === 'fdm_download_media') {
    targetUrl = info.srcUrl;
  }
  if (!targetUrl) return;

  await ensureReady();

  if (!isOnline) {
    chrome.notifications.create({
      type: 'basic',
      iconUrl: 'icons/icon128_gray.png',
      title: 'FDM 未连接',
      message: '请先启动 FDM 桌面客户端，再使用右键下载。',
      priority: 1
    });
    return;
  }

  const cookieHeader = await getCookiesForUrl(targetUrl);
  const headers = {
    'User-Agent': navigator.userAgent,
    'Referer': (tab && tab.url) || ''
  };
  if (cookieHeader) headers['Cookie'] = cookieHeader;

  const payload = {
    url: targetUrl,
    filename: '',
    fileSize: 0,
    referrer: (tab && tab.url) || '',
    headers: headers,
    confirm: true,
    forceDialog: true
  };

  const res = await sendTaskToFDM(payload);
  if (!isHandledByFDM(res)) {
    chrome.notifications.create({
      type: 'basic',
      iconUrl: 'icons/icon128_gray.png',
      title: 'FDM 处理失败',
      message: '无法创建任务或打开新建任务窗口，请确认 FDM 主程序运行正常。',
      priority: 1
    });
  }
});

// ==================== 表单 POST 下载 ====================
// HTTPS/一次性 POST 无法重放：直接交给浏览器原生下载，不再导入 FDM。

const recentPostUrls = new Map(); // url -> timestamp
const POST_TTL_MS = 10 * 60 * 1000;

function trackFormPosts() {
  const api = chrome.webRequest;
  if (!api || !api.onBeforeRequest || typeof api.onBeforeRequest.addListener !== 'function') {
    console.warn('[FDM Ext] webRequest 不可用，POST 表单将按普通下载处理');
    return;
  }
  try {
    api.onBeforeRequest.addListener(
      (details) => {
        if (details.method !== 'POST') return;
        recentPostUrls.set(details.url, Date.now());
        if (recentPostUrls.size > 300) {
          const cut = Date.now() - POST_TTL_MS;
          for (const [u, t] of recentPostUrls) {
            if (t < cut) recentPostUrls.delete(u);
          }
        }
      },
      { urls: ['<all_urls>'] },
      ['requestBody']
    );
  } catch (e) {
    console.warn('[FDM Ext] 注册 webRequest 失败:', e);
  }
}

function isRecentFormPost(url) {
  const seen = recentPostUrls.get(url);
  if (!seen) return false;
  if (Date.now() - seen > POST_TTL_MS) {
    recentPostUrls.delete(url);
    return false;
  }
  return true;
}

// ==================== 初始化 ====================

// MV3 Service Worker 空闲后会被挂起，单靠 setInterval 会停止心跳；
// 用 alarms 定期唤醒，保证 FDM 能持续识别扩展在线。
function ensureStatusAlarm() {
  chrome.alarms.get(STATUS_ALARM, (alarm) => {
    if (!alarm) {
      chrome.alarms.create(STATUS_ALARM, { periodInMinutes: 0.5 });
    }
  });
}

chrome.runtime.onInstalled.addListener(() => {
  ensureStatusAlarm();
  void ensureReady(true);
});

chrome.runtime.onStartup.addListener(() => {
  ensureStatusAlarm();
  void ensureReady(true);
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name !== STATUS_ALARM) return;
  void ensureReady(true);
});

chrome.action.onClicked.addListener(() => {
  void ensureReady(true);
});

(async () => {
  // 冷启动就绪：任何一项初始化失败都不能影响状态探测与 PAC 生命周期
  try {
    await ensureReady(true);
  } catch (e) {
    console.warn('[FDM Ext] 就绪探测失败:', e);
  }
  try { setupContextMenus(); } catch (e) { console.warn('[FDM Ext] 菜单初始化失败:', e); }
  try { trackFormPosts(); } catch (e) { console.warn('[FDM Ext] 表单监听初始化失败:', e); }
  try { ensureStatusAlarm(); } catch (e) { console.warn('[FDM Ext] 定时器初始化失败:', e); }
  // Service Worker 活跃期间保持 5 秒刷新；休眠后由 alarms 兜底。
  setInterval(checkServerStatus, 5000);
})();
