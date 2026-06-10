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

// 模型目录定位:
// - 打包后:由启动器(embed.exe)经 KTREE_MODEL_DIR 显式传入随包 models 的绝对路径
//   (不能靠 process.pkg / execPath —— pkg 启动器 spawn 的子进程里这俩会指向启动器自身)。
// - 开发期:无该环境变量,用 sidecar/models(__dirname)。
const MODEL_DIR = process.env.KTREE_MODEL_DIR || path.join(__dirname, 'models');
const OFFLINE = !!process.env.KTREE_MODEL_DIR; // 打包态纯离线,只用随包模型

let extractorPromise = null;

// 懒加载嵌入 pipeline:首个请求触发模型加载,之后复用。
async function getExtractor() {
  if (!extractorPromise) {
    extractorPromise = (async () => {
      const { pipeline, env } = await import('@xenova/transformers');
      // localModelPath 是「本地模型根目录」(从这里按 <root>/<modelId> 找);
      // cacheDir 是远程下载缓存。模型在 <MODEL_DIR>/Xenova/bge-small-zh-v1.5,
      // 两者都指向 MODEL_DIR,本地优先、命中即用。
      env.localModelPath = MODEL_DIR;
      env.cacheDir = MODEL_DIR;
      env.allowLocalModels = true;
      env.allowRemoteModels = !OFFLINE;
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
