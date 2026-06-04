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
  // 逐行清洗:跳过目录引导点行、页码行、表格分隔行,取真正的正文做摘要。
  const body = md
    .split('\n')
    .filter((line) => {
      const t = line.trim();
      if (!t) return false;
      if (/\.{3,}/.test(t) || /……\s*\d+\s*$/.test(t)) return false; // 目录引导点 / 目录项
      if (/^[-–—\s|]*\d{0,4}[-–—\s|:]*$/.test(t)) return false; // 页码 / 表格分隔
      if (/^#{1,6}\s/.test(t)) return false; // 标题行不进摘要
      if (t === '目录' || t === '前言') return false; // 目录/前言标题词
      return true;
    })
    .join(' ');
  const text = body
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

// 清洗 pdf-parse 的朴素文本流:去目录引导点、页眉页脚页码、合并被排版硬断的中文行。
// pdf-parse 不做版面分析,表格会被打散成单列 —— 那个无法在纯文本层还原,这里只做可读性清洗。
function cleanPdfText(raw) {
  const lines = raw.split('\n');
  const out = [];
  for (let line of lines) {
    let s = line.replace(/ /g, ' ').replace(/[ \t]+$/g, '');
    const t = s.trim();

    // 纯页码行:"- 1 -" / "1" / "第 1 页" 之类,删掉
    if (/^[-–—\s]*\d{1,4}[-–—\s]*$/.test(t)) continue;
    if (/^第?\s*\d{1,4}\s*页?(\s*\/\s*共?\s*\d{1,4}\s*页?)?$/.test(t)) continue;

    // 目录引导点:"标题 ......... - 6 -" → "标题 …… 6"(把一长串点 + 尾部页码压扁)
    if (/\.{3,}/.test(s)) {
      s = s.replace(/\s*\.{3,}\s*-?\s*(\d+)?\s*-?\s*$/, (m, p) => (p ? `  …… ${p}` : ''));
      s = s.replace(/\.{3,}/g, ' ');
    }

    out.push(s);
  }

  // 合并被 PDF 排版硬断的中文段落:上一行以中文/逗号结尾(非句末标点)、
  // 当前行以中文开头 → 接到上一行(中文不加空格)。空行作为段落边界保留。
  const merged = [];
  for (let i = 0; i < out.length; i++) {
    const cur = out[i];
    const prev = merged.length ? merged[merged.length - 1] : '';
    const prevTrim = prev.trim();
    const curTrim = cur.trim();
    // 只合并"接近满行"被硬断的长行(≥14 字),避免把"目录""前言"这类独立短标题误接;
    // 目录项行(…… 页码)也不参与合并。
    const endsMidSentence =
      prevTrim.length >= 14 &&
      /[一-龥，、]$/.test(prevTrim) &&
      !/[。！？.!?:：；…]$/.test(prevTrim) &&
      !/……\s*\d+$/.test(prevTrim);
    const startsCjk = /^[一-龥]/.test(curTrim) && !/……\s*\d+$/.test(curTrim);
    if (merged.length && curTrim && endsMidSentence && startsCjk) {
      merged[merged.length - 1] = prevTrim + curTrim;
    } else {
      merged.push(cur);
    }
  }

  return merged.join('\n').replace(/\n{3,}/g, '\n\n').trim();
}

async function convertPdf(input) {
  const pdfParse = require('pdf-parse');
  const buf = fs.readFileSync(input);
  const data = await pdfParse(buf);
  return cleanPdfText(data.text || '');
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
  // 第三方库(pdf-parse 内置的 pdf.js 等)会用 console.log 往 stdout 打警告
  // (如 "Warning: TT: undefined function"),混进 JSON 输出会导致 Rust 端解析失败。
  // 把 stdout 写入劫持到 stderr,只有最后的结果 JSON 用原始 write 输出。
  const rawStdoutWrite = process.stdout.write.bind(process.stdout);
  process.stdout.write = (...args) => process.stderr.write(...args);
  console.log = (...args) => console.error(...args);

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
  // 结果 JSON 换行后输出(用未被劫持的原始 write),Rust 端取最后一行非空内容解析
  rawStdoutWrite('\n' + JSON.stringify(out));
})();
