const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const root = path.resolve(__dirname, '..');
const fixture = (name) => path.join(__dirname, 'fixtures', name);

function runConvert(input, refDir, vision) {
  const result = spawnSync(process.execPath, [path.join(root, 'convert.js')], {
    input: JSON.stringify({ input, ext: 'pptx', ref_dir: refDir, ref_prefix: 'sample.assets', vision }),
    encoding: 'utf8',
    maxBuffer: 4 * 1024 * 1024,
  });
  assert.equal(result.status, 0, result.stderr);
  return JSON.parse(result.stdout.trim());
}

test('PPTX 转 Markdown 保留逐页文字和幻灯片画面', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ktree-pptx-'));
  try {
    const result = runConvert(fixture('textbox.pptx'), dir);
    assert.equal(result.ok, true, result.error);
    assert.match(result.markdown, /第 1 页/);
    assert.match(result.markdown, /第 2 页/);
    assert.match(result.markdown, /This is test content/);
    assert.match(result.markdown, /sample\.assets\/slide-001\.svg/);
    assert.ok(fs.readFileSync(path.join(dir, 'slide-001.svg'), 'utf8').startsWith('<svg'));
    assert.ok(fs.existsSync(path.join(dir, 'slide-002.svg')));
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});

test('PPTX 图片落地到 assets，视觉结果写入 Markdown', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ktree-pptx-'));
  try {
    const { convertPptx } = await import('../pptx-convert.mjs');
    let analyzed = 0;
    const markdown = await convertPptx(fixture('image.pptx'), {
      refDir: dir, refPrefix: 'sample.assets',
    }, {
      analyzeImage: async () => {
        analyzed++;
        return { description: '一张测试图片的语义描述', visibleText: '图中文字' };
      },
    });
    assert.equal(analyzed, 3); // 相同图片跨页复用，不重复计费。
    assert.match(markdown, /图片内容：一张测试图片的语义描述/);
    assert.match(markdown, /图片中的文字：图中文字/);
    assert.match(markdown, /sample\.assets\/image-/);
    assert.equal(fs.readdirSync(dir).filter((name) => name.startsWith('slide-')).length, 2);
    assert.equal(fs.readdirSync(dir).filter((name) => name.startsWith('image-')).length, 3);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});

test('百炼响应只接受 JSON 里的描述与可见文字', () => {
  const { parseContent, validateVision } = require('../bailian-vision.js');
  assert.deepEqual(parseContent('```json\n{"description":"趋势上升","visible_text":"收入 12"}\n```'), {
    description: '趋势上升', visibleText: '收入 12',
  });
  assert.throws(() => validateVision({ api_key: 'secret', base_url: 'http://example.com/v1', model: 'qwen3.8-max' }));
  assert.equal(validateVision({ api_key: '', base_url: 'https://example.com/v1', model: 'qwen3.8-max' }), null);
});

test('百炼视觉请求只上传单张图片，携带 Bearer Key 和 JSON 输出约束', async () => {
  const { createImageAnalyzer } = require('../bailian-vision.js');
  const oldFetch = global.fetch;
  const image = fs.readFileSync(path.join(__dirname, '..', '..', 'src-tauri', 'icons', '128x128.png'));
  let called = 0;
  global.fetch = async (url, options) => {
    called++;
    assert.equal(url, 'https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions');
    assert.equal(options.headers.Authorization, 'Bearer fake-test-key');
    const body = JSON.parse(options.body);
    assert.equal(body.model, 'qwen3.8-max');
    assert.deepEqual(body.response_format, { type: 'json_object' });
    assert.equal(body.enable_thinking, false);
    assert.equal(body.messages[0].content[1].image_url.url,
      `data:image/png;base64,${image.toString('base64')}`);
    return { ok: true, text: async () => JSON.stringify({ choices: [{ message: {
      content: '{"description":"一只猫坐在椅子上","visible_text":"月报"}',
    } }] }) };
  };
  try {
    const analyzer = createImageAnalyzer({
      api_key: 'fake-test-key',
      base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
      model: 'qwen3.8-max',
    });
    assert.deepEqual(await analyzer(image, 'png'), {
      description: '一只猫坐在椅子上', visibleText: '月报',
    });
    assert.equal(called, 1);
  } finally { global.fetch = oldFetch; }
});

test('百炼错误提示不回显 API Key', async () => {
  const { createImageAnalyzer } = require('../bailian-vision.js');
  const oldFetch = global.fetch;
  global.fetch = async () => ({
    ok: false, status: 401,
    text: async () => '{"error":{"message":"fake-test-key is invalid"}}',
  });
  try {
    const analyzer = createImageAnalyzer({
      api_key: 'fake-test-key', base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
      model: 'qwen3.8-max',
    });
    await assert.rejects(analyzer(Buffer.from('png'), 'png'), (error) => {
      assert.match(error.message, /\[密钥已隐藏\]/);
      assert.doesNotMatch(error.message, /fake-test-key/);
      return true;
    });
  } finally { global.fetch = oldFetch; }
});

test('重转 PPTX 清理旧的幻灯片和图片资产，但保留其他文件', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ktree-pptx-'));
  try {
    fs.writeFileSync(path.join(dir, 'slide-099.svg'), '<svg/>');
    fs.writeFileSync(path.join(dir, 'image-0123456789abcdef.png'), 'old');
    fs.writeFileSync(path.join(dir, 'other.png'), 'keep');
    const { convertPptx } = await import('../pptx-convert.mjs');
    await convertPptx(fixture('textbox.pptx'), { refDir: dir, refPrefix: 'sample.assets' });
    assert.equal(fs.existsSync(path.join(dir, 'slide-099.svg')), false);
    assert.equal(fs.existsSync(path.join(dir, 'image-0123456789abcdef.png')), false);
    assert.equal(fs.existsSync(path.join(dir, 'other.png')), true);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});

test('百炼异常时仍输出 Markdown，并明确标记图片描述缺失', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ktree-pptx-'));
  try {
    const { convertPptx } = await import('../pptx-convert.mjs');
    const markdown = await convertPptx(fixture('image.pptx'), {
      refDir: dir, refPrefix: 'sample.assets',
    }, { analyzeImage: async () => { throw new Error('network unavailable'); } });
    assert.match(markdown, /自动识别失败；请检查百炼配置和网络后重新执行全库检查/);
    assert.match(markdown, /sample\.assets\/slide-001\.svg/);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});

test('PPTX 图表缓存数据进入 Markdown，便于搜索图表数值', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ktree-pptx-'));
  try {
    const { convertPptx } = await import('../pptx-convert.mjs');
    const markdown = await convertPptx(fixture('mixed.pptx'), {
      refDir: dir, refPrefix: 'sample.assets',
    });
    assert.match(markdown, /### 图表数据/);
    assert.match(markdown, /Costs/);
    assert.match(markdown, /Revenue/);
    assert.match(markdown, /360000/);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
