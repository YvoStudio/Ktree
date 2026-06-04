# Ktree 知识库服务

跨平台知识库服务器:上传文档 / 同步仓库 → 自动转 Markdown → 建混合索引 → 局域网内通过 REST API 或 MCP 让人和 AI 快速检索。

基于 Tauri 2 + Rust,内置 HTTP 服务,**不依赖 nginx 等外部服务**,装完即用。文档转换、飞书同步、语义向量等逻辑由打包进应用的 Node sidecar 承担,目标机无需安装 Node 或 Python。

## 主要特性

- **文档上传与转换** — docx / xlsx / pdf / html / md / txt 上传后可一键转 Markdown(Node sidecar:mammoth / xlsx / pdf-parse / turndown)
- **混合检索** — tantivy(Rust 版 Lucene)BM25 字面匹配 + jieba 中文分词,叠加 **语义向量检索**(bge-small-zh 本地嵌入模型),用 RRF 融合排序,能命中近义 / 概念相关的文档
- **元数据库** — SQLite 记录文档、分类与语义向量
- **VCS 同步** — 把 git / svn 仓库(可指定仓库内子目录,git 用稀疏检出)映射到知识库 `src` 子目录,支持手动触发或按间隔定时同步
- **飞书同步** — 内置 feishu sidecar,可手动触发或按间隔定时同步飞书共享文件夹(docx / 多维表格 / 画板思维导图)
- **REST API** — 绑 `0.0.0.0`,局域网任意客户端可上传 / 搜索 / 读取
- **MCP Server** — 内置 `/mcp` 端点(Streamable HTTP),Claude Code / Desktop 可直接接入,让 AI 检索知识库、并管理知识库配置
- **桌面端** — 上传 / 搜索 / 文档管理 / 设置界面,关闭即隐藏到托盘,服务驻留后台

## 快速开始

### 开发模式

需要 [Rust](https://rustup.rs/) 1.77+ 和 Node.js 18+。

```bash
npm install                    # 前端 Tauri 依赖
npm --prefix sidecar install   # sidecar 依赖(含语义嵌入模型库)
npm run dev                    # 启动应用 + HTTP 服务
```

启动后控制台会打印 `HTTP API 监听于 http://0.0.0.0:<port>`(优先 80,失败回退 8080)。
语义检索首次使用时会自动下载嵌入模型(约 23MB)缓存到 `sidecar/models/`,之后离线复用。

### 打包发行

```bash
node sidecar/build.js          # 把 sidecar 打包成当前平台自包含二进制
npm run build                  # 出当前平台安装包(externalBin 自动打包 sidecar)
```

跨平台:在各目标平台分别重跑 `node sidecar/build.js` + `npm run build`。

> 注:语义检索的 embed sidecar 目前仅开发模式(`node sidecar/embed.js`)可用,
> 尚未纳入 `build.js` 发行打包(pkg 打包 onnxruntime 原生插件 + 模型待处理)。
> 发行版在 embed 不可用时会自动退化为纯 BM25 检索。

## 检索说明

`kb_search` / `/api/search` 走混合检索:

1. **BM25** —— tantivy + jieba 分词,字面匹配。
2. **语义向量** —— 查询经 bge-small-zh 编码,与库内文档向量算 cosine。
3. **RRF 融合** —— 两路结果按名次融合排序,返回的「相关度」为 0–100 可读分(两路都靠前的文档接近 100)。

文档入库时即算向量;启动时会给存量文档补算。embed sidecar 不可用时自动退化为纯 BM25。

## REST API

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/health` | 服务状态、文档数 |
| GET | `/api/kbs` | 知识库列表 |
| GET | `/api/search?kb=<id>&q=<kw>&limit=<n>` | 混合检索(`kb` 可选) |
| POST | `/api/upload?kb=<id>&path=src/<dir>&convert=md` | multipart 上传,可选转 Markdown |
| GET | `/api/doc/:id` `/md` `/raw` | 文档元信息 / Markdown / 原始文件 |
| DELETE | `/api/doc/:id` | 删除文档 |
| GET | `/api/tree?kb=<id>` / `/api/files?kb=<id>&path=<dir>` | 目录树 / 目录内文件 |
| POST/DELETE | `/api/folder?kb=<id>&path=src/<dir>` | 新建 / 删除文件夹 |
| POST | `/api/sync/feishu/trigger?kb=<id>` | 手动触发飞书全量同步 |
| GET/POST | `/api/kb/:kb_id/vcs` | 列出 / 新增 VCS 绑定 |
| PUT/DELETE | `/api/kb/:kb_id/vcs/:idx` | 改 / 删 VCS 绑定 |
| POST | `/api/kb/:kb_id/vcs/sync` `/:idx/sync` | 触发 VCS 同步 |
| POST | `/api/kb` | 新增知识库 |
| PUT | `/api/kb/:kb_id/feishu` | 配置飞书 |
| GET/PUT | `/api/config` | 读取 / 整体替换配置 |

示例:

```bash
# 上传并转 Markdown
curl -F "file=@需求文档.docx" "http://192.168.1.10:8080/api/upload?kb=默认知识库&path=src/策划&convert=md"

# 混合检索
curl "http://192.168.1.10:8080/api/search?q=防沉迷&limit=10"
```

### 安全边界

REST API 与 `/mcp` 绑 `0.0.0.0`,局域网可访问,**默认无鉴权,面向可信局域网**。
其中**改配置类写操作**(`POST /api/kb`、`PUT /api/kb/:id/feishu`、VCS 绑定增删改、
`PUT /api/config`,以及对应的 MCP 写工具)**仅限本机 `127.0.0.1` 调用**,局域网调用会被拒。
检索 / 读取 / 上传文档等不受此限。公网暴露请自行在前置网关加鉴权。

## MCP 接入(Claude / Codex)

Ktree 在 `http://<host>:<port>/mcp` 暴露 Streamable HTTP MCP Server。AI 客户端接入后即可直接检索、读取和写入知识库:

- **检索 / 读取**:`kb_list`、`kb_search`、`kb_get_doc`、`kb_list_docs`、`kb_get_config`
- **写入内容**:`kb_upload`
- **配置管理(仅本机)**:`kb_create`、`kb_add_vcs`、`kb_update_vcs`、`kb_remove_vcs`
- **记事板**:`kb_list_notes`、`kb_add_note`

`<host>` 按使用场景替换:同机用 `127.0.0.1`,局域网其它机器用 Ktree 控制台 / 设置页显示的 IP。

### Claude

```bash
claude mcp add --transport http ktree http://192.168.1.10:8080/mcp
```

也可以写进项目 `.mcp.json` 或 Claude Desktop 配置:

```json
{
  "mcpServers": {
    "ktree": {
      "type": "http",
      "url": "http://192.168.1.10:8080/mcp"
    }
  }
}
```

旧客户端如果不支持 HTTP MCP,用 `mcp-remote` 桥接:

```json
{
  "mcpServers": {
    "ktree": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://192.168.1.10:8080/mcp"]
    }
  }
}
```

### Codex

推荐用 Codex CLI 添加(配置会被 Codex CLI / IDE Extension 共享):

```bash
codex mcp add ktree --url http://127.0.0.1:8080/mcp
```

也可以手动写入 `~/.codex/config.toml`,或可信项目的 `.codex/config.toml`:

```toml
[mcp_servers.ktree]
url = "http://127.0.0.1:8080/mcp"
```

接入后可直接对 Claude / Codex 说:「用 Ktree 搜一下知识库里关于 X 的文档」。配置管理类工具仅允许本机调用,所以 AI 客户端和 Ktree 不在同一台机器时只能检索 / 读取 / 上传内容。

## 飞书同步

在桌面端「设置 →(知识库)→ 飞书同步」填入飞书应用凭证:

- **App ID** / **App Secret** — 飞书开放平台自建应用凭证
- **共享文件夹 Token** — 要同步的飞书文件夹 token
- **自动同步间隔** — 分钟数,0 表示关闭;每个知识库各自配置,改后即时生效无需重启

填好后点「立即同步飞书」,或等定时任务触发。飞书文档会转成 Markdown 入库并建立索引。
飞书端删除的文档,同步时会一并从本地索引移除。

## VCS 同步

在桌面端「设置 →(知识库)→ VCS 绑定」可把 git / svn 仓库同步进知识库的 `src` 子目录:

- **git** — 可填「仓库子目录」,只稀疏检出该目录,内容扁平映射到指定 `src` 子目录
- **svn** — 直接把 URL 指到仓库内任意子目录即可
- 凭证留空走系统凭证;可设自动同步间隔

同步进来的文件会自动转换 / 入库 / 建索引,仓库端删除的文件本地一并清除。

## 配置文件

```
macOS:   ~/Library/Application Support/studio.yvo.ktree/config.json
Windows: %APPDATA%\studio.yvo.ktree\config.json
Linux:   ~/.config/studio.yvo.ktree/config.json
```

每个知识库根目录下维护 `src/`(源文件)、`docs/`(转换后的 Markdown)、`ref/`(转换产生的资源)、`.ktree/`(manifest 等元数据)四个子目录。知识库数据默认在 `app_data_dir` 下,可在设置里改根目录。

## 目录结构

```
Ktree/
  package.json              Tauri CLI + 前端依赖
  src/index.html            桌面端设置界面(单页)
  sidecar/                  Node sidecar
    convert.js              文档 → Markdown
    feishu-sync.js          飞书同步
    embed.js                语义向量嵌入服务(常驻)
    build.js                打包成自包含二进制
  src-tauri/
    src/
      lib.rs                Tauri builder + 托盘 + 启动 HTTP/调度
      config.rs             配置:知识库、端口、飞书 / VCS 绑定
      store.rs              SQLite:documents / doc_vectors
      ingest.rs             文件入库:转换 / 索引 / 向量
      convert.rs            调 convert sidecar
      embed.rs              调 embed sidecar(语义向量)
      index.rs              tantivy 全文索引
      search.rs             BM25 + 向量混合检索
      vcs.rs                git / svn 仓库同步
      kbmeta.rs             知识库元数据 / 缓存重建
      http.rs               axum REST API
      mcp.rs                MCP Server
      feishu.rs             调 feishu sidecar + 入库
      scheduler.rs          定时飞书 / VCS 同步
      commands.rs           Tauri invoke 命令
      webui.html            浏览器端 web 界面
    binaries/               打包后的 sidecar 二进制(externalBin)
```

## 技术栈

- **外壳** — Tauri 2 + Rust,内置 axum HTTP 服务,系统托盘
- **检索** — tantivy + tantivy-jieba(BM25 中文分词)+ 语义向量(SQLite 存向量,暴力 cosine)
- **语义嵌入** — bge-small-zh-v1.5(`@xenova/transformers`,本地离线)
- **元数据** — SQLite(rusqlite,bundled)
- **文档转换 / 飞书 / 嵌入** — Node sidecar,发行时打包成自包含二进制(@yao-pkg/pkg)
- **前端** — 单文件 `src/index.html` 与 `webui.html`,原生 ES6,无打包步骤

## License

GPL-3.0-or-later。
