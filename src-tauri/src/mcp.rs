// MCP Server over Streamable HTTP transport
//
// 挂在 axum 的 POST /mcp 上,讲 JSON-RPC 2.0。客户端(Claude Code / Desktop)
// 可直接把 http://<host>:<port>/mcp 配成 HTTP MCP server。
//
// 工具:kb_list / kb_search / kb_get_doc / kb_list_docs / kb_upload

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, State as AxState},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::config::{CloudBinding, VcsBinding};
use crate::ingest;
use crate::state::AppState;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// POST /mcp —— 接收单条或批量 JSON-RPC 消息。
/// (不提供 GET/SSE 通道;GET 请求由 axum 自动返回 405 + Allow: POST)
pub async fn handle(
    AxState(state): AxState<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // 改配置类写工具仅限本机调用 —— is_local 一路传到 call_tool。
    let is_local = addr.ip().is_loopback();
    // 知识库资源(图片等)的访问基地址:优先自定义域名,否则用请求的 Host 头。
    // kb_get_doc 用它把 md 里的相对图片路径改写成完整 URL。
    let base_url = {
        let cd = crate::commands::normalize_custom_domain(&state.config.snapshot().custom_domain);
        if !cd.is_empty() {
            cd
        } else {
            headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(|h| format!("http://{h}"))
                .unwrap_or_default()
        }
    };
    match body {
        Value::Array(items) => {
            let mut responses = Vec::new();
            for item in items {
                if let Some(resp) = handle_one(&state, item, is_local, &base_url).await {
                    responses.push(resp);
                }
            }
            if responses.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(responses)).into_response()
            }
        }
        obj => match handle_one(&state, obj, is_local, &base_url).await {
            Some(resp) => Json(resp).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// 处理单条 JSON-RPC 消息。notification(无 id)返回 None。
async fn handle_one(state: &AppState, msg: Value, is_local: bool, base_url: &str) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "ktree", "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(state, &params, is_local, base_url).await,
        other => Err((-32601, format!("未知方法: {other}"))),
    };

    Some(match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        }),
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "kb_list",
            "description": "列出所有知识库及其 id、名称、文档数。其它工具的 kb 参数用这里的 id。\n知识库源文件按来源分区,文档的 rel_path 都带区前缀:\n- upload/…       用户上传区,可通过 kb_upload 写入\n- vcs/<绑定名>/…  git/svn 仓库的只读镜像,内容由同步维护,要修改请提交到仓库\n显式忽略:任一路径层级的目录名或文件名以 ##! 或 ##！ 开头时,不转换、不索引,AI 工具也不会返回。",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kb_search",
            "description": "在知识库中做中文混合检索:BM25 字面匹配 + 语义向量,RRF 融合排序。能命中近义 / 概念相关的文档。返回标题、摘要、所属知识库。路径层级以 ##! 或 ##！ 开头的内容会被视为显式忽略,不会返回。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键词" },
                    "kb": { "type": "string", "description": "可选,限定某个知识库 id;不填搜全部" },
                    "limit": { "type": "integer", "description": "返回条数,默认 10,最多 50" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "kb_get_doc",
            "description": "按文档 id 读取全文。先用 kb_search 拿 id,再调用本工具;优先返回 Markdown,docs 失效时会回源到 src 文本文件。",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer", "description": "文档 id" } },
                "required": ["id"]
            }
        },
        {
            "name": "kb_list_docs",
            "description": "列出某个知识库的文档,可选按目录前缀过滤。\n注意:path 是文档 rel_path 的前缀(相对 src/,带区前缀),不要加 \"src/\" 或 \"docs/\";静态直链才使用 /<kb>/src/... 或 /<kb>/docs/...。\n示例:path=\"upload\" 列上传区全部;path=\"vcs/svn/版号申请版本\" 列仓库镜像区该目录下全部。\n显式忽略:任一路径层级的目录名或文件名以 ##! 或 ##！ 开头时,不转换、不索引,本工具不会列出。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "path": { "type": "string", "description": "可选,目录前缀。如 \"upload\" 或 \"vcs/svn/版号申请版本\";不要带 src/ 或 docs/ 前缀" }
                },
                "required": ["kb"]
            }
        },
        {
            "name": "kb_upload",
            "description": "把一段文本写入某知识库上传区 src/upload/<path>/<filename>,并建立索引。\n只有上传区(upload/)可写;仓库镜像区(vcs/<绑定名>/)只读 —— 那些内容由 git/svn 同步维护,要修改请到仓库里提交,下次同步自动更新。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "filename": { "type": "string", "description": "文件名,如 笔记.md" },
                    "content": { "type": "string", "description": "文本内容" },
                    "path": { "type": "string", "description": "可选,上传区内的目标子目录。如 \"会议纪要\" → 写入 upload/会议纪要/<filename>" },
                    "convert": { "type": "boolean", "description": "是否转 Markdown 并写入 docs/,默认 true" }
                },
                "required": ["kb", "filename", "content"]
            }
        },
        {
            "name": "kb_get_config",
            "description": "查看知识库配置:根目录、VCS(git/svn)绑定列表(密钥已脱敏)。改配置前先用它查绑定下标 idx。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "可选,限定某知识库 id;不填看全部" }
                }
            }
        },
        {
            "name": "kb_create",
            "description": "新建一个知识库,根目录自动放在应用数据目录下。返回新知识库 id。不支持删除知识库。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "知识库名称,需唯一" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "kb_add_vcs",
            "description": "给某知识库新增一条 VCS(git / svn)同步绑定:仓库内容严格镜像到 src/vcs/<name>/(只读目录)。返回绑定下标 idx。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "name": { "type": "string", "description": "绑定名 = src/vcs/ 下的目录名,库内唯一,创建后不可改" },
                    "vcs_type": { "type": "string", "description": "git 或 svn" },
                    "url": { "type": "string", "description": "仓库 URL" },
                    "repo_sub_path": { "type": "string", "description": "可选,仅 git:只稀疏检出仓库内的这个子目录" },
                    "branch": { "type": "string", "description": "可选,仅 git:分支" },
                    "username": { "type": "string", "description": "可选,凭证用户名;留空走系统凭证" },
                    "password": { "type": "string", "description": "可选,凭证密码 / token" },
                    "sync_interval_minutes": { "type": "integer", "description": "自动同步间隔(分钟),0=不自动" }
                },
                "required": ["kb", "name", "vcs_type", "url"]
            }
        },
        {
            "name": "kb_update_vcs",
            "description": "覆盖某知识库第 idx 条 VCS 绑定(整体替换,字段同 kb_add_vcs;name 不可改)。idx 用 kb_get_config 查。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "idx": { "type": "integer", "description": "VCS 绑定下标" },
                    "name": { "type": "string", "description": "绑定名(必须与原值一致,不可改)" },
                    "vcs_type": { "type": "string", "description": "git 或 svn" },
                    "url": { "type": "string", "description": "仓库 URL" },
                    "repo_sub_path": { "type": "string" },
                    "branch": { "type": "string" },
                    "username": { "type": "string" },
                    "password": { "type": "string" },
                    "sync_interval_minutes": { "type": "integer" }
                },
                "required": ["kb", "idx", "name", "vcs_type", "url"]
            }
        },
        {
            "name": "kb_remove_vcs",
            "description": "删除某知识库第 idx 条 VCS 绑定,并清掉 src/vcs/<name>/ 目录与已入库内容(镜像可重拉)。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "idx": { "type": "integer", "description": "VCS 绑定下标" }
                },
                "required": ["kb", "idx"]
            }
        },
        // 云文档(飞书)绑定的 kb_add_cloud / kb_update_cloud / kb_remove_cloud 工具已封存:
        // 后端 API 与同步链路保留,但不再对 MCP 客户端公开 —— 内网部署暂无云文档需求,
        // 以后需要时把工具定义加回来即可(分发逻辑 call_tool 里仍保留)。
        {
            "name": "kb_list_notes",
            "description": "列出公共记事板的全部记事(标题、类型、内容 / 链接)。记事板是知识库的共享便签区,所有人可见。",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kb_add_note",
            "description": "往公共记事板添加一条记事,所有人可见。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "记事标题" },
                    "note_type": { "type": "string", "description": "text 纯文本 / url 网络链接 / kblink 知识库链接,默认 text" },
                    "content": { "type": "string", "description": "纯文本内容,或 URL,或知识库相对路径" },
                    "kb": { "type": "string", "description": "仅 kblink:目标知识库 id" }
                },
                "required": ["title"]
            }
        }
    ])
}

/// 改配置类写工具 —— 仅允许本机调用。
const WRITE_TOOLS: [&str; 7] = [
    "kb_create",
    "kb_add_vcs",
    "kb_update_vcs",
    "kb_remove_vcs",
    "kb_add_cloud",
    "kb_update_cloud",
    "kb_remove_cloud",
];

/// 分发 tools/call。工具执行错误按 MCP 约定返回 result{isError:true}。
async fn call_tool(
    state: &AppState,
    params: &Value,
    is_local: bool,
    base_url: &str,
) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    // 写配置工具拦截:非本机调用直接拒绝。
    if WRITE_TOOLS.contains(&name) && !is_local {
        return Ok(json!({
            "content": [{
                "type": "text",
                "text": "错误: 该工具是改配置写操作,仅限本机(127.0.0.1)调用"
            }],
            "isError": true
        }));
    }

    let outcome = match name {
        "kb_list" => tool_list(state).await,
        "kb_search" => tool_search(state, &args).await,
        "kb_get_doc" => tool_get_doc(state, &args, base_url).await,
        "kb_list_docs" => tool_list_docs(state, &args).await,
        "kb_upload" => tool_upload(state, &args).await,
        "kb_get_config" => tool_get_config(state, &args).await,
        "kb_create" => tool_create_kb(state, &args).await,
        "kb_add_vcs" => tool_add_vcs(state, &args).await,
        "kb_update_vcs" => tool_update_vcs(state, &args).await,
        "kb_remove_vcs" => tool_remove_vcs(state, &args).await,
        "kb_add_cloud" => tool_add_cloud(state, &args).await,
        "kb_update_cloud" => tool_update_cloud(state, &args).await,
        "kb_remove_cloud" => tool_remove_cloud(state, &args).await,
        "kb_list_notes" => tool_list_notes(state).await,
        "kb_add_note" => tool_add_note(state, &args).await,
        other => return Err((-32602, format!("未知工具: {other}"))),
    };

    Ok(match outcome {
        Ok(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("错误: {e}") }],
            "isError": true
        }),
    })
}

async fn tool_list(state: &AppState) -> anyhow::Result<String> {
    let cfg = state.config.snapshot();
    if cfg.knowledge_bases.is_empty() {
        return Ok("还没有知识库。".to_string());
    }
    let mut out = String::from("知识库列表:\n");
    for kb in &cfg.knowledge_bases {
        let n = state
            .store
            .list_documents(&kb.id, None)
            .map(|docs| {
                docs.into_iter()
                    .filter(|d| !ingest::path_has_ignored_component(&d.rel_path))
                    .count()
            })
            .unwrap_or(0);
        out.push_str(&format!("- id={} 名称「{}」 {} 篇文档\n", kb.id, kb.name, n));
    }
    Ok(out)
}

async fn tool_search(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if query.trim().is_empty() {
        anyhow::bail!("query 不能为空");
    }
    let kb = args
        .get("kb")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // 指定了知识库就先校验存在 —— 与 kb_list_docs 行为一致,不存在直接报错。
    if let Some(ref kb_id) = kb {
        if state.config.get_kb(kb_id).is_none() {
            anyhow::bail!("知识库「{kb_id}」不存在");
        }
    }
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .clamp(1, 50) as usize;

    let st = state.clone();
    let hits = tokio::task::spawn_blocking(move || {
        crate::search::hybrid(&st, kb.as_deref(), &query, limit)
    })
    .await??;

    if hits.is_empty() {
        return Ok("未找到匹配文档".to_string());
    }
    let mut out = format!("找到 {} 条结果:\n\n", hits.len());
    for h in &hits {
        out.push_str(&format!(
            "### #{} {}\n- 知识库: {}\n- 分类: {}\n- 相关度: {:.0}\n- 摘要: {}\n- 取全文: kb_get_doc(id={})\n\n",
            h.doc_id, h.title, h.kb_id, h.category, h.score, h.summary, h.doc_id
        ));
    }
    Ok(out)
}

async fn tool_get_doc(state: &AppState, args: &Value, base_url: &str) -> anyhow::Result<String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("缺少整数参数 id"))?;
    let doc = state
        .store
        .get_document(id)?
        .ok_or_else(|| anyhow::anyhow!("文档 #{id} 不存在"))?;
    if ingest::path_has_ignored_component(&doc.rel_path) {
        anyhow::bail!("文档 #{id} 不存在");
    }
    let kb = state
        .config
        .get_kb(&doc.kb_id)
        .ok_or_else(|| anyhow::anyhow!("文档所属知识库不存在"))?;

    let mut content = ingest::read_doc_markdown(&kb, &doc)
        .unwrap_or_else(|_| format!("(该文档暂无可读 Markdown,请用 REST GET /api/doc/{id}/raw 下载原件)"));

    // 把 md 里的相对 .assets 资源引用改写成完整 URL,MCP 客户端才能直接显示图片。
    // 转换产物的引用形如 "<文件名去后缀>.assets/img_001.png"(与 md 同目录),
    // 对应直链 <base>/<kb>/docs/<父目录>/<同名>.assets/img_001.png。
    if !base_url.is_empty() {
        let stem = std::path::Path::new(&doc.rel_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if !stem.is_empty() {
            let parent = std::path::Path::new(&doc.rel_path)
                .parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let url_dir = if parent.is_empty() {
                format!("{base_url}/{}/docs", kb.id)
            } else {
                format!("{base_url}/{}/docs/{parent}", kb.id)
            };
            let assets_name = format!("{stem}.assets/");
            // markdown 引用 ![](xxx.assets/…) / [附件](xxx.assets/…)
            content = content.replace(
                &format!("]({assets_name}"),
                &format!("]({url_dir}/{assets_name}"),
            );
            // HTML 引用 <img src="xxx.assets/…"> / <video src=…> / <a href=…>
            content = content
                .replace(
                    &format!("src=\"{assets_name}"),
                    &format!("src=\"{url_dir}/{assets_name}"),
                )
                .replace(
                    &format!("href=\"{assets_name}"),
                    &format!("href=\"{url_dir}/{assets_name}"),
                );
        }
    }

    Ok(format!(
        "# {}\n\n> 知识库: {} | 路径: {} | 类型: .{}\n\n{}",
        doc.title, kb.name, doc.rel_path, doc.ext, content
    ))
}

async fn tool_list_docs(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = args
        .get("kb")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少 kb"))?;
    let kb = state
        .config
        .get_kb(kb_id)
        .ok_or_else(|| anyhow::anyhow!("知识库 {kb_id} 不存在"))?;
    let path = args.get("path").and_then(|v| v.as_str());
    let docs: Vec<_> = state
        .store
        .list_documents(&kb.id, path)?
        .into_iter()
        .filter(|d| !ingest::path_has_ignored_component(&d.rel_path))
        .collect();
    let mut out = format!("知识库「{}」共 {} 篇文档:\n", kb.name, docs.len());
    for d in &docs {
        out.push_str(&format!("- #{} {} ({})\n", d.id, d.title, d.rel_path));
    }
    Ok(out)
}

async fn tool_upload(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = args
        .get("kb")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少 kb"))?;
    let kb = state
        .config
        .get_kb(kb_id)
        .ok_or_else(|| anyhow::anyhow!("知识库 {kb_id} 不存在"))?;
    let raw_name = args
        .get("filename")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少 filename"))?;
    let filename =
        ingest::safe_component(raw_name).ok_or_else(|| anyhow::anyhow!("非法 filename"))?;
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少 content"))?;
    // path 是上传区内的子目录(相对 src/upload/);兼容直接传 "upload/xxx" 的写法
    let dir = match args.get("path").and_then(|v| v.as_str()) {
        None | Some("") => crate::config::AREA_UPLOAD.to_string(),
        Some(p) => {
            let rp = ingest::safe_rel_path(p).ok_or_else(|| anyhow::anyhow!("非法 path"))?;
            let full = if rp == crate::config::AREA_UPLOAD
                || rp.starts_with(&format!("{}/", crate::config::AREA_UPLOAD))
            {
                rp
            } else {
                format!("{}/{rp}", crate::config::AREA_UPLOAD)
            };
            full
        }
    };
    if filename.ends_with(".assets") {
        anyhow::bail!("文件名不能以 .assets 结尾(系统保留后缀)");
    }
    let convert_md = args
        .get("convert")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let rel_path = format!("{dir}/{filename}");
    if ingest::path_has_ignored_component(&rel_path) {
        anyhow::bail!("{}", ingest::ignore_rule_description());
    }
    let abs = kb.root.join("src").join(&rel_path);
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&abs, content.as_bytes()).await?;

    let st = state.clone();
    let kb2 = kb.clone();
    let rp = rel_path.clone();
    let doc = tokio::task::spawn_blocking(move || {
        let d = ingest::ingest_file(&st, &kb2, &rp, "mcp", convert_md, false)?;
        ingest::refresh_kb_meta(&st, &kb2)?;
        Ok::<_, anyhow::Error>(d)
    })
    .await??;

    Ok(format!(
        "已入库 #{} {} → 知识库「{}」 {}",
        doc.id, doc.title, kb.name, doc.rel_path
    ))
}

// ---- 改配置类工具:新增知识库 / 配置飞书 / 管理 VCS 绑定 ----

fn arg_kb_id(args: &Value) -> anyhow::Result<String> {
    args.get("kb")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("缺少 kb"))
}

fn arg_idx(args: &Value) -> anyhow::Result<usize> {
    args.get("idx")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .ok_or_else(|| anyhow::anyhow!("缺少整数参数 idx"))
}

async fn tool_get_config(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let cfg = state.config.snapshot();
    let filter = args
        .get("kb")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let mut out = String::new();
    for kb in &cfg.knowledge_bases {
        if filter.map(|f| f != kb.id).unwrap_or(false) {
            continue;
        }
        out.push_str(&format!("知识库「{}」(id={})\n", kb.name, kb.id));
        out.push_str(&format!("  根目录: {}\n", kb.root.display()));
        if kb.vcs_bindings.is_empty() {
            out.push_str("  VCS 绑定: 无\n");
        } else {
            out.push_str("  VCS 绑定:\n");
            for (i, b) in kb.vcs_bindings.iter().enumerate() {
                let mut line = format!("    [{}] {} {}", i, b.vcs_type, b.url);
                if !b.repo_sub_path.is_empty() {
                    line.push_str(&format!(" 仓库子目录={}", b.repo_sub_path));
                }
                line.push_str(&format!(" → src/vcs/{}", b.name));
                if !b.branch.is_empty() {
                    line.push_str(&format!(" 分支={}", b.branch));
                }
                line.push_str(&format!(
                    " 凭证={} 间隔={}分钟",
                    if b.username.is_empty() && b.password.is_empty() {
                        "系统"
                    } else {
                        "自定义"
                    },
                    b.sync_interval_minutes
                ));
                out.push_str(&line);
                out.push('\n');
            }
        }
        // 云文档功能已封存:有历史绑定才显示,没有就不提
        if !kb.cloud_bindings.is_empty() {
            out.push_str("  云文档绑定:\n");
            for (i, b) in kb.cloud_bindings.iter().enumerate() {
                out.push_str(&format!(
                    "    [{}] {} {}({}) → src/cloud/{}/{} app_id={} 间隔={}分钟\n",
                    i,
                    b.provider,
                    if b.target_type == "doc" { "单篇文档" } else { "文件夹" },
                    b.target_token,
                    b.provider,
                    b.name,
                    if b.app_id.is_empty() { "(空)" } else { &b.app_id },
                    b.sync_interval_minutes
                ));
            }
        }
        out.push('\n');
    }
    if out.is_empty() {
        if let Some(f) = filter {
            anyhow::bail!("知识库「{f}」不存在");
        }
        return Ok("还没有知识库。".to_string());
    }
    Ok(out)
}

async fn tool_create_kb(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少 name"))?;
    let kb = state.config.add_kb(name, None)?;
    Ok(format!(
        "已创建知识库「{}」(id={}),根目录 {}",
        kb.name,
        kb.id,
        kb.root.display()
    ))
}

async fn tool_add_vcs(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let binding: VcsBinding = serde_json::from_value(args.clone())?;
    let name = binding.name.clone();
    let idx = state.config.add_vcs_binding(&kb_id, binding)?;
    Ok(format!(
        "已为知识库「{kb_id}」新增 VCS 绑定「{name}」(idx={idx}),内容将镜像到 src/vcs/{name}/"
    ))
}

async fn tool_update_vcs(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let idx = arg_idx(args)?;
    let binding: VcsBinding = serde_json::from_value(args.clone())?;
    state.config.update_vcs_binding(&kb_id, idx, binding)?;
    Ok(format!("已更新知识库「{kb_id}」第 {idx} 条 VCS 绑定"))
}

async fn tool_remove_vcs(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let idx = arg_idx(args)?;
    let kb = state
        .config
        .get_kb(&kb_id)
        .ok_or_else(|| anyhow::anyhow!("知识库「{kb_id}」不存在"))?;
    let removed = state.config.remove_vcs_binding(&kb_id, idx)?;
    // 清掉本地镜像目录与已入库内容
    let st = state.clone();
    let purged = tokio::task::spawn_blocking(move || {
        crate::vcs::purge_binding_data(&st, &kb, &removed)
    })
    .await?
    .unwrap_or(0);
    Ok(format!(
        "已删除知识库「{kb_id}」第 {idx} 条 VCS 绑定,并清理本地镜像内容 {purged} 篇"
    ))
}

async fn tool_add_cloud(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let binding: CloudBinding = serde_json::from_value(args.clone())?;
    let name = binding.name.clone();
    let provider = binding.provider.clone();
    let idx = state.config.add_cloud_binding(&kb_id, binding)?;
    Ok(format!(
        "已为知识库「{kb_id}」新增云文档绑定「{name}」(idx={idx}),内容将镜像到 src/cloud/{provider}/{name}/"
    ))
}

async fn tool_update_cloud(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let idx = arg_idx(args)?;
    let binding: CloudBinding = serde_json::from_value(args.clone())?;
    state.config.update_cloud_binding(&kb_id, idx, binding)?;
    Ok(format!("已更新知识库「{kb_id}」第 {idx} 条云文档绑定"))
}

async fn tool_remove_cloud(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let idx = arg_idx(args)?;
    let kb = state
        .config
        .get_kb(&kb_id)
        .ok_or_else(|| anyhow::anyhow!("知识库「{kb_id}」不存在"))?;
    let removed = state.config.remove_cloud_binding(&kb_id, idx)?;
    let st = state.clone();
    let purged = tokio::task::spawn_blocking(move || {
        crate::feishu::purge_binding_data(&st, &kb, &removed)
    })
    .await?
    .unwrap_or(0);
    Ok(format!(
        "已删除知识库「{kb_id}」第 {idx} 条云文档绑定,并清理本地镜像内容 {purged} 篇"
    ))
}

// ---- 记事板:公共记事 ----

async fn tool_list_notes(state: &AppState) -> anyhow::Result<String> {
    let notes = state.store.list_notes()?;
    if notes.is_empty() {
        return Ok("公共记事板暂无记事。".to_string());
    }
    let mut out = format!("公共记事板共 {} 条:\n\n", notes.len());
    for n in &notes {
        let body = match n.note_type.as_str() {
            "url" => format!("网络链接 {}", n.content),
            "kblink" => format!("知识库链接 {} / {}", n.kb_id, n.content),
            _ => n.content.clone(),
        };
        out.push_str(&format!("- #{} 「{}」 {}\n", n.id, n.title, body));
    }
    Ok(out)
}

async fn tool_add_note(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let note_type = args
        .get("note_type")
        .and_then(|v| v.as_str())
        .unwrap_or("text")
        .to_string();
    if !matches!(note_type.as_str(), "text" | "url" | "kblink") {
        anyhow::bail!("note_type 必须是 text / url / kblink");
    }
    let n = crate::store::NewNote {
        title: args
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        note_type,
        content: args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        kb_id: args
            .get("kb")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        color: String::new(),
        pinned: false,
    };
    if n.title.trim().is_empty() && n.content.trim().is_empty() {
        anyhow::bail!("记事标题和内容不能都为空");
    }
    let note = state.store.add_note(&n)?;
    Ok(format!("已添加公共记事 #{} 「{}」", note.id, note.title))
}
