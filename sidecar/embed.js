#!/usr/bin/env node
// Ktree 语义向量 sidecar —— 常驻进程,把文本编码成句向量。
//
// 协议:常驻运行,从 stdin 按行读 JSON,每行一个请求,向 stdout 写一行 JSON 响应。
//   入: { "texts": ["...", "..."], "instruction": "可选,会前置到每条文本" }
//   出成功: { "ok": true, "vectors": [[...], ...], "dim": 512 }
//   出失败: { "ok": false, "error": "..." }
//
// 模型:bge-small-zh-v1.5(CLS pooling + L2 归一化),512 维。
// 首次运行会把模型下载并缓存到 sidecar/models/,之后离线复用。
// 检索时:文档向量直接编码;查询向量给 instruction 前缀(bge 检索惯例)。

const path = require('path');
const readline = require('readline');

const MODEL = 'Xenova/bge-small-zh-v1.5';
const CACHE_DIR = path.join(__dirname, 'models');

let extractorPromise = null;

// 懒加载嵌入 pipeline:首个请求触发模型加载,之后复用。
async function getExtractor() {
  if (!extractorPromise) {
    extractorPromise = (async () => {
      const { pipeline, env } = await import('@xenova/transformers');
      env.cacheDir = CACHE_DIR; // 模型缓存目录,首次下载后离线复用
      env.allowRemoteModels = true; // 允许首次联网下载
      return pipeline('feature-extraction', MODEL, { quantized: true });
    })();
  }
  return extractorPromise;
}

async function embed(texts, instruction) {
  const extractor = await getExtractor();
  const inputs = instruction ? texts.map((t) => instruction + t) : texts;
  // bge 用 CLS pooling;normalize 后向量是单位长度,cosine 相似度 = 点积。
  const output = await extractor(inputs, { pooling: 'cls', normalize: true });
  const dim = output.dims[output.dims.length - 1];
  const data = Array.from(output.data);
  const vectors = [];
  for (let i = 0; i < texts.length; i++) {
    vectors.push(data.slice(i * dim, (i + 1) * dim));
  }
  return { vectors, dim };
}

async function handleLine(line) {
  const trimmed = line.trim();
  if (!trimmed) return;
  let out;
  try {
    const req = JSON.parse(trimmed);
    const texts = Array.isArray(req.texts) ? req.texts : [];
    if (!texts.length) throw new Error('texts 不能为空');
    const { vectors, dim } = await embed(texts, req.instruction || '');
    out = { ok: true, vectors, dim };
  } catch (err) {
    out = { ok: false, error: String((err && err.message) || err) };
  }
  process.stdout.write(JSON.stringify(out) + '\n');
}

// 串行处理:Rust 端是请求-应答串行调用,这里也排队执行,避免响应乱序。
let queue = Promise.resolve();
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  queue = queue.then(() => handleLine(line));
});
