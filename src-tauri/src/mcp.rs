// MCP Server over Streamable HTTP transport
//
// 挂在 axum 的 POST /mcp 上,讲 JSON-RPC 2.0。客户端(Claude Code / Desktop)
// 可直接把 http://<host>:<port>/mcp 配成 HTTP MCP server。
//
// 工具:kb_list / kb_search / kb_get_doc / kb_list_docs / kb_upload

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, State as AxState},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::config::{FeishuConfig, VcsBinding};
use crate::ingest;
use crate::state::AppState;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// GET /mcp —— 不提供 server→client 主动推送的 SSE 通道。
pub async fn handle_get() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "Ktree MCP over HTTP:请用 POST 发送 JSON-RPC",
    )
        .into_response()
}

/// POST /mcp —— 接收单条或批量 JSON-RPC 消息。
pub async fn handle(
    AxState(state): AxState<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<Value>,
) -> Response {
    // 改配置类写工具仅限本机调用 —— is_local 一路传到 call_tool。
    let is_local = addr.ip().is_loopback();
    match body {
        Value::Array(items) => {
            let mut responses = Vec::new();
            for item in items {
                if let Some(resp) = handle_one(&state, item, is_local).await {
                    responses.push(resp);
                }
            }
            if responses.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(responses)).into_response()
            }
        }
        obj => match handle_one(&state, obj, is_local).await {
            Some(resp) => Json(resp).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// 处理单条 JSON-RPC 消息。notification(无 id)返回 None。
async fn handle_one(state: &AppState, msg: Value, is_local: bool) -> Option<Value> {
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
        "tools/call" => call_tool(state, &params, is_local).await,
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
            "description": "列出所有知识库及其 id、名称、文档数。其它工具的 kb 参数用这里的 id。",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kb_search",
            "description": "在知识库中做中文混合检索:BM25 字面匹配 + 语义向量,RRF 融合排序。能命中近义 / 概念相关的文档。返回标题、摘要、所属知识库。",
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
            "description": "按文档 id 读取全文(优先返回 Markdown 转换结果)。",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer", "description": "文档 id" } },
                "required": ["id"]
            }
        },
        {
            "name": "kb_list_docs",
            "description": "列出某个知识库的文档,可选按目录前缀过滤。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "path": { "type": "string", "description": "可选,限定 src 下的目录前缀" }
                },
                "required": ["kb"]
            }
        },
        {
            "name": "kb_upload",
            "description": "把一段文本写入某知识库的 src/<path>/<filename>,并建立索引。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "filename": { "type": "string", "description": "文件名,如 笔记.md" },
                    "content": { "type": "string", "description": "文本内容" },
                    "path": { "type": "string", "description": "可选,src 下的目标目录" },
                    "convert": { "type": "boolean", "description": "是否转 Markdown 并写入 docs/,默认 true" }
                },
                "required": ["kb", "filename", "content"]
            }
        },
        {
            "name": "kb_get_config",
            "description": "查看知识库配置:根目录、飞书同步设置、VCS 绑定列表(密钥已脱敏)。改配置前先用它查 VCS 绑定下标 idx。",
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
            "name": "kb_set_feishu",
            "description": "配置某知识库的飞书同步(整体覆盖)。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "app_id": { "type": "string", "description": "飞书应用 App ID" },
                    "app_secret": { "type": "string", "description": "飞书应用 App Secret" },
                    "folder_token": { "type": "string", "description": "飞书共享文件夹 token" },
                    "sync_interval_minutes": { "type": "integer", "description": "自动同步间隔(分钟),0=不自动" }
                },
                "required": ["kb"]
            }
        },
        {
            "name": "kb_add_vcs",
            "description": "给某知识库新增一条 VCS(git / svn)同步绑定,返回绑定下标 idx。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "vcs_type": { "type": "string", "description": "git 或 svn" },
                    "url": { "type": "string", "description": "仓库 URL" },
                    "sub_dir": { "type": "string", "description": "可选,同步到 src 下的子目录;空=src 根" },
                    "repo_sub_path": { "type": "string", "description": "可选,仅 git:只稀疏检出仓库内的这个子目录" },
                    "branch": { "type": "string", "description": "可选,仅 git:分支" },
                    "username": { "type": "string", "description": "可选,凭证用户名;留空走系统凭证" },
                    "password": { "type": "string", "description": "可选,凭证密码 / token" },
                    "sync_interval_minutes": { "type": "integer", "description": "自动同步间隔(分钟),0=不自动" }
                },
                "required": ["kb", "vcs_type", "url"]
            }
        },
        {
            "name": "kb_update_vcs",
            "description": "覆盖某知识库第 idx 条 VCS 绑定(整体替换,字段同 kb_add_vcs)。idx 用 kb_get_config 查。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "idx": { "type": "integer", "description": "VCS 绑定下标" },
                    "vcs_type": { "type": "string", "description": "git 或 svn" },
                    "url": { "type": "string", "description": "仓库 URL" },
                    "sub_dir": { "type": "string" },
                    "repo_sub_path": { "type": "string" },
                    "branch": { "type": "string" },
                    "username": { "type": "string" },
                    "password": { "type": "string" },
                    "sync_interval_minutes": { "type": "integer" }
                },
                "required": ["kb", "idx", "vcs_type", "url"]
            }
        },
        {
            "name": "kb_remove_vcs",
            "description": "删除某知识库第 idx 条 VCS 绑定(只是不再同步,不影响已入库内容)。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb": { "type": "string", "description": "知识库 id" },
                    "idx": { "type": "integer", "description": "VCS 绑定下标" }
                },
                "required": ["kb", "idx"]
            }
        },
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
const WRITE_TOOLS: [&str; 5] = [
    "kb_create",
    "kb_set_feishu",
    "kb_add_vcs",
    "kb_update_vcs",
    "kb_remove_vcs",
];

/// 分发 tools/call。工具执行错误按 MCP 约定返回 result{isError:true}。
async fn call_tool(
    state: &AppState,
    params: &Value,
    is_local: bool,
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
        "kb_get_doc" => tool_get_doc(state, &args).await,
        "kb_list_docs" => tool_list_docs(state, &args).await,
        "kb_upload" => tool_upload(state, &args).await,
        "kb_get_config" => tool_get_config(state, &args).await,
        "kb_create" => tool_create_kb(state, &args).await,
        "kb_set_feishu" => tool_set_feishu(state, &args).await,
        "kb_add_vcs" => tool_add_vcs(state, &args).await,
        "kb_update_vcs" => tool_update_vcs(state, &args).await,
        "kb_remove_vcs" => tool_remove_vcs(state, &args).await,
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
        let n = state.store.count_documents(Some(&kb.id)).unwrap_or(0);
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

async fn tool_get_doc(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("缺少整数参数 id"))?;
    let doc = state
        .store
        .get_document(id)?
        .ok_or_else(|| anyhow::anyhow!("文档 #{id} 不存在"))?;
    let kb = state
        .config
        .get_kb(&doc.kb_id)
        .ok_or_else(|| anyhow::anyhow!("文档所属知识库不存在"))?;

    let content = if let Some(md) = &doc.md_path {
        tokio::fs::read_to_string(kb.root.join(md))
            .await
            .unwrap_or_default()
    } else {
        tokio::fs::read_to_string(kb.root.join("src").join(&doc.rel_path))
            .await
            .unwrap_or_else(|_| "(该文档无可读文本,请用 REST GET /api/doc/{id}/raw 下载原件)".to_string())
    };

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
    let docs = state.store.list_documents(&kb.id, path)?;
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
    let dir = match args.get("path").and_then(|v| v.as_str()) {
        None | Some("") => String::new(),
        Some(p) => ingest::safe_rel_path(p).ok_or_else(|| anyhow::anyhow!("非法 path"))?,
    };
    let convert_md = args
        .get("convert")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let rel_path = if dir.is_empty() {
        filename.clone()
    } else {
        format!("{dir}/{filename}")
    };
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
        let fs = &kb.feishu;
        if fs.app_id.is_empty() && fs.app_secret.is_empty() && fs.folder_token.is_empty() {
            out.push_str("  飞书: 未配置\n");
        } else {
            out.push_str(&format!(
                "  飞书: app_id={} / app_secret={} / folder_token={} / 间隔 {} 分钟\n",
                if fs.app_id.is_empty() { "(空)" } else { &fs.app_id },
                if fs.app_secret.is_empty() { "(空)" } else { "已设置" },
                if fs.folder_token.is_empty() { "(空)" } else { &fs.folder_token },
                fs.sync_interval_minutes
            ));
        }
        if kb.vcs_bindings.is_empty() {
            out.push_str("  VCS 绑定: 无\n");
        } else {
            out.push_str("  VCS 绑定:\n");
            for (i, b) in kb.vcs_bindings.iter().enumerate() {
                let mut line = format!("    [{}] {} {}", i, b.vcs_type, b.url);
                if !b.repo_sub_path.is_empty() {
                    line.push_str(&format!(" 仓库子目录={}", b.repo_sub_path));
                }
                line.push_str(&format!(
                    " → src{}",
                    if b.sub_dir.is_empty() {
                        String::new()
                    } else {
                        format!("/{}", b.sub_dir)
                    }
                ));
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

async fn tool_set_feishu(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let feishu: FeishuConfig = serde_json::from_value(args.clone())?;
    state.config.set_feishu(&kb_id, feishu)?;
    Ok(format!("已更新知识库「{kb_id}」的飞书配置"))
}

async fn tool_add_vcs(state: &AppState, args: &Value) -> anyhow::Result<String> {
    let kb_id = arg_kb_id(args)?;
    let binding: VcsBinding = serde_json::from_value(args.clone())?;
    let idx = state.config.add_vcs_binding(&kb_id, binding)?;
    Ok(format!("已为知识库「{kb_id}」新增 VCS 绑定,下标 idx={idx}"))
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
    state.config.remove_vcs_binding(&kb_id, idx)?;
    Ok(format!("已删除知识库「{kb_id}」第 {idx} 条 VCS 绑定"))
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
