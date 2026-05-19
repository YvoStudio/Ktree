#!/usr/bin/env node
// Ktree 文档转换 sidecar
//
// 协议:从 stdin 读一行 JSON,向 stdout 写一行 JSON。
//   入:  { "input": "/abs/file.docx", "ext": "docx",
//          "ref_dir": "/abs/ref/<rel>", "ref_prefix": "../ref/<rel>" }
//   出成功:{ "ok": true, "markdown": "...", "title": "...", "summary": "..." }
//   出失败:{ "ok": false, "error": "..." }
//
// ref_dir   转换产生的图片等资源的输出目录(绝对路径)
// ref_prefix Markdown 中引用这些资源时用的相对路径前缀
//
// 支持:docx / xlsx / xls / pdf / html / htm / md / markdown / txt
// 老格式 .doc(二进制)不支持,返回错误。

const fs = require('fs');
const path = require('path');

const SUMMARY_LEN = 200;

function summarize(md) {
  const text = md
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/!\[[^\]]*\]\([^)]*\)/g, ' ')
    .replace(/[#>*_`|\-]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
  return text.slice(0, SUMMARY_LEN);
}

// 把 docx 内嵌图片抽取到 ref_dir,返回 mammoth 的 convertImage 处理器。
function makeImageHandler(mammoth, ctx) {
  let seq = 0;
  return mammoth.images.imgElement(async (image) => {
    const buf = await image.read();
    const ct = image.contentType || 'image/png';
    const ext = (ct.split('/')[1] || 'png').replace('jpeg', 'jpg');
    seq += 1;
    const name = `img${String(seq).padStart(3, '0')}.${ext}`;
    if (ctx.refDir) {
      fs.mkdirSync(ctx.refDir, { recursive: true });
      fs.writeFileSync(path.join(ctx.refDir, name), buf);
      ctx.wrote = true;
      return { src: `${ctx.refPrefix}/${name}` };
    }
    // 没给 ref_dir 时退化为 base64 内嵌
    return { src: `data:${ct};base64,${buf.toString('base64')}` };
  });
}

async function convertDocx(input, ctx) {
  const mammoth = require('mammoth');
  const TurndownService = require('turndown');
  const { value: html } = await mammoth.convertToHtml(
    { path: input },
    { convertImage: makeImageHandler(mammoth, ctx) }
  );
  const td = new TurndownService({ headingStyle: 'atx', codeBlockStyle: 'fenced' });
  return td.turndown(html || '');
}

function convertXlsx(input) {
  const XLSX = require('xlsx');
  const wb = XLSX.readFile(input);
  const parts = [];
  for (const name of wb.SheetNames) {
    const sheet = wb.Sheets[name];
    const rows = XLSX.utils.sheet_to_json(sheet, { header: 1, defval: '', blankrows: false });
    if (!rows.length) continue;
    parts.push(`## ${name}\n`);
    const width = Math.max(...rows.map((r) => r.length));
    const norm = (r) => {
      const cells = [];
      for (let i = 0; i < width; i++) cells.push(String(r[i] ?? '').replace(/\|/g, '\\|').replace(/\n/g, ' '));
      return `| ${cells.join(' | ')} |`;
    };
    parts.push(norm(rows[0]));
    parts.push(`|${' --- |'.repeat(width)}`);
    for (let i = 1; i < rows.length; i++) parts.push(norm(rows[i]));
    parts.push('');
  }
  return parts.join('\n');
}

async function convertPdf(input) {
  const pdfParse = require('pdf-parse');
  const buf = fs.readFileSync(input);
  const data = await pdfParse(buf);
  return (data.text || '').replace(/\n{3,}/g, '\n\n').trim();
}

function convertHtml(input) {
  const TurndownService = require('turndown');
  const html = fs.readFileSync(input, 'utf8');
  const td = new TurndownService({ headingStyle: 'atx', codeBlockStyle: 'fenced' });
  return td.turndown(html);
}

function convertPlain(input) {
  return fs.readFileSync(input, 'utf8');
}

async function convert(input, ext, ctx) {
  const e = (ext || path.extname(input).slice(1)).toLowerCase();
  switch (e) {
    case 'docx':
      return convertDocx(input, ctx);
    case 'xlsx':
    case 'xls':
      return convertXlsx(input);
    case 'pdf':
      return convertPdf(input);
    case 'html':
    case 'htm':
      return convertHtml(input);
    case 'md':
    case 'markdown':
    case 'txt':
      return convertPlain(input);
    case 'doc':
      throw new Error('老式 .doc 二进制格式不支持,请另存为 .docx');
    default:
      throw new Error(`不支持的格式: .${e}`);
  }
}

function readStdin() {
  return new Promise((resolve, reject) => {
    let buf = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (c) => (buf += c));
    process.stdin.on('end', () => resolve(buf));
    process.stdin.on('error', reject);
  });
}

(async () => {
  let out;
  try {
    const raw = await readStdin();
    const req = JSON.parse(raw);
    if (!req.input) throw new Error('缺少 input 字段');
    if (!fs.existsSync(req.input)) throw new Error(`文件不存在: ${req.input}`);
    const ctx = {
      refDir: req.ref_dir || '',
      refPrefix: (req.ref_prefix || '').replace(/\/+$/, ''),
      wrote: false,
    };
    const markdown = (await convert(req.input, req.ext, ctx)) || '';
    out = {
      ok: true,
      markdown,
      title: path.basename(req.input, path.extname(req.input)),
      summary: summarize(markdown),
    };
  } catch (err) {
    out = { ok: false, error: String((err && err.message) || err) };
  }
  process.stdout.write(JSON.stringify(out));
})();
