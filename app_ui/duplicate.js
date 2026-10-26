'use strict';
const $ = (s) => document.querySelector(s);
let _id = '';
let _actions = [];
let _defaultAction = 3;
let _kind = 'duplicate';

function getParam(name) {
  try { return new URLSearchParams(window.location.search).get(name) || ''; } catch (e) { return ''; }
}
function esc(s) {
  return String(s == null ? '' : s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
function setMsg(text) { $('#msg').textContent = text || ''; }

async function load() {
  _id = getParam('id');
  if (!_id) { setMsg('缺少重复下载请求 ID'); return; }
  try {
    const ctx = await window.api.duplicateContext(_id);
    const dup = ctx.duplicate || {};
    _kind = ctx.kind || 'duplicate';
    const conflict = _kind === 'name_conflict';
    _defaultAction = Number(ctx.defaultAction || (conflict ? 1 : 3));
    document.title = conflict ? '文件名已存在' : '重复的下载链接';
    if (conflict) {
      const fname = ctx.filename || dup.filename || '';
      $('#summary').textContent = ctx.reason === 'list'
        ? `文件名“${fname}”已在下载列表中，请选择处理方式。`
        : `文件已存在于磁盘：${ctx.path || fname}`;
    } else {
      $('#summary').textContent = `该文件在您的下载列表中已存在：${dup.filename || ''}`;
    }
    _actions = Array.isArray(ctx.actions) && ctx.actions.length ? ctx.actions : (conflict ? [
      { value: 1, label: `使用带序号的文件名：${ctx.suggested || '自动生成'}` },
      { value: 2, label: '覆盖同名文件' },
    ] : [
      { value: 1, label: '为重复的下载链接添加带序号的文件名' },
      { value: 2, label: '添加重复的下载链接，并覆盖已存在的文件' },
      { value: 3, label: '若原任务已完成，按完成设置处理；否则继续下载' },
    ]);
    const box = $('#options');
    box.innerHTML = '';
    _actions.forEach((a, idx) => {
      const label = document.createElement('label');
      label.className = 'opt';
      const radio = document.createElement('input');
      radio.type = 'radio';
      radio.name = 'dupAction';
      radio.value = String(a.value);
      radio.checked = Number(a.value) === _defaultAction || (idx === 0 && !_actions.some((x) => Number(x.value) === _defaultAction));
      const span = document.createElement('span');
      span.textContent = a.label;
      label.appendChild(radio);
      label.appendChild(span);
      box.appendChild(label);
    });
    $('#ok').disabled = false;
  } catch (e) {
    setMsg('加载重复下载信息失败：' + (e.message || e));
  }
}

async function submit() {
  const picked = document.querySelector('input[name="dupAction"]:checked');
  if (!picked) return;
  const action = Number(picked.value);
  $('#ok').disabled = true;
  $('#cancel').disabled = true;
  try {
    const res = await window.api.duplicateSubmit(_id, action, $('#remember').checked);
    if (!res || !res.ok) {
      setMsg((res && (res.error || res.code)) || '创建下载任务失败');
      $('#ok').disabled = false;
      $('#cancel').disabled = false;
      return;
    }
    if (res.id != null && res.action !== 'existing_done') {
      try { await window.api.openProgress(res.id); } catch (e) {}
    }
    // 关闭背后的“下载文件信息”窗口，再关闭当前询问窗口
    window.api.winClose('newdownload');
    window.api.closeSelf('duplicate');
  } catch (e) {
    setMsg('处理重复下载失败：' + (e.message || e));
    $('#ok').disabled = false;
    $('#cancel').disabled = false;
  }
}

async function cancel() {
  try { await window.api.duplicateCancel(_id); } catch (e) {}
  window.api.winClose('newdownload');
  window.api.closeSelf('duplicate');
}

$('#ok').onclick = submit;
$('#cancel').onclick = cancel;
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') { e.preventDefault(); cancel(); }
  if (e.key === 'Enter') { e.preventDefault(); submit(); }
});
load();
