import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {
  getShapeAltTitle,
  getGroupChildren,
  getShapeChartSpec,
  getShapeDescription,
  getShapeImageBytes,
  getShapeImageFormat,
  getShapeName,
  getSlideNotes,
  getSlideShapes,
  getSlideText,
  getSlideTitle,
  getSlides,
  loadPresentation,
} from '@office-kit/pptx';
import { renderSlideToSvg } from '@office-kit/pptx-preview';

const IMAGE_EXT = new Set(['png', 'jpeg', 'jpg', 'gif', 'webp', 'bmp', 'tif', 'tiff', 'svg']);
const VISION_EXT = new Set(['png', 'jpg', 'gif', 'webp', 'bmp']);

function cleanText(value) {
  return String(value ?? '').replace(/\r\n?/g, '\n').replace(/[ \t]+$/gm, '').trim();
}

function mdText(value) {
  return cleanText(value)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/\\/g, '\\\\').replace(/([`*_\[\]])/g, '\\$1')
    .split('\n').map((line) => line.replace(/^([#*+\-])/g, '\\$1')).join('\n');
}

function imageExt(format) {
  const e = String(format || '').toLowerCase().replace(/^image\//, '');
  if (e === 'jpeg') return 'jpg';
  return IMAGE_EXT.has(e) ? e : 'bin';
}

function imageLabel(shape) {
  return [getShapeDescription(shape), getShapeAltTitle(shape), getShapeName(shape)]
    .map(cleanText).find(Boolean) || '';
}

function tableCell(value) {
  return mdText(value).replace(/\n/g, ' ').replace(/\|/g, '\\|');
}

function* walkShapes(shapes) {
  for (const shape of shapes) {
    yield shape;
    yield* walkShapes(getGroupChildren(shape));
  }
}

function chartLines(spec) {
  if (!spec) return [];
  const lines = ['### 图表数据', ''];
  if (spec.title) lines.push(mdText(spec.title), '');
  const series = Array.isArray(spec.series) ? spec.series : [];
  const categories = Array.isArray(spec.categories) ? spec.categories : [];
  if (!series.length) return lines;
  lines.push(`| 分类 | ${series.map((s) => tableCell(s.name || '系列')).join(' | ')} |`);
  lines.push(`| --- | ${series.map(() => '---').join(' | ')} |`);
  const n = Math.min(200, Math.max(categories.length, ...series.map((s) => (s.values || []).length)));
  for (let i = 0; i < n; i++) {
    const values = series.map((s) => tableCell((s.values || [])[i] ?? ''));
    lines.push(`| ${tableCell(categories[i] ?? i + 1)} | ${values.join(' | ')} |`);
  }
  if (Math.max(categories.length, ...series.map((s) => (s.values || []).length)) > n) {
    lines.push('', `图表仅展示前 ${n} 行数据。`);
  }
  lines.push('');
  return lines;
}

/**
 * 从原始 PPTX 生成逐页 Markdown、整页 SVG 与单张图片。
 * analyzeImage 由调用方提供，可选；模型不可用不妨碍基础转换。
 */
export async function convertPptx(input, ctx, { analyzeImage } = {}) {
  const bytes = fs.readFileSync(input);
  const presentation = await loadPresentation(bytes);
  const slides = getSlides(presentation);
  if (!slides.length) throw new Error('PPTX 不含幻灯片');

  if (ctx.refDir) fs.mkdirSync(ctx.refDir, { recursive: true });
  const stem = path.basename(input, path.extname(input));
  const parts = [`# ${mdText(stem)}`, ''];
  const seenImages = new Map();
  const generatedAssets = new Set();

  for (let i = 0; i < slides.length; i++) {
    const slide = slides[i];
    const text = cleanText(getSlideText(slide));
    const title = cleanText(getSlideTitle(slide)) || text.split('\n').find(Boolean) || '';
    const heading = title ? `## 第 ${i + 1} 页：${mdText(title).slice(0, 80)}` : `## 第 ${i + 1} 页`;
    parts.push(heading, '');

    // 幻灯片画面用于 docs 阅读视图；源 PPTX 在 src/ 中原样保留。
    try {
      const svg = renderSlideToSvg(presentation, slide);
      if (ctx.refDir && svg) {
        const name = `slide-${String(i + 1).padStart(3, '0')}.svg`;
        fs.writeFileSync(path.join(ctx.refDir, name), svg);
        generatedAssets.add(name);
        parts.push(`![第 ${i + 1} 页幻灯片](${ctx.refPrefix}/${name})`, '');
      }
    } catch (error) {
      console.error(`[ktree] PPTX 第 ${i + 1} 页画面渲染失败: ${error}`);
    }

    if (text) parts.push('### 页面文字', '', mdText(text), '');
    const notes = cleanText(getSlideNotes(slide));
    if (notes) parts.push('### 演讲备注', '', mdText(notes), '');

    for (const shape of walkShapes(getSlideShapes(slide))) {
      const chart = getShapeChartSpec(shape);
      if (chart) parts.push(...chartLines(chart));
    }

    let imageIndex = 0;
    for (const shape of walkShapes(getSlideShapes(slide))) {
      const image = getShapeImageBytes(shape);
      if (!image?.length) continue;
      imageIndex++;
      const format = imageExt(getShapeImageFormat(shape));
      const hash = crypto.createHash('sha256').update(image).digest('hex');
      const name = `image-${hash.slice(0, 16)}.${format}`;
      const label = imageLabel(shape);
      if (ctx.refDir && !seenImages.has(hash)) {
        fs.writeFileSync(path.join(ctx.refDir, name), image);
      }
      generatedAssets.add(name);
      let info = seenImages.get(hash);
      if (!info) {
        info = { ocr: '', description: '', note: '' };
        if (analyzeImage) {
          if (!VISION_EXT.has(format)) {
            info.note = `.${format} 图片暂不支持自动识别`;
          } else {
            try {
              const result = await analyzeImage(image, format, text, label);
              info.ocr = cleanText(result?.visibleText);
              info.description = cleanText(result?.description);
              if (!info.ocr && !info.description) throw new Error('百炼未返回可用的图片信息');
            } catch (error) {
              console.error(`[ktree] PPTX 图片识别失败: ${error}`);
              info.note = '自动识别失败；请检查百炼配置和网络后重新执行全库检查';
            }
          }
        }
        seenImages.set(hash, info);
      }
      parts.push(`### 图片 ${imageIndex}`, '');
      if (ctx.refDir && format !== 'bin') {
        parts.push(`![第 ${i + 1} 页图片 ${imageIndex}](${ctx.refPrefix}/${name})`, '');
      }
      if (label) parts.push(`原有图片说明：${mdText(label)}`, '');
      if (info.ocr) parts.push(`图片中的文字：${mdText(info.ocr)}`, '');
      if (info.description) parts.push(`图片内容：${mdText(info.description)}`, '');
      if (info.note) parts.push(`图片识别状态：${info.note}`, '');
    }
  }

  // 该 .assets 目录专属同名源文档；重转后清掉不再被当前幻灯片引用的旧资源。
  // 仅处理本转换器命名的文件，避免影响同名其他文档类型的资源。
  if (ctx.refDir) {
    for (const name of fs.readdirSync(ctx.refDir)) {
      if (/^(?:slide-\d+\.svg|image-[a-f0-9]{16}\.[a-z0-9]+)$/.test(name)
          && !generatedAssets.has(name)) {
        fs.rmSync(path.join(ctx.refDir, name), { force: true });
      }
    }
  }

  return parts.join('\n').trim() + '\n';
}
