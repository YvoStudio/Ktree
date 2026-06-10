// Ktree embed sidecar 启动器(externalBin=embed)。pkg 打不动 ESM transformers.js +
// 原生 onnxruntime,故本启动器只做一件事:用系统 node 跑同级 embed-rt/embed.js,
// 透传 stdin/stdout(Rust 端的 JSON 请求-应答协议)。
const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');
const dir = path.dirname(process.execPath);        // = ktree.exe 所在目录
const rt = path.join(dir, 'embed-rt');
const entry = path.join(rt, 'embed.js');
// pkg 启动器 spawn 的子进程里 process.pkg/execPath 会指向启动器自身,故把模型目录
// 经环境变量显式传给 embed.js,绕开其内部路径推断。
const childEnv = Object.assign({}, process.env, { KTREE_MODEL_DIR: path.join(rt, 'models') });
// node 解析:优先 PATH 上的 node,回退常见安装路径
const candidates = ['node'];
if (process.platform === 'win32') candidates.push('C:\\Program Files\\nodejs\\node.exe');
function launch(i) {
  const child = spawn(candidates[i], [entry], { cwd: rt, env: childEnv, stdio: 'inherit', windowsHide: true });
  child.on('error', () => { if (i + 1 < candidates.length) launch(i + 1); else process.exit(1); });
  child.on('exit', (code) => process.exit(code == null ? 0 : code));
}
if (!fs.existsSync(entry)) { process.stderr.write('embed-rt/embed.js 不存在\n'); process.exit(1); }
launch(0);
