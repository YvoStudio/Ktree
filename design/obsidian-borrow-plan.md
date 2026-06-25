# Obsidian 借鉴 — 落地清单

> 背景:对照 Obsidian 功能体系,筛选**贴合 Ktree「只读多源镜像 + 混合检索 + AI 接口」定位**的可借鉴功能。
> 结论:不追编辑器/插件生态赛道,只借鉴检索语法、属性数据库、链接发现等强化现有底座的能力。
> 评审日期:2026-06-24

## 定位前提(别丢的领先项)

Ktree 已比 Obsidian 强、需继续作为护城河、**不要去对标 Obsidian 的**:
- 原生离线语义检索(bge-zh 向量,Obsidian 要插件)
- MCP / REST API-first(Obsidian 无)
- VCS / 飞书严格镜像强同步(Obsidian 无原生)
- 零依赖打包部署(Tauri + Rust)

明确**不做**(偏离定位):WYSIWYG/实时预览编辑器、Daily Notes、Templates、Canvas 白板、Obsidian Sync/Publish 式付费云同步。

## 批次总览(按依赖与 ROI)

```
第一批(可并行,打地基 / 快赢)
  ① 搜索 operator 语法     ← 核心,且是 ② 的前置
  ⑤ Mermaid + 公式渲染     ← 最快赢,纯前端
  ④ Web Clipper            ← 独立赛道(浏览器端),可并行
第二批(依赖第一批)
  ② Properties + 属性视图   ← 复用 ① 的 property: 算子
  ③a Backlinks 面板         ← 链接解析 + 反链
第三批(可选 / 重投入)
  ③b 图谱视图               ← 贵在前端可视化,独立成增量
```

依赖关系:**① 是 ② 的前置;③a 是 ③b 的前置;⑤ 和 ④ 完全独立可并行。**

## 逐条落地卡

### ① 结构化搜索语法 〔M｜依赖 无｜先做〕 ✅ MVP 已实现(2026-06-24)
- 落地:`query_parser.rs`(新模块,含 9 个单测)/ `store::doc_ids_matching` / `search::hybrid` + `vector_search` + `filter_only_hits` / `mcp.rs` kb_search 描述。SQLite allowed-set 方案,**未动 tantivy schema、无需重建索引**。`cargo test --lib` 16 项全绿。
- 改动层:Rust 检索模块(query 解析层,jieba 分词 → tantivy 之前)
- 范围:
  - operator 解析器:`tag:` `path:` `kb:` `type:` `title:` + 布尔 / `"短语"` / `-` 取反
  - tantivy schema 确认 / 补齐可过滤字段(path、kb、type、tags 作为 indexed facet);**改 schema 需重建索引**
  - 嵌套标签匹配(`tag:a` 命中 `a/b`)= tags 字段前缀匹配,一并做掉
  - 算子同步暴露到 MCP `kb_search` 入参 + REST query → 让 AI 调用也能精确限定
- 注意:前端搜索框只透传字符串;成本在解析器 + schema 迁移
- 函数级改动点:见文末「附:① 代码改动点」(代码调研后补)

### ⑤ Mermaid + 数学公式渲染 〔S｜依赖 无｜可立刻做〕 ✅ 已实现(2026-06-24)
- 落地:vendor `lib/mermaid.min.js`(v9.4.3 UMD)+ `lib/tex-svg.js`(MathJax3,SVG 输出免字体);`http.rs` 加 `/lib/mermaid.min.js` `/lib/tex-svg.js` 两路;`webui.html` 加 MathJax 配置 + 脚本,`showMarkdownPreview` 接入 `protectMath/restoreMath`(控制字符占位避开 markdown 转义)+ `enhanceMarkdown`(```mermaid → .mermaid 渲染、MathJax typeset)。`cargo check` 通过,库完整性 + JS 语法已校验;浏览器内视觉效果待运行 app 确认。
- 说明:数学不用 KaTeX 是为免随包数十个字体文件;mermaid 不用 v10+ 是因其 ESM 分块不适合无构建步骤的单文件前端。
- 改动层:前端阅读视图(marked.js 渲染管线)
- 范围:vendor mermaid.js(```mermaid``` 块)+ KaTeX(`$...$` / `$$...$$`),挂到 marked renderer
- 注意:单文件前端,库需内联 / 随包;无后端、无索引改动
- 动机:VCS / 飞书技术文档大量含 mermaid 和公式,现在被当代码块 / 纯文本显示

### ④ Web Clipper 〔M｜依赖 无｜独立赛道〕 ✅ 已实现(2026-06-24)
- 落地:`web-clipper/`(MV3 扩展:manifest + popup + README)。**零服务端改动** —— 注入脚本提取正文/选区为 HTML(顶部附标题与来源链接、相对地址绝对化),以 `<标题>.html` 多部分上传到既有 `/api/upload?...&convert=md`,服务端 turndown 转 md 入库。配置(地址/知识库/子目录)存浏览器本地。popup.js / manifest 已校验。Chrome/Edge 开发者模式加载即用;实际剪藏需在浏览器里手测。
- 改动层:新建浏览器扩展(独立代码)+ 复用现有 REST 上传 + turndown sidecar
- 范围:
  - 扩展(MV3):content script 抓正文 / 选区 + popup,POST 到 Ktree
  - 服务端:加 `/clip` 端点(收 HTML+URL+title)或复用 upload;sidecar turndown 转 md 落到 upload 区
- 注意:另一个交付面(扩展打包 / 侧载),与其它条目资源不冲突,适合并行

### ② Properties + 属性视图 〔L｜依赖 ①〕 ✅ 已实现(2026-06-24)
- 落地:入库解析 frontmatter 顶层键为通用属性(`ingest::parse_frontmatter_props`,跳过 title/category/tags/summary 四个保留键),存 `documents.props`(JSON);`prop:key=val` / `prop:key` 算子(query_parser,复用 ① 的 allowed-set 过滤);前端 markdown 预览顶部「属性」面板。`/api/doc/:id` 与 `/api/backlinks` 回传 props。新增单测覆盖解析与过滤。
- 说明:完整的 Bases 式可交互表格视图(多视图 / 公式)未做,属性的「过滤视图」由 `prop:` 算子 + 搜索结果承担;表格构建 UI 列为后续。
- 改动层:Rust 入库 + SQLite + 检索 API + 前端新视图
- 范围:
  - 入库:frontmatter 从「只取 tags」扩展为通用 key-value
  - SQLite:新增 `doc_properties` 表(或 JSON 列),属性作为 facet 索引进 tantivy
  - API:按属性过滤 / 排序(复用 ① 的 `property:` 算子)
  - 前端:表格式「属性过滤库视图」(过滤 + 排序)
- 注意:成本主要在数据模型 + 前端视图;先打通数据 / 算子,UI 可后置

### ③a Backlinks 面板 〔M｜依赖 链接解析〕 ✅ 已实现(2026-06-24)
- 落地:入库解析 `[[wikilink]]` 与 `[](相对路径)`(`ingest::extract_links`,外链/锚点排除,去别名),存 `doc_links` 表(target_key = 去扩展名小写文件名);`GET /api/backlinks?kb=&path=`(回传目标 props + 反链列表)+ MCP `kb_backlinks` 工具;前端 markdown 预览底部「反向链接」面板(点击打开来源,复用 previewPathOf)。删除文档 / 删库时清理链接。单测覆盖 basename 解析与排自链。
- 边界:按去扩展名文件名匹配,同名文件可能过度匹配(小库可接受,已记);存量文档在下次(重)入库 / 缓存重建时回填(ingest_file 与 kbmeta 重建路径均已接)。
- **范式转向(2026-06-25,实测后)**:Ktree 的 md 多为同步/上传时各自独立生成,彼此无显式互链 → 反向链接对主力语料恒空,鸡肋。改为:① 阅读页主推 **「相关文档(语义)」面板**(`GET /api/related?kb=&path=`,目标向量 cosine top-K,阈值 0.5)+ MCP `kb_related` 工具;反向链接仅在确有互链时才显示。② 解析用 `store::resolve_doc` 兼容 src//docs/ 前缀与**转换产物按 md_path 匹配**(转换文档 rel_path 是源扩展名,预览走 docs/.md)。
- 改动层:Rust 入库链接解析 + SQLite + REST/MCP + 前端面板
- 范围:
  - 入库时解析 `[[wikilink]]` 和 `[](path)`,目标对文档索引解析
  - 新增 `doc_links` 表(src_doc, dst_doc, type)
  - API:查 dst = 当前文档 → 反链列表
  - 前端:文档页侧栏「反向链接」面板(顺带挂悬停预览)
- 注意:在只读镜像上做,不碰编辑器,纯增量

### ③b 图谱视图 〔L｜依赖 ③a｜可选 / 最后〕 ❌ 已移除(2026-06-25,实测后撤回)
- **结论:删除。** 语义图谱(113 文档 / 152 边)是个毛球,既梳理不出结构,Ktree 的场景(给人看内容 / 搜索 / 统一源分享 / MCP 给 AI 查文档)也没有「梳理全局关系」的需求。已删:左栏入口、前端 `showGraph/renderGraph/openGraphNode` + CSS、`/api/graph` 路由 + handler + `semantic_edges`、`store::all_links`。
- **保留的语义关系入口**:语义相似真正有用的形态不是全局图,而是「读某篇 / AI 查某篇时给出相关的几篇」→ **③ 相关文档面板 + `/api/related` + MCP `kb_related`** 保留。
- 教训:图谱是这轮借鉴里最「Obsidian 形状」、最不贴合 Ktree 的一项;按四个真实用途对账后应删。
- 以下为已删实现的历史记录:
### (历史)③b 曾经的实现 〔已删〕
- 落地:`GET /api/graph?kb=`(文档为节点、出链解析对为边,按知识库构建);前端左栏「关系图」入口 → 模态内 canvas 力导向图(自写斥力/引力/向心模拟,无第三方库),支持拖拽节点 / 平移 / 滚轮缩放 / 点击节点打开文档,度数越大节点越大。
- **语义图谱(2026-06-25 增补,因实测发现链接图对真实语料为空)**:`GET /api/graph?kb=&mode=semantic&k=&min=` 复用已算好的文档向量做 cosine top-K 连边(归一化向量 → 点积即相似度),边权=相似度;前端加「语义相似 / 显式链接」切换 + 相似度阈值滑杆,边的粗细/透明度按相似度。这是对 Ktree 语料(SVN 同步、散文式互引、无 wikilink)真正有意义的关系图,也是 Obsidian 原生做不到的。默认 k=4 / min=0.7(实测 107 篇 → 264 边、中位相似度 0.8)。
- 改动层:复用 `doc_links` 出图 JSON + 前端可视化;语义模式复用 `store::all_vectors`
- 范围:API 吐 links 图;前端引轻量力导向图库(canvas 系),做局部 / 全局图
- 注意:贵在前端可视化选型与性能;价值不如 ③a 直接,确认前面落地后再投

## 被砍掉的「中价值」项及原因

- 通用渲染(Callouts/Embed/脚注):仅当来源是 Obsidian 风格 md 才有用 → 按来源占比再说,优先级低
- Outline 大纲:可有可无
- 悬停预览:依赖 ③a 的链接才有意义 → 不独立做,依附 ③a
- Unlinked mentions:与现有语义检索冗余 → 丢弃
- 独立标签聚合视图:丢弃;嵌套标签匹配并入 ①

---

## 附:① 代码改动点(基于代码调研,file:line 为调研所得,实施前以实际代码为准)

### 检索链路现状
- REST 入口:`src-tauri/src/http.rs:1073` `search()` —— `GET /api/search?kb=&q=&limit=`,直接把 `q.q` 透传
- MCP 入口:`src-tauri/src/mcp.rs:333` `tool_search()` —— `kb_search` 工具,直接透传 `query`
- 核心融合:`src-tauri/src/search.rs:19` `hybrid()` —— ① BM25 路 `index.search()` ② 向量路 `vector_search()` ③ RRF 融合(`RRF_K=60`,`search.rs:46-93`),媒体无正文打 3 折,归一化 0-100
- BM25:`src-tauri/src/index.rs:136` `search()` —— jieba 分词 → **手工构造 BooleanQuery**(非 QueryParser),60% MSM + 短语加权;**当前唯一过滤是 `kb_id` 的 Must clause**
- 向量:`src-tauri/src/search.rs:98` `vector_search()` —— `store.all_vectors(kb)`(`store.rs:336`)暴力点积;**已支持 kb_id 的 SQL 过滤**

### 元数据来源(关键:过滤所需字段 SQLite 里都已有列)
`documents` 表(`src-tauri/src/store.rs:120-135`)已含独立列:`kb_id` / `rel_path` / `ext` / `tags`(逗号分隔,来自 frontmatter)/ `title` / `source`。
→ **`tag:` `path:` `type:` `kb:` 的过滤值,SQLite 里全都现成,无需进 tantivy。**

### 关键设计决策:走 SQLite allowed-set,不动 tantivy schema
- **不采用**子 agent 的「加 tantivy 字段 + 重建索引」方案(成本高、且过滤不需要打分)。
- **采用**:解析出约束 → 用 `documents` 表算出 `allowed: Option<HashSet<i64>>` → 对 BM25 和向量两路结果**取交集**。单一过滤真相源、零 schema 变更、零重建。
- 候选池:存在约束时把 `candidates`(现为 `limit*3`)放大,避免过滤后不足。小库定位下成本可忽略(向量本就全量遍历)。
- **何时才需要碰 tantivy**:仅当某元数据字段要参与**相关性排序**(而非过滤)时;当前需求不需要。`title:"短语"` 若要硬过滤,走 SQLite `title LIKE`;若只想加权,title 本就是已索引字段,现有短语加权已覆盖。
- 兼容:保留现有显式 `kb` 参数(默认值),`kb:` 算子可覆盖 / 叠加。

### 改动点清单(MVP,无需重建索引)
| 文件 | 函数 / 位置 | 改动 |
|---|---|---|
| `search.rs`(新增 `query_parser.rs` 亦可) | 新增 `parse_query(raw) -> (free_text, Vec<Constraint>)` | operator 解析:`tag:` / `path:` / `type:` / `kb:` / `title:` + 布尔 / `"短语"` / `-`取反;`tag:a` 前缀命中 `a/b` |
| `store.rs` | 新增 `doc_ids_matching(kb, &[Constraint]) -> Option<HashSet<i64>>` | 按约束对 `documents` 表出 WHERE,返回允许的 doc_id 集合(`tags LIKE` 注意按整段 / 前缀匹配逗号分隔值) |
| `search.rs:19` | `hybrid()` | 先 `parse_query`;算 `allowed`;`free_text` 传给 BM25 与 `vector_search`;两路结果用 `allowed` 过滤;约束存在时放大候选池 |
| `search.rs` | 空 `free_text` + 有约束 | **过滤-only 浏览模式**:跳过 BM25/向量打分,直接列 `allowed` 文档(按时间排序)——否则现有 `query.is_empty()` 早返回会吞掉纯算子查询 |
| `mcp.rs:104` | `kb_search` 工具描述 | 在 description 里说明支持的 operator 语法,让 AI 会用 |
| `http.rs:1073` / `mcp.rs:333` | 入口 | **无需改**,继续透传 query 字符串 |

### 边界与注意
- `tags` 是逗号分隔字符串:`tag:bug` 不能简单 `LIKE '%bug%'`(会误命中 `debugger`),要按分隔切分后整段 / 段前缀匹配。
- 纯算子无自由文本时务必走过滤-only,别让 `hybrid()`/`index.search()` 的空查询早返回。
- 向量路与 BM25 路共用同一 `allowed`,保证两路过滤一致。
- 分阶段:**MVP = SQLite allowed-set**(本清单);**仅当大库出现过滤选择性 / 性能问题**,再把热点过滤下推为 tantivy Must clause(那时才需 schema + 重建)。
