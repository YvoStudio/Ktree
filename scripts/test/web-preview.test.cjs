const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const html = fs.readFileSync(path.join(__dirname, '../../src-tauri/src/webui.html'), 'utf8');

// 执行页面中的真实函数,避免测试另写一份预览路由而漏掉回归。
function functionSource(name) {
  const source = html.match(new RegExp(`^(?:async )?function ${name}\\([^\\n]*\\)\\{(?:[^\\n]*\\}|[\\s\\S]*?^\\})`, 'm'));
  assert.ok(source, `页面函数不存在: ${name}`);
  return source[0];
}

function pageContext(extra = {}) {
  const context = vm.createContext({
    encodeURIComponent,
    CUR_KB: '测试库', CUR_PATH: 'src/upload', VIEW_MODE: 'list',
    MD_EXT: new Set(['md', 'markdown']), IMG_EXT: new Set(['png', 'jpg']),
    HTML_EXT: new Set(['html', 'htm']), VIDEO_EXT: new Set(), AUDIO_EXT: new Set(),
    CODE_EXT: new Set(), TEXT_EXT: new Set(['txt']),
    esc: String, fmtSize: String, fmtDate: String, scorePill: String,
    fileIconHtml: () => '', fileIcon: () => '', colorDotHtml: () => '', actionsHtml: () => '',
    ...extra,
  });
  const functions = ['extOf', 'rawUrl', 'previewPathOf', 'fileRow', 'fileCard',
    'previewFile', 'showPdfPreview', 'openBacklink', 'doSearch'];
  vm.runInContext(functions.map(functionSource).join('\n'), context);
  return context;
}

const pdf = {
  kb_id: '测试库', doc_id: 123, name: '中文 报告.PDF', title: '中文报告', ext: 'pdf',
  rel_path: 'upload/中文 报告.PDF', md_path: 'docs/upload/中文 报告.md',
};

test('PDF 原件预览不依赖转换后的 Markdown', () => {
  const page = pageContext();
  for (const rel_path of ['src/upload/中文 报告.PDF', 'upload/中文 报告.PDF', 'vcs/svn/报告.pdf', 'cloud/feishu/课程/报告.pdf']) {
    const expected = rel_path.startsWith('src/') ? rel_path : 'src/' + rel_path;
    for (const md_path of ['docs/upload/中文 报告.md', null, '']) {
      assert.equal(page.previewPathOf({ rel_path, md_path }), expected);
    }
  }
  assert.equal(page.previewPathOf({ rel_path: 'docs/upload/原件.pdf', md_path: 'docs/upload/原件.md' }), 'docs/upload/原件.pdf');
  assert.equal(page.previewPathOf({
    rel_path: pdf.rel_path,
    get md_path() { assert.fail('PDF 预览不应读取关联 MD'); },
  }), 'src/' + pdf.rel_path);
});

test('PDF 文件列表和缩略图卡片都打开原件', () => {
  const page = pageContext();
  const file = { ...pdf, rel_path: 'src/' + pdf.rel_path };
  for (const render of [page.fileRow, page.fileCard]) {
    const row = render(pdf.kb_id, file);
    assert.match(row, /data-path="src\/upload\/中文 报告\.PDF"/);
    assert.match(row, /data-name="中文 报告\.PDF"/);
    assert.doesNotMatch(row, /data-path="docs\/upload\/中文 报告\.md"/);
  }
});

test('PDF 搜索结果直接指向原件,不请求 Markdown 正文', async () => {
  const nodes = new Map();
  const requests = [];
  const page = pageContext({
    gs: { value: '报告' },
    pushSearchHistory() {}, hideSearchHist() {}, clearHash() {}, renderCrumb() {}, renderSidebar() {},
    document: { getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, { style: {}, classList: { remove() {} }, innerHTML: '' });
      return nodes.get(id);
    } },
    api: async url => { requests.push(url); return { hits: [pdf] }; },
  });
  await page.doSearch();
  const row = nodes.get('listArea').innerHTML;
  assert.match(row, /data-path="src\/upload\/中文 报告\.PDF"/);
  assert.match(row, /data-name="中文 报告\.PDF"/);
  assert.equal(requests.length, 1);
  assert.ok(requests[0].startsWith('/api/search?'));
});

test('PDF 相关文档/反向链接入口直接预览原件', () => {
  const calls = [];
  const page = pageContext({
    createModal: (name, url) => ({ appendChild: element => calls.push({ name, url, element }) }),
    document: { createElement: tag => ({ tag }) },
    showMarkdownPreview() { assert.fail('PDF 不应打开 Markdown'); },
  });
  page.openBacklink(pdf);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].name, pdf.name);
  assert.equal(calls[0].element.tag, 'embed');
  assert.equal(calls[0].element.type, 'application/pdf');
  assert.equal(calls[0].element.src, '/%E6%B5%8B%E8%AF%95%E5%BA%93/src/upload/%E4%B8%AD%E6%96%87%20%E6%8A%A5%E5%91%8A.PDF');
  assert.equal(calls[0].url, calls[0].element.src);
});

test('显式打开 docs Markdown 仍走 Markdown,Office 和其它文件的路由保持不变', () => {
  const page = pageContext();
  assert.equal(page.previewPathOf({ rel_path: 'docs/upload/报告.md' }), 'docs/upload/报告.md');
  assert.equal(page.previewPathOf({ rel_path: 'src/upload/报告.txt', md_path: 'docs/upload/报告.md' }), 'docs/upload/报告.md');
  assert.equal(page.previewPathOf({ rel_path: 'src/upload/页面.html', md_path: 'docs/upload/页面.html' }), 'src/upload/页面.html');
  for (const ext of ['pptx', 'docx']) {
    assert.equal(page.previewPathOf({ rel_path: `upload/报告.${ext}`, md_path: 'docs/upload/报告.md' }), `src/upload/报告.${ext}`);
  }
  const calls = [];
  page.showMarkdownPreview = (...args) => calls.push(args);
  page.previewFile('/测试库/docs/upload/报告.md', '报告.md');
  assert.deepEqual(calls, [['/测试库/docs/upload/报告.md', '报告.md']]);
});
