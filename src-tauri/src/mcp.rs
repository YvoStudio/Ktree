// MCP Server over Streamable HTTP transport
//
// 挂在 axum 的 POST /mcp 上,讲 JSON-RPC 2.0。客户端(Claude Code / Desktop)
// 可直接把 http://<host>:<port>/mcp 配成 HTTP MCP server。
//
// 工具:kb_list / kb_search / kb_get_doc / kb_list_docs / kb_upload

use axum::{
    extract::State as AxState,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

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
pub async fn handle(AxState(state): AxState<AppState>, Json(body): Json<Value>) -> Response {
    match body {
        Value::Array(items) => {
            let mut responses = Vec::new();
            for item in items {
                if let Some(resp) = handle_one(&state, item).await {
                    responses.push(resp);
                }
            }
            if responses.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(responses)).into_response()
            }
        }
        obj => match handle_one(&state, obj).await {
            Some(resp) => Json(resp).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// 处理单条 JSON-RPC 消息。notification(无 id)返回 None。
async fn handle_one(state: &AppState, msg: Value) -> Option<Value> {
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
        "tools/call" => call_tool(state, &params).await,
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
            "description": "在知识库中做中文全文检索(BM25)。返回匹配文档的标题、摘要、所属知识库。",
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
        }
    ])
}

/// 分发 tools/call。工具执行错误按 MCP 约定返回 result{isError:true}。
async fn call_tool(state: &AppState, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let outcome = match name {
        "kb_list" => tool_list(state).await,
        "kb_search" => tool_search(state, &args).await,
        "kb_get_doc" => tool_get_doc(state, &args).await,
        "kb_list_docs" => tool_list_docs(state, &args).await,
        "kb_upload" => tool_upload(state, &args).await,
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
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .clamp(1, 50) as usize;

    let index = state.index.clone();
    let hits = tokio::task::spawn_blocking(move || index.search(kb.as_deref(), &query, limit))
        .await??;

    if hits.is_empty() {
        return Ok("未找到匹配文档".to_string());
    }
    let mut out = format!("找到 {} 条结果:\n\n", hits.len());
    for h in &hits {
        out.push_str(&format!(
            "### #{} {}\n- 知识库: {}\n- 分类: {}\n- 相关度: {:.2}\n- 摘要: {}\n- 取全文: kb_get_doc(id={})\n\n",
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
