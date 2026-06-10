#!/usr/bin/env bash
# 把语义检索 embed sidecar 部署到 dev1(Windows,系统 node 跑真实文件)。
# pkg 打不成单文件(ESM transformers + 原生 onnxruntime),故:
#   embed.exe(极小 pkg 启动器) + embed-rt/(embed.js + node_modules + 模型) + 系统 node。
# 详见 memory: ktree-embed-dev1-wiring。
#
# 前置:本机 sidecar/node_modules 与 sidecar/models 已就绪(npm i + 跑过一次 embed 下过模型)。
# 用法: DEV1_PASS=xxx scripts/deploy-embed-dev1.sh
set -euo pipefail

HOST="${DEV1_HOST:-niudan.baijing.studio}"
USER_="${DEV1_USER:-Administrator}"
PASS="${DEV1_PASS:?需设 DEV1_PASS 环境变量}"
KTREE_DIR='C:\Users\Administrator\AppData\Local\Ktree'
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SIDE="$ROOT/sidecar"
TMP="$(mktemp -d)"

ssh_() { sshpass -p "$PASS" ssh -o StrictHostKeyChecking=no -o ConnectTimeout=60 "$USER_@$HOST" "$1" 2>&1 | iconv -f GBK -t UTF-8 2>/dev/null | tr -d '\r'; }
scp_() { sshpass -p "$PASS" scp -o StrictHostKeyChecking=no "$1" "$USER_@$HOST:$2"; }

echo "[1/5] 打包 embed.exe 启动器(win-x64,交叉打包需 --no-bytecode)"
( cd "$SIDE" && npx @yao-pkg/pkg embed-launcher.js --target node22-win-x64 --no-bytecode --public --output "$TMP/embed.exe" >/dev/null )

echo "[2/5] 打包 embed-rt(剔除非 win 原生库,sharp 在远端 stub)"
tar -czf "$TMP/embed-rt.tgz" -C "$SIDE" \
  --exclude='node_modules/onnxruntime-node/bin/napi-v3/darwin' \
  --exclude='node_modules/onnxruntime-node/bin/napi-v3/linux' \
  --exclude='node_modules/onnxruntime-web/dist/*.wasm' \
  --exclude='node_modules/.cache' \
  embed.js models node_modules

echo "[3/5] 传输到 dev1"
scp_ "$TMP/embed-rt.tgz" 'Downloads/embed-rt.tgz' >/dev/null
scp_ "$TMP/embed.exe"    'Downloads/embed.exe' >/dev/null

echo "[4/5] 停 ktree、铺设 embed-rt + embed.exe、stub sharp"
ssh_ 'taskkill /IM ktree.exe /F & taskkill /IM embed.exe /F 2>NUL' >/dev/null || true
ssh_ "powershell -NoProfile -Command \"\$rt='$KTREE_DIR\embed-rt'; if(Test-Path \$rt){Remove-Item \$rt -Recurse -Force}; New-Item -ItemType Directory -Force \$rt | Out-Null; tar -xzf C:\Users\Administrator\Downloads\embed-rt.tgz -C \$rt; Copy-Item C:\Users\Administrator\Downloads\embed.exe '$KTREE_DIR\embed.exe' -Force; Set-Content -NoNewline -Path \$rt'\node_modules\sharp\lib\index.js' -Value 'module.exports=function(){throw new Error(\\\"sharp stub\\\")};'\""

echo "[5/5] 重启 ktree(触发 backfill_vectors 自动补算向量)"
ssh_ 'schtasks /run /tn KtreeStartOnce' >/dev/null
rm -rf "$TMP"
echo "完成。验证:几分钟后搜索分数应突破 50(BM25+向量双路命中)。"
