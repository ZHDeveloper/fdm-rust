(() => {
  'use strict';

  const CHANNEL = 'fdm-blob-v1';
  const INSTALL_FLAG = '__fdmBlobBridgeInstalled';
  // 写入版本号：后台据此判断页面上的桥接是否属于当前扩展版本。
  if (window[INSTALL_FLAG]) return;
  window[INSTALL_FLAG] = { version: chrome.runtime.getManifest().version };

  const activeOffers = new Set();
  let enabled = false;

  // 重新加载扩展后，页面上失效的旧桥接不会被移除，仍会强制回退原生下载。
  // 每次注入都递增 epoch，页面据此只接受最新桥接的消息。
  const epoch = (window.__fdmBlobEpoch = (window.__fdmBlobEpoch || 0) + 1);

  function post(type, extra = {}) {
    window.postMessage({ channel: CHANNEL, type, epoch, ...extra }, '*');
  }

  function applyState(nextEnabled) {
    enabled = nextEnabled === true;
    post('fdm-blob-state', { enabled });
  }

  async function refreshState() {
    try {
      const state = await chrome.runtime.sendMessage({ type: 'fdm:blobState' });
      applyState(!!(state && state.enabled));
    } catch (e) {
      applyState(false);
      // 扩展重载后本桥接已失效，此时点击会静默回退原生下载，必须自愈。
      selfHealIfStale(e);
    }
  }

  // 扩展重载/升级后本页面残留失效的旧脚本，会拦截下载并强制回退浏览器。
  // 检测到上下文失效就刷新一次，让新脚本重新注入，避免用户手动刷新。
  function selfHealIfStale(error) {
    const message = error && error.message ? String(error.message) : '';
    if (!/context invalidated|Extension context/i.test(message)) return;
    if (window.__fdmBlobReloading) return;
    window.__fdmBlobReloading = true;
    setTimeout(() => {
      try {
        window.location.reload();
      } catch (_) {}
    }, 0);
  }

  async function handleOffer(data) {
    const id = data.id;
    if (typeof id !== 'string' || activeOffers.has(id)) return;
    const blobUrl = data.blobUrl;
    if (typeof blobUrl !== 'string' || !blobUrl.startsWith('blob:')) return;
    activeOffers.add(id);
    try {
      if (!enabled) {
        post('fdm-blob-native', { id });
        return;
      }
      // WebSocket 必须由扩展 Service Worker 建立：公网页面内容脚本连
      // 127.0.0.1 会被 Chrome 的本地网络访问策略拦截。
      const result = await chrome.runtime.sendMessage({
        type: 'fdm:blobStart',
        blobUrl,
        filename: String(data.filename || 'download.bin'),
        size: Number(data.size) || 0,
        pageUrl: String(data.pageUrl || location.href)
      });
      // 传输已开始后失败不回退浏览器，避免同一文件被下载两次。
      if (result && (result.ok === true || result.started === true)) {
        post('fdm-blob-finish', { id });
      } else {
        post('fdm-blob-native', { id });
      }
    } catch (e) {
      console.warn('[FDM Ext] Blob 接管失败:', e);
      selfHealIfStale(e);
      post('fdm-blob-native', { id });
    } finally {
      activeOffers.delete(id);
    }
  }

  window.addEventListener('message', (event) => {
    if (event.source !== window) return;
    const data = event.data;
    if (!data || data.channel !== CHANNEL) return;
    if (data.type === 'fdm-blob-hello') {
      post('fdm-blob-state', { enabled });
    } else if (data.type === 'fdm-blob-offer') {
      void handleOffer(data);
    }
  });

  chrome.runtime.onMessage.addListener((message) => {
    if (message && message.type === 'fdm:blobState') {
      applyState(message.enabled === true);
    }
  });

  void refreshState();
})();
