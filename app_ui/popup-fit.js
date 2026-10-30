'use strict';

// 让采用“内容区 + 底部分割线/按钮栏”结构的弹窗按内容收高，
// 使最后一个模块到分割线保持与样式一致的 15px 留白。
(function () {
  // resizeWindow 设置的是页面内容区高度。
  let currentWinHeight = window.innerHeight || 0;

  function visible(el) {
    if (!el) return false;
    const style = getComputedStyle(el);
    return style.display !== 'none' && style.visibility !== 'hidden' && el.getClientRects().length > 0;
  }

  function activeScroller() {
    const pane = document.querySelector('.pane.active');
    const local = pane ? Array.from(pane.querySelectorAll('.scroll, .panel, .body')) : [];
    const all = Array.from(document.querySelectorAll('.scroll, .panel, .body'));
    return [...local, ...all].find((el) => visible(el));
  }

  window.fitPopupToContent = function fitPopupToContent(target, width, options) {
    // 允许页面在首次测量前直接给出内容区高度，避免扩展向导先以固定 400px 打开再收缩。
    if (options && Number.isFinite(options.initialHeight)) currentWinHeight = options.initialHeight;
    if (!window.api || !document.body) return;
    // 固定使用原始窗口控制方法，避免页面或测试替换 api 对象后失效。
    const resize = fitPopupToContent.resize ||
      (typeof window.api.resizeWindow === 'function' ? window.api.resizeWindow : null);
    const opts = options || {};
    const scroller = activeScroller();
    const foot = document.querySelector('.foot');
    if (!scroller || !foot) return;

    const viewportHeight = document.documentElement.clientHeight || 0;
    const frameHeight = Math.max(0, currentWinHeight - viewportHeight);
    const tabs = document.querySelector('.tabs');
    const tabsHeight = visible(tabs) ? tabs.getBoundingClientRect().height : 0;
    const minHeight = opts.minHeight == null ? 200 : opts.minHeight;
    const maxHeight = opts.maxHeight == null ? 850 : opts.maxHeight;
    // 内容区通常是 flex:1，当前窗口较高时 scrollHeight 会被可视高度撑满。
    // 临时按内容自然展开，才能测出真正的紧凑高度；分组框的 15px 底部留白不变。
    const prev = {
      flex: scroller.style.flex,
      height: scroller.style.height,
      minHeight: scroller.style.minHeight,
      overflowY: scroller.style.overflowY,
    };
    scroller.style.flex = '0 0 auto';
    scroller.style.height = 'auto';
    scroller.style.minHeight = '0';
    scroller.style.overflowY = 'visible';
    const contentHeight = scroller.getBoundingClientRect().height;
    scroller.style.flex = prev.flex;
    scroller.style.height = prev.height;
    scroller.style.minHeight = prev.minHeight;
    scroller.style.overflowY = prev.overflowY;
    const targetHeight = Math.max(
      minHeight,
      Math.min(maxHeight, Math.ceil(tabsHeight + contentHeight + foot.offsetHeight + frameHeight)),
    );
    const settleOverflow = (attempt) => {
      clearTimeout(fitPopupToContent.timer);
      fitPopupToContent.timer = setTimeout(() => {
        const current = activeScroller();
        if (!current || attempt <= 0) return;
        // Tauri 的 set_size 计算包含原生标题栏；若内容区仍有溢出，
        // 补足实际缺少的高度，确保弹窗完整展示且不出现纵向滚动条。
        const overflow = Math.ceil(current.scrollHeight - current.clientHeight);
        if (overflow <= 1) return;
        const nextHeight = Math.min(maxHeight, currentWinHeight + overflow);
        if (nextHeight <= currentWinHeight + 1) return;
        currentWinHeight = nextHeight;
        if (resize) resize(target, width, nextHeight);
        settleOverflow(attempt - 1);
      }, 140);
    };

    if (Math.abs(targetHeight - currentWinHeight) > 2) {
      currentWinHeight = targetHeight;
      if (resize) resize(target, width, targetHeight);
    }
    settleOverflow(3);
    return targetHeight;
  };
})();
