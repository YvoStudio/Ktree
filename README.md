# Ktree 知识库服务

跨平台知识库服务器:上传文档 → 自动转 Markdown → 建全文索引 → 局域网内通过 REST API 或 MCP 让人和 AI 快速检索。

基于 Tauri 2 + Rust,内置 HTTP 服务,**不依赖 nginx 等外部服务**,装完即用。文档转换逻辑(含飞书同步)由打包进应用的 Node sidecar 二进制承担,目标机无需安装 Node 或 Python。

## 主要特性

- **文档上传与转换** — docx / xlsx / pdf / html / md / txt 上传后可一键转 Markdown(Node sidecar:mammoth / xlsx / pdf-parse / turndown)
- **全文检索** — tantivy(Rust 版 Lucene)+ jieba 中文分词,BM25 排序,扛大量文档
- **元数据库** — SQLite 记录文档、分类,等价于一份可查询的 INDEX/KEYWORDS
- **REST API** — 绑 `0.0.0.0`,局域网任意客户端可上传 / 搜索 / 读取
- **MCP Server** — 内置 `/mcp` 端点(Streamable HTTP),Claude Code / Desktop 可直接接入,让 AI 直接检索知识库
- **飞书同步** — 内置 feishu sidecar,可手动触发或按间隔定时同步飞书共享文件夹(docx / 多维表格 / 画板思维导图)
- **桌面端** — 上传 / 搜索 / 文档管理 / 设置界面,关闭即隐藏到托盘,服务驻留后台

## 快速开始

### 开发模式

需要 [Rust](https://rustup.rs/) 1.77+ 和 Node.js 18+。

```bash
npm install                    # 前端 Tauri 依赖
npm --prefix sidecar install   # sidecar 转换器依赖
npm run dev                    # 启动应用 + HTTP 服务
```

启动后控制台会打印 `HTTP API 监听于 http://0.0.0.0:<port>`(优先 80,失败回退 8080)。

### 打包发行

```bash
node sidecar/build.js          # 把 sidecar 打包成当前平台自包含二进制
npm run build                  # 出当前平台安装包(externalBin 自动打包 sidecar)
```

跨平台:在各目标平台分别重跑 `node sidecar/build.js` + `npm run build`。

## REST API

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/health` | 服务状态、文档数 |
| POST | `/api/upload?convert=md&category=<cat>` | multipart 上传,可选转 Markdown |
| GET | `/api/search?q=<kw>&category=<cat>&limit=<n>` | BM25 全文搜索 |
| GET | `/api/index` | 全部文档元信息 |
| GET | `/api/categories` | 分类列表 |
| GET | `/api/doc/:id` | 文档元信息 |
| GET | `/api/doc/:id/md` | 转换后的 Markdown |
| GET | `/api/doc/:id/raw` | 原始文件 |
| DELETE | `/api/doc/:id` | 删除文档 |
| POST | `/api/sync/feishu/trigger` | 手动触发飞书全量同步 |

示例:

```bash
# 上传并转 Markdown
curl -F "file=@需求文档.docx" "http://192.168.1.10:8080/api/upload?convert=md&category=策划"

# 搜索
curl "http://192.168.1.10:8080/api/search?q=防沉迷&limit=10"
```

## MCP 接入(让 AI 直接读知识库)

Ktree 在 `/mcp` 暴露 MCP Server(Streamable HTTP transport),工具:`kb_search`、`kb_get_doc`、`kb_list_categories`、`kb_list_docs`、`kb_upload`。

### Claude Code

```bash
claude mcp add --transport http ktree http://192.168.1.10:8080/mcp
```

或写进项目的 `.mcp.json`:

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

### Claude Desktop

新版 Claude Desktop 支持 HTTP MCP,在 `claude_desktop_config.json` 写:

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

只支持 stdio 的旧客户端,可用 `mcp-remote` 桥接:

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

接入后,直接让 AI「搜一下知识库里关于 X 的文档」即可。

## 飞书同步

在桌面端「设置 → 飞书同步」填入飞书应用凭证:

- **App ID** / **App Secret** — 飞书开放平台自建应用凭证
- **共享文件夹 Token** — 要同步的飞书文件夹 token
- **自动同步间隔** — 分钟数,0 表示关闭定时同步(改后需重启应用)

填好后点「立即全量同步」,或等定时任务触发。飞书文档会转成 Markdown 存入 `<知识库根目录>/raw/feishu/` 并建立索引。飞书端删除的文档,同步时会一并从本地索引移除。

## 配置文件

```
macOS:   ~/Library/Application Support/studio.yvo.ktree/config.json
Windows: %APPDATA%\studio.yvo.ktree\config.json
Linux:   ~/.config/studio.yvo.ktree/config.json
```

知识库数据(原始文件、Markdown、SQLite、tantivy 索引)默认在对应的 `app_data_dir` 下,可在设置里改知识库根目录。

## 目录结构

```
Ktree/
  package.json              Tauri CLI + 前端依赖
  src/                      前端单页(管理界面)
  sidecar/                  Node 文档转换器
    convert.js              文档 → Markdown
    feishu-sync.js          飞书同步
    build.js                打包成自包含二进制
  src-tauri/
    src/
      lib.rs                Tauri builder + 托盘 + 启动 HTTP/调度
      config.rs             配置:知识库根、端口、飞书凭证、同步间隔
      store.rs              SQLite:documents / categories
      convert.rs            调 convert sidecar
      index.rs              tantivy 全文索引
      http.rs               axum REST API
      mcp.rs                MCP Server
      feishu.rs             调 feishu sidecar + 入库
      scheduler.rs          定时飞书同步
      commands.rs           Tauri invoke 命令
    binaries/               打包后的 sidecar 二进制(externalBin)
```

## 技术栈

- **外壳** — Tauri 2 + Rust,内置 axum HTTP 服务,系统托盘
- **索引** — tantivy + tantivy-jieba(中文分词)
- **元数据** — SQLite(rusqlite,bundled)
- **文档转换 / 飞书同步** — Node sidecar,发行时打包成自包含二进制(@yao-pkg/pkg)
- **前端** — 单文件 `src/index.html`,原生 ES6,无打包步骤

## License

GPL-3.0-or-later。
