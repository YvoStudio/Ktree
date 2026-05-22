#!/usr/bin/env node
// 把 Node sidecar(convert.js / feishu-sync.js)打包成自包含可执行二进制,
// 输出到 src-tauri/binaries/,文件名按 Tauri externalBin 要求带 target triple 后缀。
//
// 用法: node sidecar/build.js                          # 打当前平台
//       node sidecar/build.js <rust-target-triple>     # 交叉打包到指定 target
//   例: node sidecar/build.js x86_64-apple-darwin

const { execSync } = require('child_process');
const os = require('os');
const fs = require('fs');
const path = require('path');

// Rust target triple → pkg target(@yao-pkg/pkg 支持交叉打包,JS sidecar 跨架构通用)
const TRIPLE_PKG = {
  'aarch64-apple-darwin': { pkg: 'node22-macos-arm64', ext: '' },
  'x86_64-apple-darwin': { pkg: 'node22-macos-x64', ext: '' },
  'x86_64-pc-windows-msvc': { pkg: 'node22-win-x64', ext: '.exe' },
  'aarch64-unknown-linux-gnu': { pkg: 'node22-linux-arm64', ext: '' },
  'x86_64-unknown-linux-gnu': { pkg: 'node22-linux-x64', ext: '' },
};

// 解析目标:命令行给了 target triple 就按它(交叉打包),否则按当前平台。
function resolveTarget() {
  const wanted = process.argv[2];
  if (wanted && wanted !== '--all') {
    const t = TRIPLE_PKG[wanted];
    if (!t) {
      console.error(`不支持的 target triple: ${wanted}`);
      console.error(`支持: ${Object.keys(TRIPLE_PKG).join(', ')}`);
      process.exit(1);
    }
    return { pkg: t.pkg, triple: wanted, ext: t.ext };
  }
  const platform = os.platform();
  const arch = os.arch();
  if (platform === 'darwin') {
    return arch === 'arm64'
      ? { pkg: 'node22-macos-arm64', triple: 'aarch64-apple-darwin', ext: '' }
      : { pkg: 'node22-macos-x64', triple: 'x86_64-apple-darwin', ext: '' };
  }
  if (platform === 'win32') {
    return { pkg: 'node22-win-x64', triple: 'x86_64-pc-windows-msvc', ext: '.exe' };
  }
  // linux
  return arch === 'arm64'
    ? { pkg: 'node22-linux-arm64', triple: 'aarch64-unknown-linux-gnu', ext: '' }
    : { pkg: 'node22-linux-x64', triple: 'x86_64-unknown-linux-gnu', ext: '' };
}

const SIDECAR_DIR = __dirname;
const BIN_DIR = path.join(SIDECAR_DIR, '..', 'src-tauri', 'binaries');
const ENTRIES = ['convert', 'feishu-sync'];

function main() {
  const { pkg, triple, ext } = resolveTarget();
  fs.mkdirSync(BIN_DIR, { recursive: true });
  console.log(`目标平台: ${pkg}  (triple: ${triple})`);

  for (const name of ENTRIES) {
    const entry = path.join(SIDECAR_DIR, `${name}.js`);
    const out = path.join(BIN_DIR, `${name}-${triple}${ext}`);
    console.log(`\n打包 ${name}.js → ${out}`);
    execSync(
      `npx @yao-pkg/pkg "${entry}" --target ${pkg} --output "${out}"`,
      { stdio: 'inherit', cwd: SIDECAR_DIR }
    );
  }
  console.log('\n完成。二进制已就绪,tauri build 会通过 externalBin 打包进应用。');
  console.log('交叉打包其他平台:在对应平台重跑本脚本,或用 pkg 的 --target 多值能力。');
}

main();
