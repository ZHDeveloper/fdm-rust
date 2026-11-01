(() => {
  'use strict';

  const CHANNEL = 'fdm-blob-v1';
  const INSTALL_FLAG = '__fdmBlobMainInstalled';
  const MAX_BLOBS = 64;
  if (window[INSTALL_FLAG]) return;
  window[INSTALL_FLAG] = true;

  const nativePostMessage = window.postMessage.bind(window);
  const nativeFetch = window.fetch.bind(window);
  const blobUrls = new Map();
  const pending = new Map();
  const capturing = new WeakSet();
  const bypass = new WeakSet();
  let enabled = false;
  let requestSeq = 0;
  // 只接受最新桥接的消息；旧桥接在扩展重载后失效，会强制回退原生下载。
  let latestEpoch = 0;

  function post(type, extra = {}) {
    nativePostMessage({ channel: CHANNEL, type, ...extra }, '*');
  }

  function trimBlobUrls() {
    while (blobUrls.size > MAX_BLOBS) {
      const first = blobUrls.keys().next();
      if (first.done) break;
      blobUrls.delete(first.value);
    }
  }

  const originalCreateObjectURL = URL.createObjectURL;
  const originalRevokeObjectURL = URL.revokeObjectURL;
  if (typeof originalCreateObjectURL === 'function') {
    URL.createObjectURL = function createObjectURL(object) {
      const url = originalCreateObjectURL.call(URL, object);
      if (object instanceof Blob) {
        blobUrls.delete(url);
        blobUrls.set(url, object);
        trimBlobUrls();
      }
      return url;
    };
  }
  if (typeof originalRevokeObjectURL === 'function') {
    URL.revokeObjectURL = function revokeObjectURL(url) {
      try {
        return originalRevokeObjectURL.call(URL, url);
      } finally {
        setTimeout(() => blobUrls.delete(url), 60000);
      }
    };
  }

  function fallback(id) {
    const entry = pending.get(id);
    if (!entry) return;
    const { anchor, blob } = entry;
    pending.delete(id);
    capturing.delete(anchor);
    revokeSendUrl(entry);
    let fallbackUrl = '';
    const previousHref = anchor.getAttribute('href');
    if (blob) {
      try {
        fallbackUrl = originalCreateObjectURL.call(URL, blob);
        anchor.setAttribute('href', fallbackUrl);
      } catch (_) {}
    }
    bypass.add(anchor);
    try {
      anchor.click();
    } finally {
      bypass.delete(anchor);
      if (fallbackUrl) {
        // 点击已取走目标地址，立刻恢复页面原有的链接，避免 60 秒后撤销临时 URL 让页面链接失效。
        if (previousHref === null) anchor.removeAttribute('href');
        else anchor.setAttribute('href', previousHref);
        setTimeout(() => originalRevokeObjectURL.call(URL, fallbackUrl), 60000);
      }
    }
  }

  function finish(id) {
    const entry = pending.get(id);
    if (entry && entry.anchor) capturing.delete(entry.anchor);
    revokeSendUrl(entry);
    pending.delete(id);
  }

  // 交给扩展后台的临时 URL 只用于本次传输，结束后立即撤销。
  function revokeSendUrl(entry) {
    if (!entry || !entry.sendUrl) return;
    try { originalRevokeObjectURL.call(URL, entry.sendUrl); } catch (_) {}
    entry.sendUrl = '';
  }

  function safeFilename(anchor, blob) {
    let name = (anchor.getAttribute('download') || '').trim();
    if (!name && blob && typeof blob.name === 'string') name = blob.name.trim();
    name = name.replace(/[\\/:*?"<>|\u0000-\u001f]/g, '_').trim();
    if (!name || /^\.+$/.test(name)) return 'download.bin';
    return name;
  }

  function isBlobDownload(anchor) {
    return anchor &&
      anchor.nodeType === Node.ELEMENT_NODE &&
      anchor.tagName === 'A' &&
      anchor.hasAttribute('download') &&
      String(anchor.href || '').startsWith('blob:');
  }

  async function resolveBlob(url) {
    const cached = blobUrls.get(url);
    if (cached) return cached;
    try {
      const resp = await nativeFetch(url);
      if (resp.ok) return await resp.blob();
    } catch (_) {}
    return null;
  }

  function startCapture(anchor) {
    if (capturing.has(anchor)) return;
    capturing.add(anchor);

    const id = `${Date.now().toString(36)}-${(++requestSeq).toString(36)}`;
    pending.set(id, { anchor, blob: null, sendUrl: '' });
    resolveBlob(anchor.href)
      .then((blob) => {
        if (!enabled || !blob) {
          fallback(id);
          return;
        }
        const entry = pending.get(id);
        if (!entry) return;
        let sendUrl = '';
        try {
          sendUrl = originalCreateObjectURL.call(URL, blob);
        } catch (_) {}
        if (!sendUrl) {
          fallback(id);
          return;
        }
        entry.blob = blob;
        entry.sendUrl = sendUrl;
        post('fdm-blob-offer', {
          id,
          blobUrl: sendUrl,
          size: blob.size,
          filename: safeFilename(anchor, blob),
          pageUrl: location.href
        });
      })
      .catch(() => fallback(id));
  }

  function interceptAnchor(anchor, event) {
    if (!enabled || bypass.has(anchor) || !isBlobDownload(anchor)) return false;
    if (event) {
      event.preventDefault();
      event.stopImmediatePropagation();
    }
    startCapture(anchor);
    return true;
  }

  const nativeAnchorClick = HTMLAnchorElement.prototype.click;
  HTMLAnchorElement.prototype.click = function () {
    if (interceptAnchor(this, null)) return;
    return nativeAnchorClick.call(this);
  };

  const nativeAnchorDispatchEvent = HTMLAnchorElement.prototype.dispatchEvent;
  HTMLAnchorElement.prototype.dispatchEvent = function (event) {
    if (event && event.type === 'click' && interceptAnchor(this, event)) return false;
    return nativeAnchorDispatchEvent.call(this, event);
  };

  document.addEventListener('click', (event) => {
    const path = typeof event.composedPath === 'function' ? event.composedPath() : [event.target];
    const anchor = path.find(isBlobDownload);
    if (anchor) interceptAnchor(anchor, event);
  }, true);

  window.addEventListener('message', (event) => {
    if (event.source !== window) return;
    const data = event.data;
    if (!data || data.channel !== CHANNEL) return;
    if (typeof data.epoch === 'number') {
      if (data.epoch < latestEpoch) return;
      latestEpoch = data.epoch;
    }
    if (data.type === 'fdm-blob-state') {
      enabled = data.enabled === true;
      if (!enabled) {
        for (const id of Array.from(pending.keys())) fallback(id);
      }
    } else if (data.type === 'fdm-blob-native') {
      fallback(data.id);
    } else if (data.type === 'fdm-blob-finish') {
      finish(data.id);
    }
  });

  post('fdm-blob-hello');
})();
