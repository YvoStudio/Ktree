// Ktree 剪藏 — 弹窗逻辑。
// 思路:在页面里提取正文/选区为 HTML(顶部附标题与来源链接),
// 以 <title>.html 多部分上传到 Ktree 既有的 /api/upload?...&convert=md,
// 服务端用 turndown sidecar 转 Markdown 并入库。无需任何服务端改动。

const $ = (id) => document.getElementById(id);
const DEFAULTS = { base: 'http://127.0.0.1:8080', kb: '', folder: 'clips' };

function escapeHtml(s) {
  return String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
// 文件名清洗:去掉非法字符、压缩空白、限长;服务端还有一层 safe_component 兜底。
function sanitize(t) {
  return (t || 'untitled').replace(/[\/\\:*?"<>|\n\r\t]+/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 80) || 'untitled';
}
function status(msg, kind) {
  const el = $('status');
  el.textContent = msg || '';
  el.className = 'status ' + (kind || '');
}
function baseUrl() {
  return $('base').value.trim().replace(/\/+$/, '');
}

async function saveCfg() {
  await chrome.storage.local.set({ base: baseUrl(), kb: $('kb').value, folder: $('folder').value.trim() });
}

async function loadKbs(savedKb) {
  const sel = $('kb');
  sel.innerHTML = '<option value="">加载中…</option>';
  try {
    const r = await fetch(baseUrl() + '/api/kbs');
    const j = await r.json();
    const list = j.knowledge_bases || [];
    if (!list.length) { sel.innerHTML = '<option value="">(无知识库)</option>'; status('已连接,但没有知识库', 'err'); return; }
    sel.innerHTML = list.map(k => `<option value="${escapeHtml(k.id)}">${escapeHtml(k.name || k.id)}</option>`).join('');
    if (savedKb && list.some(k => k.id === savedKb)) sel.value = savedKb;
    status('已连接 · ' + list.length + ' 个知识库', 'ok');
  } catch (e) {
    sel.innerHTML = '<option value="">连接失败</option>';
    status('无法连接 Ktree:' + e.message, 'err');
  }
}

// 在目标页面上下文里执行(由 chrome.scripting 序列化注入,不能引用本文件其它变量)。
function extractInPage(mode) {
  try {
    const url = location.href;
    const title = document.title || 'untitled';
    const abs = (v) => { try { return new URL(v, location.href).href; } catch (e) { return v || ''; } };
    let container;
    const sel = window.getSelection();
    if (mode === 'selection' && sel && sel.rangeCount && sel.toString().trim()) {
      container = document.createElement('div');
      for (let i = 0; i < sel.rangeCount; i++) container.appendChild(sel.getRangeAt(i).cloneContents());
    } else {
      const srcEl = document.querySelector('article') || document.querySelector('main') || document.body;
      container = srcEl.cloneNode(true);
      container.querySelectorAll('script,style,noscript,nav,header,footer,aside,form,iframe,svg,button,input,select,textarea')
        .forEach(e => e.remove());
    }
    // 相对链接/图片绝对化,转 md 后仍能解析
    container.querySelectorAll('img[src]').forEach(e => e.setAttribute('src', abs(e.getAttribute('src'))));
    container.querySelectorAll('a[href]').forEach(e => e.setAttribute('href', abs(e.getAttribute('href'))));
    const safeTitle = String(title).replace(/[<>&]/g, '');
    const head = `<h1>${safeTitle}</h1>\n<p>来源:<a href="${url}">${url}</a></p>\n<hr>\n`;
    return { ok: true, title, url, html: head + container.innerHTML };
  } catch (e) {
    return { ok: false, error: String((e && e.message) || e) };
  }
}

async function clip(mode) {
  await saveCfg();
  const base = baseUrl();
  const kb = $('kb').value;
  const folder = ($('folder').value.trim() || 'clips').replace(/^\/+|\/+$/g, '');
  if (!kb) { status('请先选择知识库', 'err'); return; }

  status('提取页面…');
  let res;
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab || !tab.id) { status('无法获取当前标签页', 'err'); return; }
    const out = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: extractInPage, args: [mode] });
    res = out && out[0] && out[0].result;
  } catch (e) {
    status('注入失败(此页面可能禁止扩展):' + e.message, 'err');
    return;
  }
  if (!res || !res.ok) { status('提取失败:' + ((res && res.error) || '空内容'), 'err'); return; }

  status('上传到 Ktree…');
  try {
    const blob = new Blob([res.html], { type: 'text/html' });
    const fd = new FormData();
    fd.append('file', blob, sanitize(res.title) + '.html');
    const url = `${base}/api/upload?kb=${encodeURIComponent(kb)}&path=${encodeURIComponent('src/upload/' + folder)}&convert=md`;
    const r = await fetch(url, { method: 'POST', body: fd });
    const j = await r.json().catch(() => ({}));
    if (r.ok && j.ok) {
      const n = (j.documents && j.documents.length) || 1;
      status(`✓ 已剪藏到「${folder}」(${n} 篇)`, 'ok');
    } else {
      const err = (j.errors && j.errors[0] && j.errors[0].error) || ('HTTP ' + r.status);
      status('上传失败:' + err, 'err');
    }
  } catch (e) {
    status('上传失败:' + e.message, 'err');
  }
}

document.addEventListener('DOMContentLoaded', async () => {
  const c = await chrome.storage.local.get(DEFAULTS);
  $('base').value = c.base || DEFAULTS.base;
  $('folder').value = c.folder || DEFAULTS.folder;
  await loadKbs(c.kb);
  $('base').addEventListener('change', () => loadKbs($('kb').value));
  $('reload').addEventListener('click', () => loadKbs($('kb').value));
  $('kb').addEventListener('change', saveCfg);
  $('folder').addEventListener('change', saveCfg);
  $('clipPage').addEventListener('click', () => clip('page'));
  $('clipSel').addEventListener('click', () => clip('selection'));
});
