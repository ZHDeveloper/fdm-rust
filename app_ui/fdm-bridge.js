// fdm-bridge.js - FDM 前端通信桥接层
// 将 window.api 完整对接到本地 Rust/Tauri HTTP/REST/SSE 服务
(function () {
  'use strict';

  // 基础 API 地址（默认同源相对路径）
  const BASE_URL = '';

  // 事件监听器注册表
  const listeners = {
    'task:update': [],
    'task:remove': [],
    'task:info': [],
    'task:log': [],
    'settings:changed': [],
    'progress:init': [],
    'update:status': [],
    'update:available': []
  };

  function addListener(name, cb) {
    if (!listeners[name]) listeners[name] = [];
    listeners[name].push(cb);
  }

  function emit(name, data) {
    const list = listeners[name] || [];
    for (const cb of list) {
      try {
        cb(data);
      } catch (e) {
        console.error(`[Event Error: ${name}]`, e);
      }
    }
  }

  // 通用 HTTP 请求封装
  async function request(path, method = 'GET', body = null) {
    const opts = {
      method,
      headers: {
        'Accept': 'application/json',
      }
    };
    if (body !== null) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    try {
      const resp = await fetch(BASE_URL + path, opts);
      if (!resp.ok) {
        let msg = `HTTP ${resp.status}`;
        let errData = null;
        try {
          errData = await resp.json();
          if (errData && errData.error) msg = errData.error;
        } catch (_) {}
        const err = new Error(msg);
        err.status = resp.status;
        err.data = errData;
        throw err;
      }
      return await resp.json();
    } catch (e) {
      console.warn(`[API Request Error] ${method} ${path}:`, e);
      throw e;
    }
  }

  // 原生系统消息框（Windows MessageBoxW），失败时退回浏览器弹窗
  async function showNativeMessage(message, title = 'FDM', kind = 'info') {
    try {
      const res = await request('/api/system/message_box', 'POST', {
        message: String(message), title, kind, confirm: false
      });
      return !!(res && res.confirmed);
    } catch (_) {
      // 原生弹窗不可用时退回浏览器确认框：只有明确点“确定”才返回 true，
      // 关闭/取消都返回 false，避免误删。
      return confirm(message);
    }
  }

  async function showNativeConfirm(message, title = 'FDM', kind = 'question') {
    try {
      const res = await request('/api/system/message_box', 'POST', {
        message: String(message), title, kind, confirm: true
      });
      return !!(res && res.confirmed);
    } catch (_) {
      return confirm(message);
    }
  }

  // SSE (Server-Sent Events) 实时事件监听
  let evtSource = null;
  function initSSE() {
    try {
      evtSource = new EventSource('/api/events');
      
      evtSource.addEventListener('task:update', (e) => {
        try {
          const data = JSON.parse(e.data);
          emit('task:update', data);
        } catch (err) {
          console.error('[SSE parse error: task:update]', err);
        }
      });

      evtSource.addEventListener('task:remove', (e) => {
        try {
          const data = JSON.parse(e.data);
          emit('task:remove', data);
        } catch (err) {
          console.error('[SSE parse error: task:remove]', err);
        }
      });

      evtSource.addEventListener('settings:changed', (e) => {
        try {
          const data = JSON.parse(e.data);
          emit('settings:changed', data);
        } catch (err) {}
      });

      evtSource.addEventListener('update:status', (e) => {
        try {
          emit('update:status', JSON.parse(e.data));
        } catch (err) {
          console.error('[SSE parse error: update:status]', err);
        }
      });

      evtSource.addEventListener('update:available', (e) => {
        try {
          emit('update:available', JSON.parse(e.data));
        } catch (err) {
          console.error('[SSE parse error: update:available]', err);
        }
      });

      evtSource.onerror = () => {
        // SSE 异常自动重连
        setTimeout(() => {
          if (evtSource && evtSource.readyState === EventSource.CLOSED) {
            initSSE();
          }
        }, 3000);
      };
    } catch (e) {
      console.error('[SSE Init Error]', e);
    }
  }

  initSSE();

  // 辅助：生成「当前屏幕」居中的弹窗特性字符串（浏览器弹窗降级模式）
  // 基准是当前窗口所在监视器的可用工作区（screen.availWidth/Height），而不是当前窗口自身外框尺寸，
  // 这样与 Tauri 原生路径（以屏幕为基准）的居中结果完全一致。
  function getCenteredPopupFeatures(width, height) {
    const w = Number(width) || 600;
    const h = Number(height) || 500;

    // 1. 定位当前窗口所在监视器的原点（screenLeft/screenTop 是屏幕绝对坐标）
    let scrX = 0, scrY = 0;
    try {
      if (window.screenLeft !== undefined || window.screenTop !== undefined) {
        scrX = window.screenLeft || 0;
        scrY = window.screenTop || 0;
      } else if (window.screenX !== undefined || window.screenY !== undefined) {
        scrX = window.screenX || 0;
        scrY = window.screenY || 0;
      }
    } catch (e) { /* noop */ }

    // 2. 以「所在屏幕的可用工作区」为基准计算居中坐标
    const availW = (window.screen && window.screen.availWidth) || 1920;
    const availH = (window.screen && window.screen.availHeight) || 1080;

    let left = Math.round(scrX + (availW - w) / 2);
    let top = Math.round(scrY + (availH - h) / 2);

    // 3. 夹取到屏幕内，避免越界到负坐标或超出可用区
    left = Math.max(scrX, Math.min(left, scrX + availW - w));
    top = Math.max(scrY, Math.min(top, scrY + availH - h));

    return `width=${w},height=${h},left=${left},top=${top},resizable=yes,scrollbars=yes`;
  }

  // 进度窗口参数解析
  function getUrlParam(name) {
    const match = RegExp('[?&]' + name + '=([^&]*)').exec(window.location.search);
    return match ? decodeURIComponent(match[1].replace(/\+/g, ' ')) : null;
  }

  // 识别当前页面所属的原生窗口目标 ID
  function resolveCurrentTarget() {
    try {
      const path = (window.location && window.location.pathname) ? window.location.pathname.toLowerCase() : '';
      if (path.includes('newdownload')) return 'newdownload';
      if (path.includes('extension-guide')) return 'extension_guide';
      if (path.includes('settings')) return 'settings';
      if (path.includes('progress')) {
        const tid = getUrlParam('taskId');
        return tid ? `prog_${tid}` : 'progress';
      }
      if (path.includes('dialog')) return 'dialog';
      return 'main';
    } catch (e) {
      return 'main';
    }
  }

  // ---- 重复下载 / 文件名冲突对话框（纯前端，所有窗口共用） ----
  function ensureConflictStyles() {
    if (document.getElementById('fdmConflictStyles')) return;
    const style = document.createElement('style');
    style.id = 'fdmConflictStyles';
    style.textContent = `
      .fdm-conflict-mask{position:fixed;inset:0;background:rgba(0,0,0,.28);z-index:2147483000;display:flex;align-items:center;justify-content:center;font-family:"Segoe UI","Microsoft YaHei",Tahoma,sans-serif}
      .fdm-conflict{width:380px;max-width:92vw;background:#f0f0f0;border:.5px solid rgba(0,0,0,.2);border-radius:6px;box-shadow:0 8px 28px rgba(0,0,0,.2);font-size:11px;color:#1a1a1a}
      .fdm-conflict .h{padding:10px 14px 4px;font-size:12px;font-weight:600}
      .fdm-conflict .b{padding:4px 14px 8px;line-height:1.55}
      .fdm-conflict .sub{color:#555;margin:2px 0 8px;word-break:break-all}
      .fdm-conflict label{display:flex;gap:7px;align-items:center;margin:6px 0;line-height:1.5;cursor:pointer}
      .fdm-conflict input[type=radio],.fdm-conflict input[type=checkbox]{margin:0;accent-color:#0078d7;flex:0 0 auto;position:relative;top:1px}
      .fdm-conflict .chk{margin-top:8px;color:#444}
      .fdm-conflict .f{display:flex;justify-content:flex-end;gap:8px;padding:8px 14px;border-top:.5px solid rgba(0,0,0,.1)}
      .fdm-conflict button{min-width:74px;min-height:24px;padding:3px 11px;border:.5px solid rgba(0,0,0,.2);border-radius:5px;background:#fff;font:inherit;cursor:pointer}
      .fdm-conflict button:hover{background:#e5f1fb;border-color:#0078d7}
    `;
    document.head.appendChild(style);
  }

  function openConflictDialog({ title, summary, options, defaultIndex, remember }) {
    ensureConflictStyles();
    return new Promise((resolve) => {
      const mask = document.createElement('div');
      mask.className = 'fdm-conflict-mask';
      const box = document.createElement('div');
      box.className = 'fdm-conflict';
      const name = 'fdm-dup-' + Math.random().toString(36).slice(2);
      box.innerHTML = `<div class="h"></div><div class="b"><div class="sub"></div><div class="opts"></div>${remember ? '<label class="chk"><input type="checkbox" data-remember><span>记住我的选择，之后按同样方式处理</span></label>' : ''}</div><div class="f"><button data-cancel>取消</button><button class="pri" data-ok>确定</button></div>`;
      box.querySelector('.h').textContent = title || '重复的下载链接';
      box.querySelector('.sub').textContent = summary || '';
      const optsEl = box.querySelector('.opts');
      options.forEach((opt, idx) => {
        const label = document.createElement('label');
        const radio = document.createElement('input');
        radio.type = 'radio';
        radio.name = name;
        radio.value = String(opt.value);
        radio.checked = idx === defaultIndex;
        const span = document.createElement('span');
        span.textContent = opt.label;
        label.appendChild(radio);
        label.appendChild(span);
        optsEl.appendChild(label);
      });
      const finish = (value) => { try { mask.remove(); } catch (_) {} resolve(value); };
      const ok = () => {
        const picked = box.querySelector(`input[name="${name}"]:checked`);
        if (!picked) return;
        const rem = box.querySelector('[data-remember]');
        finish({ value: Number(picked.value), remember: !!(rem && rem.checked) });
      };
      box.querySelector('[data-ok]').onclick = ok;
      box.querySelector('[data-cancel]').onclick = () => finish(null);
      mask.addEventListener('mousedown', (e) => { if (e.target === mask) finish(null); });
      mask.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') { e.preventDefault(); finish(null); }
        if (e.key === 'Enter') { e.preventDefault(); ok(); }
      });
      mask.tabIndex = -1;
      mask.appendChild(box);
      document.body.appendChild(mask);
      mask.focus();
    });
  }

  async function showDuplicateDialog(data) {
    const dup = data.duplicate || {};
    const actions = Array.isArray(data.actions) && data.actions.length ? data.actions : [
      { value: 1, label: '为重复的下载链接添加带序号的文件名' },
      { value: 2, label: '添加重复的下载链接，并覆盖已存在的文件' },
      { value: 3, label: '若原任务已完成，按完成设置处理；否则继续下载' },
    ];
    const def = Number(data.defaultAction || 3);
    const defaultIndex = Math.max(0, actions.findIndex((a) => Number(a.value) === def));
    const summary = `该文件在您的下载列表中已存在：${dup.filename || ''}`;
    const picked = await openConflictDialog({
      title: '重复的下载链接', summary, options: actions, defaultIndex, remember: true,
    });
    if (!picked) return null;
    return { action: picked.value, remember: picked.remember };
  }

  async function showNameConflictDialog(data) {
    const summary = data.reason === 'list'
      ? `文件名“${data.filename || ''}”已在下载列表中，请选择处理方式。`
      : `文件已存在于磁盘：${data.path || data.filename || ''}`;
    const options = [
      { value: 1, label: `使用带序号的文件名：${data.suggested || '自动生成'}` },
      { value: 2, label: '覆盖同名文件' },
    ];
    const picked = await openConflictDialog({
      title: '文件名已存在', summary, options, defaultIndex: 0, remember: true,
    });
    if (!picked) return null;
    return { fileAction: picked.value, rememberFileConflict: picked.remember };
  }

  // 挂载核心 API
  window.api = {
    // ---- 窗口控制 ----
    winMinimize: (target) => {
      const tgt = target || resolveCurrentTarget();
      return request('/api/system/window', 'POST', { action: 'minimize', target: tgt }).catch(() => {});
    },
    winMaximize: (target) => {
      const tgt = target || resolveCurrentTarget();
      return request('/api/system/window', 'POST', { action: 'maximize', target: tgt }).catch(() => {});
    },
    winClose: (target) => {
      const tgt = target || resolveCurrentTarget();
      return request('/api/system/window', 'POST', { action: 'close', target: tgt })
        .catch(() => {})
        .finally(() => {
          try { if (window.close) window.close(); } catch (e) {}
        });
    },
    resizeWindow: (target, width, height) => {
      const tgt = target || resolveCurrentTarget();
      return request('/api/system/window', 'POST', {
        action: 'resize',
        target: tgt,
        width,
        height
      }).catch(() => {});
    },

    // ---- 系统消息框（原生 MessageBoxW） ----
    message: (message, title = 'FDM', kind = 'info') =>
      showNativeMessage(message, title, kind),
    confirm: (message, title = 'FDM', kind = 'question') =>
      showNativeConfirm(message, title, kind),

    // ---- 任务列表与控制 ----
    list: async () => {
      try {
        const res = await request('/api/tasks');
        return Array.isArray(res) ? res : (res.tasks || []);
      } catch (e) {
        return [];
      }
    },

    add: async (opts) => {
      try {
        let payload = { ...(opts || {}) };
        let res = null;
        for (let attempt = 0; attempt < 4; attempt++) {
          try {
            res = await request('/api/tasks', 'POST', payload);
            break;
          } catch (e) {
            if (e && e.status === 409 && e.data && e.data.code === 'duplicate') {
              // 优先使用独立原生窗口；打不开时退回页面内弹窗。
              const opened = await window.api.openDuplicateRequest(payload, e.data);
              if (opened) return 'delegated';
              const choice = await showDuplicateDialog(e.data);
              if (!choice) return null;
              payload = { ...payload, duplicateAction: choice.action, rememberDuplicate: choice.remember };
              continue;
            }
            if (e && e.status === 409 && e.data && e.data.code === 'name_conflict') {
              const opened = await window.api.openPromptRequest('name_conflict', payload, e.data);
              if (opened) return 'delegated';
              const choice = await showNameConflictDialog(e.data);
              if (!choice) return null;
              payload = { ...payload, fileAction: choice.fileAction, rememberFileConflict: choice.rememberFileConflict };
              continue;
            }
            throw e;
          }
        }
        if (!res) return null;
        if (res.action === 'skipped') return null;
        if (res.action === 'existing_done') return 'existing_done';
        // 返回任务 ID (整型数字)
        if (res.id != null) return res.id;
        if (res.task && res.task.id != null) return res.task.id;
        return typeof res === 'number' ? res : null;
      } catch (e) {
        await showNativeMessage('添加下载失败：' + (e.message || e), 'FDM', 'error');
        return null;
      }
    },

    pause: (id) => request(`/api/tasks/${id}/pause`, 'POST'),
    resume: (id) => request(`/api/tasks/${id}/resume`, 'POST'),
    remove: (id, del) => request(`/api/tasks/${id}?delete_file=${!!del}`, 'DELETE'),
    removeAll: (del) => request(`/api/tasks/remove_all?delete_file=${!!del}`, 'POST'),

    // ---- 重复下载独立窗口 ----
    openPromptRequest: async (kind, opts, info) => {
      try {
        const res = await request('/api/duplicate/open', 'POST', {
          kind: kind || 'duplicate',
          opts: opts || {},
          ...(info || {})
        });
        return !!(res && res.opened);
      } catch (e) {
        return false;
      }
    },
    openDuplicateRequest: (opts, info) => window.api.openPromptRequest('duplicate', opts, info),
    duplicateContext: (id) => request('/api/duplicate/context/' + encodeURIComponent(id)),
    duplicateSubmit: (id, action, remember) =>
      request('/api/duplicate/submit/' + encodeURIComponent(id), 'POST', { action, remember: !!remember }),
    duplicateCancel: (id) => request('/api/duplicate/cancel/' + encodeURIComponent(id), 'POST'),

    // ---- 系统与路径 ----
    chooseDir: async () => {
      try {
        const res = await request('/api/system/choose_dir', 'POST');
        return res ? res.path : null;
      } catch (e) {
        return null;
      }
    },

    chooseSaveAs: async (name) => {
      try {
        const res = await request('/api/system/choose_save_as', 'POST', { name });
        return res ? res.path : null;
      } catch (e) {
        return null;
      }
    },

    defaultDir: async () => {
      try {
        const res = await request('/api/system/default_dir');
        return res ? res.path : '';
      } catch (e) {
        return '';
      }
    },
    probeSize: async (url, headers) => {
      try {
        return await request('/api/system/probe_size', 'POST', { url, headers: headers || null });
      } catch (e) {
        return { ok: false, size: null };
      }
    },
    chooseTorrentFile: async () => {
      try {
        const res = await request('/api/system/choose_torrent_file', 'POST');
        return res ? res.path : null;
      } catch (e) {
        return null;
      }
    },
    probeTorrent: async (body) => {
      try {
        return await request('/api/system/probe_torrent', 'POST', body || {});
      } catch (e) {
        return { ok: false, error: e.message || String(e) };
      }
    },
    engineStatus: async () => {
      try {
        return await request('/api/engines/status');
      } catch (e) {
        return { bt: { ready: false }, ed2k: { ready: false } };
      }
    },
    ed2kReconnect: async () => {
      try {
        return await request('/api/engines/ed2k/reconnect', 'POST');
      } catch (e) {
        return { ok: false };
      }
    },


    openFolder: (dir) => request('/api/system/open_path', 'POST', { path: dir }),
    openFile: (fp) => request('/api/system/open_path', 'POST', { path: fp }),

    // ---- 浏览器扩展相关 ----
    extensionInfo: async () => {
      try {
        return await request('/api/system/extension');
      } catch (e) {
        return { dir: '', connected: false };
      }
    },
    openExtensionGuide: async () => {
      try {
        const res = await request('/api/system/open_extension_guide', 'POST', {});
        return !!(res && res.opened);
      } catch (e) {
        return false;
      }
    },
    openBrowserExtensions: async (browser) => {
      try {
        const res = await request('/api/system/open_browser_extensions', 'POST', {
          browser: browser || 'chrome',
        });
        return !!(res && res.opened);
      } catch (e) {
        return false;
      }
    },

    // ---- 设置面板 ----
    getSettings: async () => {
      try {
        return await request('/api/settings');
      } catch (e) {
        return {};
      }
    },

    saveSettings: (s) => request('/api/settings', 'POST', s),
    settingsChooseDir: async () => {
      try {
        const res = await request('/api/system/choose_dir', 'POST');
        return res ? res.path : null;
      } catch (e) {
        return null;
      }
    },
    // 桌面模式必须由 Tauri 创建原生子窗口（原生标题栏 + 最小化/关闭）。
    // 不再退回浏览器 window.open 弹窗：那种窗口没有原生标题栏按钮。
    openSettings: async () => {
      const ctrl = typeof AbortController !== 'undefined' ? new AbortController() : null;
      let timer = null;
      try {
        const opts = {
          method: 'POST',
          headers: { 'Accept': 'application/json', 'Content-Type': 'application/json' },
          body: '{}'
        };
        if (ctrl) {
          opts.signal = ctrl.signal;
          timer = setTimeout(() => { try { ctrl.abort(); } catch (_) {} }, 5000);
        }
        const resp = await fetch(BASE_URL + '/api/system/open_settings', opts);
        if (resp.ok) {
          const res = await resp.json();
          if (res && res.opened) return true;
        }
      } catch (_) {}
      if (timer) clearTimeout(timer);
      try {
        if (window.fdmToast) window.fdmToast('打开设置窗口失败，请重试');
        else console.warn('[FDM] 原生设置窗口创建失败');
      } catch (_) {}
      return false;
    },
    settingsClose: () => (window.api && window.api.closeSelf ? window.api.closeSelf('settings') : (window.close && window.close())),
    onSettingsLoad: (cb) => {
      request('/api/settings').then(cb).catch(() => {});
    },

    // ---- 版本与更新 ----
    getVersion: async () => {
      try {
        const res = await request('/api/system/version');
        return (res && res.version) || '';
      } catch (_) {
        return '';
      }
    },
    getUpdateStatus: () => request('/api/system/update/status'),
    checkUpdate: () => request('/api/system/update/check', 'POST', {}),
    installUpdate: (version) => request('/api/system/update/install', 'POST', { version }),
    onUpdateStatus: (cb) => addListener('update:status', cb),
    onUpdateAvailable: (cb) => addListener('update:available', cb),

    // ---- 剪贴板 ----
    clipboardRead: async () => {
      try {
        if (navigator.clipboard && navigator.clipboard.readText) {
          return await navigator.clipboard.readText();
        }
      } catch (_) {}
      try {
        const res = await request('/api/system/clipboard_read', 'POST');
        return res ? (res.text || '') : '';
      } catch (e) {
        return '';
      }
    },

    quit: () => request('/api/system/quit', 'POST').catch(() => {}),

    // ---- 弹窗与独立进度窗口 ----
    // 优先调起原生独立 Tauri 子窗口（参 [复制 TDM] 「下载文件信息」窗口）。
    // 1) 桌面模式：通过 /api/system/open_new_download 由 Tauri 创建原生独立窗口；
    // 2) 浏览器模式：直接用浏览器弹窗打开 newdownload.html；
    // 3) 接口失败时退回到主窗口的内嵌 #addModal / window.open。
    openNewDownload: async (prefill) => {
      const data = prefill || {};
      try {
        const res = await request('/api/system/open_new_download', 'POST', data);
        if (res && res.opened) return true;
      } catch (_) {}
      try {
        const qs = (function () {
          const params = new URLSearchParams();
          Object.keys(data).forEach((k) => {
            const v = data[k];
            if (v == null) return;
            if (typeof v === 'object') params.set(k, JSON.stringify(v));
            else params.set(k, String(v));
          });
          params.set('reopen_ts', String(Date.now()));
          return '?' + params.toString();
        })();
        const w = window.open('newdownload.html' + qs, 'fdm_new', getCenteredPopupFeatures(720, 420));
        return !!w;
      } catch (_) {
        return false;
      }
    },
    newDownload: (prefill) => {
      // 桌面端：直接唤起原生独立窗口；只有接口返回无法弹窗时再回退主窗口内嵌。
      // 浏览器模式 / 独立进程无主窗口：使用浏览器新窗口打开独立页面。
      const data = prefill || {};
      let delegated = false;
      try {
        const p = window.api.openNewDownload(data);
        if (p && typeof p.then === 'function') {
          p.then((ok) => {
            if (!ok && window.openAdd) window.openAdd(data);
          }).catch(() => { if (window.openAdd) window.openAdd(data); });
          delegated = true;
        }
      } catch (_) {}
      if (!delegated) {
        if (window.openAdd) {
          window.openAdd(data);
        } else {
          window.open('newdownload.html', 'fdm_new', getCenteredPopupFeatures(720, 420));
        }
      }
    },

    openProgress: async (taskId) => {
      if (taskId == null) return false;
      const tid = typeof taskId === 'object' ? (taskId.id || taskId.taskId) : taskId;
      try {
        const res = await request('/api/system/open_progress', 'POST', { taskId: Number(tid) });
        if (res && res.opened) return true;
      } catch (_) {}
      try {
        const w = window.open(`progress.html?taskId=${tid}`, `fdm_prog_${tid}`, getCenteredPopupFeatures(680, 430));
        return !!w;
      } catch (_) {
        return false;
      }
    },

    closeSelf: (target) => {
      const tgt = target || resolveCurrentTarget();
      return request('/api/system/window', 'POST', { action: 'close', target: tgt })
        .catch(() => {})
        .finally(() => {
          try { if (window.close) window.close(); } catch (e) {}
        });
    },

    // ---- 事件订阅 ----
    onTaskUpdate: (cb) => addListener('task:update', cb),
    onTaskRemove: (cb) => addListener('task:remove', cb),
    onTaskInfo: (cb) => addListener('task:info', cb),
    onTaskLog: (cb) => addListener('task:log', cb),
    onMenu: () => {},
    onPrefill: (cb) => {
      const q = window.location.search;
      if (q) {
        const u = getUrlParam('url');
        const f = getUrlParam('filename');
        const s = getUrlParam('size') || getUrlParam('fileSize');
        const r = getUrlParam('referrer');
        const d = getUrlParam('dir');
        const a = getUrlParam('autoStart');
        const k = getUrlParam('kind');
        const tp = getUrlParam('torrentPath');
        const tu = getUrlParam('torrentUrl');
        const hRaw = getUrlParam('headers');
        let headers = null;
        if (hRaw) {
          try {
            headers = JSON.parse(hRaw);
          } catch (e) {
            console.warn('[onPrefill] Failed to parse headers JSON:', e);
          }
        }
        cb({
          url: u || '',
          filename: f || '',
          size: s ? Number(s) : null,
          referrer: r || '',
          dir: d || '',
          autoStart: a === 'true' || a === '1',
          kind: k || '',
          torrentPath: tp || '',
          torrentUrl: tu || '',
          headers: headers
        });
      }
    },

    // 独立进度窗口初始化
    onProgressInit: (cb) => {
      addListener('progress:init', cb);
      const tid = getUrlParam('taskId');
      if (tid) {
        request(`/api/tasks/${tid}`)
          .then((snap) => {
            if (snap) {
              cb({ taskId: Number(tid), snap });
            }
          })
          .catch((err) => {
            console.warn('[Progress Init Error]', err);
            cb({ taskId: Number(tid), snap: null });
          });
      }
    }
  };

})();
