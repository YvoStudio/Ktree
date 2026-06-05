use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::{
    extract::{ConnectInfo, Multipart, Path as AxPath, Query, State as AxState},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

use md5::{Digest, Md5};

use crate::config::KnowledgeBase;
use crate::ingest::{self, in_upload_area, safe_component, safe_rel_path};
use crate::mcp;
use crate::state::AppState;

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": self.0.to_string() })),
        )
            .into_response()
    }
}

fn json_ok(v: Value) -> Response {
    Json(v).into_response()
}
fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "ok": false, "error": msg })),
    )
        .into_response()
}
fn not_found(msg: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "ok": false, "error": msg })),
    )
        .into_response()
}

/// 改配置类写操作仅允许本机(127.0.0.1 / ::1)调用 —— 拦掉局域网 / 外部写请求。
/// 注:不信任 X-Forwarded-For,只看真实 TCP 对端;Ktree 端口若被反代需另行处理。
fn require_local(addr: &SocketAddr) -> Result<(), Response> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": "改配置操作仅限本机调用" })),
        )
            .into_response())
    }
}

fn require_kb(state: &AppState, kb_id: &str) -> Result<KnowledgeBase, Response> {
    state
        .config
        .get_kb(kb_id)
        .ok_or_else(|| not_found("知识库不存在"))
}

/// 单段目录名的排序权重(同级之间比较):
/// 顶级:docs < src < .ktree;src/docs 下三区:vcs < cloud < upload;其余同权重按字母。
fn segment_order(name: &str) -> u8 {
    match name {
        "docs" => 0,
        "src" => 1,
        ".ktree" => 9,
        // 三区顺序:仓库同步 / 云文档在前,用户上传区在后
        "vcs" => 0,
        "cloud" => 1,
        "upload" => 2,
        _ => 5,
    }
}

/// 完整路径的层级排序键:每段映射成 (权重, 名字),Vec 的字典序即层级排序。
fn folder_sort_key(path: &str) -> Vec<(u8, String)> {
    path.split('/')
        .map(|seg| (segment_order(seg), seg.to_string()))
        .collect()
}

/// 路径是否落在镜像区(src|docs 下的 vcs / cloud)。镜像区的空目录是远端占位,
/// 知识库里无意义,文件树不展示;upload 区的空目录是用户工作目录,保留。
fn is_mirror_area_path(rel: &str) -> bool {
    matches!(
        rel.split('/').collect::<Vec<_>>().as_slice(),
        ["src", "vcs", ..] | ["src", "cloud", ..] | ["docs", "vcs", ..] | ["docs", "cloud", ..]
    )
}

fn docs_mirror_source_abs(kb: &KnowledgeBase, rel: &str) -> Option<PathBuf> {
    let src_rel = rel.strip_prefix("docs/")?;
    let src_abs = kb.root.join("src").join(src_rel);
    src_abs.is_file().then_some(src_abs)
}

fn file_metadata_with_docs_fallback(
    kb: &KnowledgeBase,
    rel: &str,
    path: &Path,
) -> Option<std::fs::Metadata> {
    let meta = std::fs::metadata(path).ok();
    let src_meta = docs_mirror_source_abs(kb, rel).and_then(|p| std::fs::metadata(p).ok());
    match (meta, src_meta) {
        (None, Some(sm)) => Some(sm),
        (Some(m), Some(sm)) if m.len() == 0 && sm.len() > 0 => Some(sm),
        (m, _) => m,
    }
}

/// 目录(递归)下是否有可见文件(跳过 .git/.svn/隐藏/.assets)。用于判断镜像区空目录。
fn dir_has_visible_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || name.ends_with(".assets")
            || ingest::is_ignored_component(&name)
        {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if dir_has_visible_file(&e.path()) {
                    return true;
                }
            }
            Ok(_) => return true,
            _ => {}
        }
    }
    false
}

/// 递归收集目录下所有子目录的相对路径(正斜杠),跳过隐藏目录与 .assets 伴生资源目录;
/// 镜像区(vcs/cloud)的空目录也跳过(远端占位,知识库无意义)。
fn collect_folders(base: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || name.ends_with(".assets")
                || ingest::is_ignored_component(&name)
            {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            // 镜像区空目录不展示
            if is_mirror_area_path(&rel) && !dir_has_visible_file(&path) {
                continue;
            }
            out.push(rel.clone());
            collect_folders(&path, &rel, out);
        }
    }
}

pub async fn serve(state: AppState) {
    let port_pref = state.config.snapshot().http_port;
    let candidates: Vec<u16> = if port_pref > 0 {
        vec![port_pref, 8080, 0]
    } else {
        vec![80, 8080, 0]
    };

    let mut listener: Option<TcpListener> = None;
    for p in candidates {
        match TcpListener::bind(("0.0.0.0", p)).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) => eprintln!("[ktree] 绑定端口 {p} 失败: {e}"),
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => {
            eprintln!("[ktree] HTTP 服务无法绑定任何端口,放弃启动");
            return;
        }
    };
    if let Ok(addr) = listener.local_addr() {
        println!("[ktree] HTTP API 监听于 http://0.0.0.0:{}", addr.port());
        *state.http_port.lock().unwrap() = Some(addr.port());
    }

    let app = Router::new()
        .route("/", get(root))
        .route("/favicon.svg", get(favicon))
        .route("/api", get(api_info))
        .route("/api/env", get(api_env))
        .route("/api/health", get(health))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/kbs", get(list_kbs))
        .route("/api/tree", get(tree))
        .route("/api/files", get(list_files))
        .route("/api/folder", post(create_folder).delete(delete_folder_http))
        .route("/api/upload", post(upload))
        .route("/api/search", get(search))
        .route("/api/doc/:id", get(get_doc).delete(delete_doc))
        .route("/api/doc/:id/md", get(get_doc_md))
        .route("/api/doc/:id/raw", get(get_doc_raw))
        .route("/api/notes", get(list_notes).post(add_note))
        .route("/api/notes/:id", put(update_note).delete(delete_note))
        .route("/api/kb", post(add_kb))
        .route("/api/kb/:kb_id/vcs", get(list_kb_vcs).post(add_kb_vcs))
        .route("/api/kb/:kb_id/vcs/sync", post(sync_kb_vcs_all))
        .route("/api/kb/:kb_id/vcs/:idx", put(update_kb_vcs).delete(remove_kb_vcs))
        .route("/api/kb/:kb_id/vcs/:idx/sync", post(sync_kb_vcs_one))
        .route("/api/kb/:kb_id/cloud", get(list_kb_cloud).post(add_kb_cloud))
        .route(
            "/api/kb/:kb_id/cloud/:idx",
            put(update_kb_cloud).delete(remove_kb_cloud),
        )
        .route("/api/kb/:kb_id/cloud/:idx/sync", post(sync_kb_cloud_one))
        .route("/lib/marked.min.js", get(lib_marked))
        .route("/lib/highlight.min.js", get(lib_hljs))
        .route("/lib/github.min.css", get(lib_css))
        // MCP 只支持 JSON-RPC POST;不注册 GET,axum 会自动回 405 + Allow: POST
        .route("/mcp", post(mcp::handle))
        // 知识库文件直链:/<知识库名>/<相对知识库根的路径>。放最后,优先匹配上面的固定路由。
        .route("/:kb_id/*path", get(serve_kb_file))
        .layer(CorsLayer::permissive())
        .with_state(state);

    // into_make_service_with_connect_info:让 handler 能拿到调用方 IP,
    // 用于把改配置类写操作限制为仅本机调用。
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, service).await {
        eprintln!("[ktree] HTTP 服务异常退出: {e}");
    }
}

async fn root() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("webui.html"),
    )
        .into_response()
}

async fn favicon() -> Response {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../../src/favicon.svg"),
    )
        .into_response()
}

/// 暴露给 web UI 的环境信息(自定义域名 / 当前端口),用来:
/// - 复制资源地址时拼前缀(`{custom_domain || origin}/{kb}/{rel_path}`)。
async fn api_env(AxState(state): AxState<AppState>) -> Response {
    let cfg = state.config.snapshot();
    json_ok(json!({
        "ok": true,
        "custom_domain": crate::commands::normalize_custom_domain(&cfg.custom_domain),
        "http_port": *state.http_port.lock().unwrap(),
    }))
}

async fn api_info() -> Response {
    json_ok(json!({
        "service": "Ktree 知识库",
        "endpoints": [
            "GET  /api/health",
            "GET/PUT /api/config",
            "GET  /api/kbs",
            "GET  /api/tree?kb=<id>",
            "GET  /api/files?kb=<id>&path=<dir>",
            "GET  /<知识库名>/<file path>  (静态文件直链)",
            "POST /api/folder?kb=<id>&path=src/upload/<dir>",
            "POST /api/upload?kb=<id>&path=src/upload/<dir>&convert=md",
            "GET  /api/search?kb=<id>&q=<kw>&limit=<n>",
            "GET  /api/doc/:id  /md  /raw",
            "DELETE /api/doc/:id  (仅 upload 区)",
            "POST /api/kb  (新增知识库 {name, root?})",
            "GET/POST /api/kb/:kb_id/vcs  (列出 / 新增 VCS 绑定 → src/vcs/<名>)",
            "PUT/DELETE /api/kb/:kb_id/vcs/:idx  (改 / 删 VCS 绑定)",
            "POST /api/kb/:kb_id/vcs/:idx/sync  (同步一条 VCS 绑定)",
            "GET/POST /api/kb/:kb_id/cloud  (列出 / 新增云文档绑定 → src/cloud/<提供方>/<名>)",
            "PUT/DELETE /api/kb/:kb_id/cloud/:idx  (改 / 删云文档绑定)",
            "POST /api/kb/:kb_id/cloud/:idx/sync  (同步一条云文档绑定)"
        ]
    }))
}

async fn lib_marked() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        include_str!("lib/marked.min.js"),
    )
        .into_response()
}
async fn lib_hljs() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        include_str!("lib/highlight.min.js"),
    )
        .into_response()
}
async fn lib_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("lib/github.min.css"),
    )
        .into_response()
}

async fn health(AxState(state): AxState<AppState>) -> Result<Response, ApiError> {
    let cfg = state.config.snapshot();
    let mut total = 0i64;
    for kb in &cfg.knowledge_bases {
        total += state
            .store
            .list_documents(&kb.id, None)?
            .into_iter()
            .filter(|d| !ingest::path_has_ignored_component(&d.rel_path))
            .count() as i64;
    }
    Ok(json_ok(json!({
        "ok": true,
        "documents": total,
        "knowledge_bases": cfg.knowledge_bases.len(),
    })))
}

async fn get_config(AxState(state): AxState<AppState>) -> Result<Response, ApiError> {
    Ok(json_ok(serde_json::to_value(state.config.snapshot())?))
}

async fn put_config(
    AxState(state): AxState<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(cfg): Json<crate::config::AppConfig>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    state.config.replace(cfg)?;
    Ok(json_ok(json!({ "ok": true })))
}

// ---- 细粒度改配置:新增知识库 / 配置飞书 / 管理 VCS 绑定 ----
// 受限接口,不含删除知识库、改端口 / 域名 —— 那些仍只走桌面设置窗。

#[derive(Deserialize)]
struct AddKbBody {
    name: String,
    /// 可选,知识库根目录的绝对路径;不填则放在应用数据目录下。
    #[serde(default)]
    root: Option<String>,
}

/// POST /api/kb —— 新增一个知识库。
async fn add_kb(
    AxState(state): AxState<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AddKbBody>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    let root = body
        .root
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);
    let kb = state.config.add_kb(&body.name, root)?;
    Ok(json_ok(json!({
        "ok": true,
        "kb": { "id": kb.id, "name": kb.name, "root": kb.root },
    })))
}

/// POST /api/kb/:kb_id/vcs —— 追加一条 VCS 绑定。
async fn add_kb_vcs(
    AxState(state): AxState<AppState>,
    AxPath(kb_id): AxPath<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(binding): Json<crate::config::VcsBinding>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    let idx = state.config.add_vcs_binding(&kb_id, binding)?;
    Ok(json_ok(json!({ "ok": true, "idx": idx })))
}

/// PUT /api/kb/:kb_id/vcs/:idx —— 覆盖第 idx 条 VCS 绑定。
async fn update_kb_vcs(
    AxState(state): AxState<AppState>,
    AxPath((kb_id, idx)): AxPath<(String, usize)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(binding): Json<crate::config::VcsBinding>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    state.config.update_vcs_binding(&kb_id, idx, binding)?;
    Ok(json_ok(json!({ "ok": true })))
}

/// DELETE /api/kb/:kb_id/vcs/:idx —— 删除第 idx 条 VCS 绑定,
/// 并清掉 src/vcs/<名>/ 目录、docs 产物与 store / 索引记录(镜像内容可重拉,放心删)。
async fn remove_kb_vcs(
    AxState(state): AxState<AppState>,
    AxPath((kb_id, idx)): AxPath<(String, usize)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    let kb = match require_kb(&state, &kb_id) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let removed_binding = state.config.remove_vcs_binding(&kb_id, idx)?;
    // 清理该绑定的持久化同步状态(下标会随删除变动,后续状态由下次同步重建)
    let _ = state.store.delete_sync_state("vcs", &kb_id, idx);
    let st = state.clone();
    let purged = tokio::task::spawn_blocking(move || {
        crate::vcs::purge_binding_data(&st, &kb, &removed_binding)
    })
    .await
    .map_err(|e| anyhow::anyhow!("清理任务失败: {e}"))?
    .unwrap_or(0);
    Ok(json_ok(json!({ "ok": true, "purged_docs": purged })))
}

// ---- 云文档绑定(飞书等):列出 / 新增 / 修改 / 删除 / 同步 ----

/// 列出某知识库下所有云文档绑定。**不会回显 app_secret**,保留其他字段。
async fn list_kb_cloud(
    AxState(state): AxState<AppState>,
    AxPath(kb_id): AxPath<String>,
) -> Result<Response, ApiError> {
    let Some(kb) = state.config.get_kb(&kb_id) else {
        return Ok(not_found("知识库不存在"));
    };
    let last_map = state.last_cloud_sync.lock().ok();
    let syncing = state.syncing.lock().ok();
    let arr: Vec<Value> = kb
        .cloud_bindings
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let last = last_map
                .as_ref()
                .and_then(|m| m.get(&(kb_id.clone(), i)))
                .cloned();
            let in_progress = syncing
                .as_ref()
                .map(|s| s.contains(&("cloud".to_string(), kb_id.clone(), i)))
                .unwrap_or(false);
            json!({
                "idx": i,
                "name": b.name,
                "provider": b.provider,
                "app_id": b.app_id,
                "target_type": b.target_type,
                "target_token": b.target_token,
                "has_credentials": !b.app_secret.is_empty(),
                "sync_interval_minutes": b.sync_interval_minutes,
                "last_sync": last,
                "syncing": in_progress,
            })
        })
        .collect();
    Ok(json_ok(json!({ "ok": true, "bindings": arr })))
}

/// POST /api/kb/:kb_id/cloud —— 追加一条云文档绑定。
async fn add_kb_cloud(
    AxState(state): AxState<AppState>,
    AxPath(kb_id): AxPath<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(binding): Json<crate::config::CloudBinding>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    let idx = state.config.add_cloud_binding(&kb_id, binding)?;
    Ok(json_ok(json!({ "ok": true, "idx": idx })))
}

/// PUT /api/kb/:kb_id/cloud/:idx —— 覆盖第 idx 条云文档绑定(名字不可改)。
async fn update_kb_cloud(
    AxState(state): AxState<AppState>,
    AxPath((kb_id, idx)): AxPath<(String, usize)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(binding): Json<crate::config::CloudBinding>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    state.config.update_cloud_binding(&kb_id, idx, binding)?;
    Ok(json_ok(json!({ "ok": true })))
}

/// DELETE /api/kb/:kb_id/cloud/:idx —— 删除第 idx 条云文档绑定,
/// 并清掉 src/cloud/<提供方>/<名>/ 目录、docs 产物与 store / 索引记录。
async fn remove_kb_cloud(
    AxState(state): AxState<AppState>,
    AxPath((kb_id, idx)): AxPath<(String, usize)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_local(&addr) {
        return Ok(r);
    }
    let kb = match require_kb(&state, &kb_id) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let removed_binding = state.config.remove_cloud_binding(&kb_id, idx)?;
    let _ = state.store.delete_sync_state("cloud", &kb_id, idx);
    let st = state.clone();
    let purged = tokio::task::spawn_blocking(move || {
        crate::feishu::purge_binding_data(&st, &kb, &removed_binding)
    })
    .await
    .map_err(|e| anyhow::anyhow!("清理任务失败: {e}"))?
    .unwrap_or(0);
    Ok(json_ok(json!({ "ok": true, "purged_docs": purged })))
}

/// POST /api/kb/:kb_id/cloud/:idx/sync —— 全量同步一条云文档绑定。
async fn sync_kb_cloud_one(
    AxState(state): AxState<AppState>,
    AxPath((kb_id, idx)): AxPath<(String, usize)>,
) -> Result<Response, ApiError> {
    let Some(kb) = state.config.get_kb(&kb_id) else {
        return Ok(not_found("知识库不存在"));
    };
    if idx >= kb.cloud_bindings.len() {
        return Ok(bad_request("云文档绑定 idx 越界"));
    }
    let st = state.clone();
    let report = tokio::task::spawn_blocking(move || {
        crate::feishu::sync_binding_with_record(&st, &kb, idx, "full", "manual")
    })
    .await
    .map_err(|e| anyhow::anyhow!("云文档同步任务失败: {e}"))??;
    Ok(json_ok(json!({ "ok": true, "report": report })))
}

// ---- 记事板:公共记事(所有人可看 / 可增删,不限本机)----

async fn list_notes(AxState(state): AxState<AppState>) -> Result<Response, ApiError> {
    let notes = state.store.list_notes()?;
    Ok(json_ok(json!({ "ok": true, "notes": notes })))
}

async fn add_note(
    AxState(state): AxState<AppState>,
    Json(n): Json<crate::store::NewNote>,
) -> Result<Response, ApiError> {
    if !matches!(n.note_type.as_str(), "text" | "url" | "kblink") {
        return Ok(bad_request("note_type 必须是 text / url / kblink"));
    }
    if n.title.trim().is_empty() && n.content.trim().is_empty() {
        return Ok(bad_request("记事标题和内容不能都为空"));
    }
    let note = state.store.add_note(&n)?;
    Ok(json_ok(json!({ "ok": true, "note": note })))
}

async fn update_note(
    AxState(state): AxState<AppState>,
    AxPath(id): AxPath<i64>,
    Json(n): Json<crate::store::NewNote>,
) -> Result<Response, ApiError> {
    if !matches!(n.note_type.as_str(), "text" | "url" | "kblink") {
        return Ok(bad_request("note_type 必须是 text / url / kblink"));
    }
    if !state.store.update_note(id, &n)? {
        return Ok(not_found("记事不存在"));
    }
    Ok(json_ok(json!({ "ok": true })))
}

async fn delete_note(
    AxState(state): AxState<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    state.store.delete_note(id)?;
    Ok(json_ok(json!({ "ok": true })))
}

async fn list_kbs(AxState(state): AxState<AppState>) -> Result<Response, ApiError> {
    let cfg = state.config.snapshot();
    let mut kbs = Vec::new();
    for kb in &cfg.knowledge_bases {
        let docs = state
            .store
            .list_documents(&kb.id, None)?
            .into_iter()
            .filter(|d| !ingest::path_has_ignored_component(&d.rel_path))
            .count();
        kbs.push(json!({
            "id": kb.id,
            "name": kb.name,
            "root": kb.root,
            "documents": docs,
            "vcs_bindings": kb.vcs_bindings.len(),
            "cloud_bindings": kb.cloud_bindings.len(),
        }));
    }
    Ok(json_ok(json!({ "ok": true, "knowledge_bases": kbs })))
}

#[derive(Deserialize)]
struct KbQuery {
    kb: String,
}

/// 该路径是否属于「没有任何绑定时应隐藏」的空镜像区。
/// vcs 区在没有 VCS 绑定、cloud 区在没有云文档绑定时不展示(目录可能还在盘上,但没有意义)。
fn is_hidden_empty_area(kb: &KnowledgeBase, rel: &str) -> bool {
    let area_of = |prefix: &str| {
        rel == format!("src/{prefix}")
            || rel.starts_with(&format!("src/{prefix}/"))
            || rel == format!("docs/{prefix}")
            || rel.starts_with(&format!("docs/{prefix}/"))
    };
    (kb.vcs_bindings.is_empty() && area_of("vcs"))
        || (kb.cloud_bindings.is_empty() && area_of("cloud"))
}

/// 返回某知识库 docs/src 下的全部目录(相对知识库根,正斜杠)。
/// 没有绑定的 vcs / cloud 镜像区不出现在树里;.ktree 元数据目录对用户隐藏
/// (其内容仍可经直链 /<库>/.ktree/INDEX.md 访问,供 AI 自描述用)。
async fn tree(
    AxState(state): AxState<AppState>,
    Query(q): Query<KbQuery>,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &q.kb) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let mut folders = Vec::new();
    for base in ["docs", "src"] {
        let base_abs = kb.root.join(base);
        if base_abs.is_dir() {
            folders.push(base.to_string());
            collect_folders(&base_abs, base, &mut folders);
        }
    }
    folders.retain(|f| !is_hidden_empty_area(&kb, f));
    folders.sort_by_cached_key(|f| folder_sort_key(f));
    Ok(json_ok(json!({ "ok": true, "folders": folders })))
}

#[derive(Deserialize)]
struct FilesQuery {
    kb: String,
    #[serde(default)]
    path: String,
}

/// 列某目录(相对知识库根)下的子目录与文件。src 下的文件附带 doc_id;
/// 只有 src/upload 区可写(可上传/建文件夹/删除),vcs/cloud 区只读。
/// .assets 伴生资源目录默认隐藏。
async fn list_files(
    AxState(state): AxState<AppState>,
    Query(q): Query<FilesQuery>,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &q.kb) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let rel = if q.path.trim().is_empty() {
        String::new()
    } else {
        match safe_rel_path(&q.path) {
            Some(p) => p,
            None => return Ok(bad_request("非法路径")),
        }
    };
    if ingest::path_has_ignored_component(&rel) {
        return Ok(not_found("目录不存在"));
    }
    let dir_abs = kb.root.join(&rel);
    if !dir_abs.is_dir() {
        return Ok(not_found("目录不存在"));
    }

    let in_src = rel == "src" || rel.starts_with("src/");
    // 可写性:仅 src/upload 区(含 src 根 —— 但上传/建目录的目标校验在各自接口里收口)
    let writable = rel
        .strip_prefix("src/")
        .map(in_upload_area)
        .unwrap_or(false);
    let mut folders: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir_abs) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // 隐藏文件(含 .ktree 元数据目录)与 .assets 伴生资源目录都不在列表展示;
            // .ktree 内容仍可经直链访问,只是不出现在文件浏览里。
            if name.starts_with('.')
                || name.ends_with(".assets")
                || ingest::is_ignored_component(&name)
            {
                continue;
            }
            // 没有绑定的 vcs / cloud 镜像区不展示
            let entry_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if is_hidden_empty_area(&kb, &entry_rel) {
                continue;
            }
            let p = e.path();
            // 镜像区(vcs/cloud)的空目录是远端占位,不展示(与文件树一致)
            if p.is_dir() && is_mirror_area_path(&entry_rel) && !dir_has_visible_file(&p) {
                continue;
            }
            // 跟随软链取真实文件元数据;docs 镜像断链时回落到 src 同名源文件。
            let meta = file_metadata_with_docs_fallback(&kb, &entry_rel, &p)
                .or_else(|| e.metadata().ok());
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if p.is_dir() {
                folders.push(json!({ "name": name, "modified": modified }));
            } else {
                let rel_file = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
                let ext = Path::new(&name)
                    .extension()
                    .and_then(|x| x.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let (doc_id, md_path, assets_dir) = if in_src {
                    let src_rel = rel_file.strip_prefix("src/").unwrap_or(&rel_file);
                    match state.store.get_by_path(&kb.id, src_rel).ok().flatten() {
                        Some(d) => {
                            // 伴生资源目录:docs/<父目录>/<stem>.assets/,存在才返回
                            let assets_rel = ingest::assets_rel_of(src_rel);
                            let ad = if kb.root.join("docs").join(&assets_rel).is_dir() {
                                Some(format!("docs/{assets_rel}"))
                            } else {
                                None
                            };
                            (Some(d.id), d.md_path, ad)
                        }
                        None => (None, None, None),
                    }
                } else {
                    (None, None, None)
                };
                files.push(json!({
                    "name": name, "rel_path": rel_file, "size": size,
                    "modified": modified, "ext": ext,
                    "doc_id": doc_id, "md_path": md_path, "assets_dir": assets_dir,
                }));
            }
        }
    }
    let name_of = |v: &Value| {
        v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    folders.sort_by(|a, b| {
        let (na, nb) = (name_of(a), name_of(b));
        let (oa, ob) = (segment_order(&na), segment_order(&nb));
        if oa != ob {
            oa.cmp(&ob)
        } else {
            na.cmp(&nb)
        }
    });
    files.sort_by(|a, b| name_of(a).cmp(&name_of(b)));

    Ok(json_ok(json!({
        "ok": true, "path": rel, "writable": writable,
        "folders": folders, "files": files,
    })))
}

/// 推断文件的 Content-Type;文本类型补 charset=utf-8 避免中文乱码。
fn file_content_type(path: &Path) -> String {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let essence = mime.essence_str();
    if essence.starts_with("text/")
        || essence == "application/json"
        || essence == "application/javascript"
        || essence == "application/xml"
    {
        format!("{essence}; charset=utf-8")
    } else {
        essence.to_string()
    }
}

/// 静态文件直链:GET /kb/<kb_id>/<相对知识库根的路径>
/// 供浏览器直接打开 docs/ref/.ktree 里的文件(只读)。
async fn serve_kb_file(
    AxState(state): AxState<AppState>,
    AxPath((kb_id, path)): AxPath<(String, String)>,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &kb_id) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let Some(rel) = safe_rel_path(&path) else {
        return Ok(bad_request("非法路径"));
    };
    let abs = kb.root.join(&rel);
    let (served_abs, bytes) = match tokio::fs::read(&abs).await {
        Ok(b) => (abs.clone(), b),
        Err(_) => {
            let Some(src_abs) = docs_mirror_source_abs(&kb, &rel) else {
                return Ok(not_found("文件不存在"));
            };
            match tokio::fs::read(&src_abs).await {
                Ok(b) => (src_abs, b),
                Err(_) => return Ok(not_found("文件不存在")),
            }
        }
    };
    Ok((
        [(header::CONTENT_TYPE, file_content_type(&served_abs))],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
struct FolderQuery {
    kb: String,
    path: String,
}

/// 递归删除上传区的一个文件夹(连同 docs/ 对应目录、SQLite + tantivy + manifest 联动)。
/// 仅允许 src/upload/ 下的子目录;vcs/cloud 区是同步镜像,不允许手动删。
async fn delete_folder_http(
    AxState(state): AxState<AppState>,
    Query(q): Query<FolderQuery>,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &q.kb) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let Some(rel) = safe_rel_path(&q.path) else {
        return Ok(bad_request("非法目录路径"));
    };
    if !rel.starts_with("src/") {
        return Ok(bad_request("只能删除 src 下的文件夹"));
    }
    let src_rel = rel["src/".len()..].to_string();
    if src_rel.is_empty() {
        return Ok(bad_request("不能删除 src 根目录"));
    }
    if !in_upload_area(&src_rel) || src_rel == crate::config::AREA_UPLOAD {
        return Ok(bad_request(
            "只能删除上传区(src/upload/)下的文件夹;仓库同步与云文档目录由同步维护",
        ));
    }
    let st = state.clone();
    let kb2 = kb.clone();
    let sr = src_rel.clone();
    let removed =
        tokio::task::spawn_blocking(move || ingest::delete_folder(&st, &kb2, &sr))
            .await
            .map_err(|e| anyhow::anyhow!("删除任务失败: {e}"))??;
    let st2 = state.clone();
    let kb3 = kb.clone();
    let _ = tokio::task::spawn_blocking(move || ingest::refresh_kb_meta(&st2, &kb3)).await;
    Ok(json_ok(json!({ "ok": true, "deleted_docs": removed })))
}

/// 在上传区创建文件夹。path 相对知识库根,须在 src/upload 下。
async fn create_folder(
    AxState(state): AxState<AppState>,
    Query(q): Query<FolderQuery>,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &q.kb) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let Some(rel) = safe_rel_path(&q.path) else {
        return Ok(bad_request("非法目录路径"));
    };
    let in_upload = rel
        .strip_prefix("src/")
        .map(in_upload_area)
        .unwrap_or(false);
    if !in_upload {
        return Ok(bad_request(
            "只能在上传区(src/upload/)创建文件夹;仓库同步与云文档目录由同步维护",
        ));
    }
    if rel.split('/').any(|seg| seg.ends_with(".assets")) {
        return Ok(bad_request("目录名不能以 .assets 结尾(系统保留后缀)"));
    }
    if ingest::path_has_ignored_component(&rel) {
        return Ok(bad_request(ingest::ignore_rule_description()));
    }
    tokio::fs::create_dir_all(kb.root.join(&rel)).await?;
    Ok(json_ok(json!({ "ok": true, "path": rel })))
}

#[derive(Deserialize)]
struct UploadQuery {
    kb: String,
    /// 上传目标目录,相对知识库根,须在 src/upload 下;空 = src/upload 根
    #[serde(default)]
    path: String,
    convert: Option<String>,
}

/// 上传文件到某知识库上传区(src/upload/<dir>/)。vcs/cloud 区只读,拒绝上传。
async fn upload(
    AxState(state): AxState<AppState>,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &q.kb) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let convert_md = q.convert.as_deref() == Some("md");

    // path 相对知识库根,解析出相对 src 的目标目录(必须在 upload 区内)
    let src_dir = {
        let p = q.path.trim();
        if p.is_empty() {
            crate::config::AREA_UPLOAD.to_string()
        } else {
            match safe_rel_path(p) {
                Some(rp) if rp == "src" => crate::config::AREA_UPLOAD.to_string(),
                Some(rp) if rp.starts_with("src/") => {
                    let rest = rp["src/".len()..].to_string();
                    if !in_upload_area(&rest) {
                        return Ok(bad_request(
                            "只能上传到上传区(src/upload/);仓库同步与云文档目录由同步维护",
                        ));
                    }
                    rest
                }
                _ => return Ok(bad_request("只能上传到 src/upload 目录下")),
            }
        }
    };

    let mut docs = Vec::new();
    let mut errors = Vec::new();
    while let Some(field) = multipart.next_field().await? {
        let Some(raw_name) = field.file_name().map(|s| s.to_string()) else {
            continue;
        };
        let Some(filename) = safe_component(&raw_name) else {
            errors.push(json!({ "file": raw_name, "error": "非法文件名" }));
            continue;
        };
        if filename.ends_with(".assets") {
            errors.push(json!({ "file": raw_name, "error": "文件名不能以 .assets 结尾(系统保留后缀)" }));
            continue;
        }
        let data = field.bytes().await?;

        let rel_path = format!("{src_dir}/{filename}");
        if ingest::path_has_ignored_component(&rel_path) {
            errors.push(json!({ "file": raw_name, "error": ingest::ignore_rule_description() }));
            continue;
        }
        let abs = kb.root.join("src").join(&rel_path);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // 已存在且 md5 完全一致 → 跳过写盘,保留原 mtime。
        // ingest_file 内还有一层 md5 短路,会直接返回旧 doc。
        let unchanged = match tokio::fs::read(&abs).await {
            Ok(old) => {
                let new_hash = format!("{:x}", Md5::digest(&data));
                let old_hash = format!("{:x}", Md5::digest(&old));
                new_hash == old_hash
            }
            Err(_) => false,
        };
        if !unchanged {
            tokio::fs::write(&abs, &data).await?;
        }

        let st = state.clone();
        let kb2 = kb.clone();
        let rp = rel_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            ingest::ingest_file(&st, &kb2, &rp, "upload", convert_md, false)
        })
        .await
        .map_err(|e| anyhow::anyhow!("入库任务失败: {e}"))?;

        match result {
            Ok(doc) => docs.push(doc),
            Err(e) => errors.push(json!({ "file": rel_path, "error": e.to_string() })),
        }
    }

    let st = state.clone();
    let kb2 = kb.clone();
    let _ = tokio::task::spawn_blocking(move || ingest::refresh_kb_meta(&st, &kb2)).await;

    Ok(json_ok(json!({
        "ok": errors.is_empty(),
        "documents": docs,
        "errors": errors,
    })))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    kb: String,
    q: String,
    limit: Option<usize>,
}

async fn search(
    AxState(state): AxState<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let limit = q.limit.unwrap_or(20).min(100);
    let st = state.clone();
    let query = q.q.clone();
    let kb = if q.kb.is_empty() {
        None
    } else {
        Some(q.kb.clone())
    };
    // 指定了知识库就先校验存在 —— 不存在直接报错,而不是返回空结果。
    if let Some(ref kb_id) = kb {
        if state.config.get_kb(kb_id).is_none() {
            return Ok(not_found("知识库不存在"));
        }
    }
    let hits = tokio::task::spawn_blocking(move || {
        crate::search::hybrid(&st, kb.as_deref(), &query, limit)
    })
    .await
    .map_err(|e| anyhow::anyhow!("搜索任务失败: {e}"))??;
    // 为每条 hit 补上 store 里的 ext / rel_path / md_path / kb_id,
    // 前端就能按真实类型走 preview(html → iframe, md → markdown, 图片 → 图片浏览…)。
    let store = state.store.clone();
    let enriched: Vec<Value> = hits
        .into_iter()
        .filter_map(|h| {
            let doc = store.get_document(h.doc_id).ok().flatten()?;
            if ingest::path_has_ignored_component(&doc.rel_path) {
                return None;
            }
            Some(json!({
                "doc_id": h.doc_id,
                "title": h.title,
                "category": h.category,
                "summary": h.summary,
                "score": h.score,
                "kb_id": doc.kb_id,
                "rel_path": doc.rel_path,
                "md_path": doc.md_path,
                "ext": doc.ext,
            }))
        })
        .collect();
    Ok(json_ok(json!({ "ok": true, "query": q.q, "hits": enriched })))
}

async fn get_doc(
    AxState(state): AxState<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    match state.store.get_document(id)? {
        Some(doc) if !ingest::path_has_ignored_component(&doc.rel_path) => {
            Ok(json_ok(json!({ "ok": true, "document": doc })))
        }
        Some(_) => Ok(not_found("文档不存在")),
        None => Ok(not_found("文档不存在")),
    }
}

async fn get_doc_md(
    AxState(state): AxState<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    let Some(doc) = state.store.get_document(id)? else {
        return Ok(not_found("文档不存在"));
    };
    if ingest::path_has_ignored_component(&doc.rel_path) {
        return Ok(not_found("文档不存在"));
    }
    let Some(kb) = state.config.get_kb(&doc.kb_id) else {
        return Ok(not_found("文档所属知识库不存在"));
    };
    let text = match ingest::read_doc_markdown(&kb, &doc) {
        Ok(text) => text,
        Err(e) => return Ok(not_found(&format!("Markdown 读取失败: {e}"))),
    };
    Ok((
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        text,
    )
        .into_response())
}

async fn get_doc_raw(
    AxState(state): AxState<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    let Some(doc) = state.store.get_document(id)? else {
        return Ok(not_found("文档不存在"));
    };
    if ingest::path_has_ignored_component(&doc.rel_path) {
        return Ok(not_found("文档不存在"));
    }
    let Some(kb) = state.config.get_kb(&doc.kb_id) else {
        return Ok(not_found("文档所属知识库不存在"));
    };
    let abs = kb.root.join("src").join(&doc.rel_path);
    let bytes = tokio::fs::read(&abs).await?;
    Ok((
        [(header::CONTENT_TYPE, file_content_type(&abs))],
        bytes,
    )
        .into_response())
}

async fn delete_doc(
    AxState(state): AxState<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    let Some(doc) = state.store.get_document(id)? else {
        return Ok(not_found("文档不存在"));
    };
    if ingest::path_has_ignored_component(&doc.rel_path) {
        return Ok(not_found("文档不存在"));
    }
    let Some(kb) = state.config.get_kb(&doc.kb_id) else {
        return Ok(not_found("文档所属知识库不存在"));
    };
    // vcs/cloud 区是同步镜像:本地删了下次同步又会回来,且会造成镜像不一致 → 拒绝
    if !in_upload_area(&doc.rel_path) {
        return Ok(bad_request(
            "该文档由仓库/云文档同步维护,不能单独删除;请在来源端删除后同步,或删除整个绑定",
        ));
    }
    let st = state.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        ingest::delete_doc(&st, &kb, &doc)?;
        ingest::refresh_kb_meta(&st, &kb)
    })
    .await
    .map_err(|e| anyhow::anyhow!("删除任务失败: {e}"))??;
    Ok(json_ok(json!({ "ok": true, "deleted": id })))
}

/// 列出某知识库下所有 VCS 绑定。**不会回显 password**(避免泄露),保留其他字段。
async fn list_kb_vcs(
    AxState(state): AxState<AppState>,
    AxPath(kb_id): AxPath<String>,
) -> Result<Response, ApiError> {
    let Some(kb) = state.config.get_kb(&kb_id) else {
        return Ok(not_found("知识库不存在"));
    };
    let last_map = state.last_vcs_sync.lock().ok();
    let syncing = state.syncing.lock().ok();
    let arr: Vec<Value> = kb
        .vcs_bindings
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let last = last_map
                .as_ref()
                .and_then(|m| m.get(&(kb_id.clone(), i)))
                .cloned();
            let in_progress = syncing
                .as_ref()
                .map(|s| s.contains(&("vcs".to_string(), kb_id.clone(), i)))
                .unwrap_or(false);
            json!({
                "idx": i,
                "name": b.name,
                "vcs_type": b.vcs_type,
                "url": b.url,
                "repo_sub_path": b.repo_sub_path,
                "branch": b.branch,
                "has_credentials": !b.username.is_empty() || !b.password.is_empty(),
                "sync_interval_minutes": b.sync_interval_minutes,
                "last_sync": last,
                "syncing": in_progress,
            })
        })
        .collect();
    Ok(json_ok(json!({ "ok": true, "bindings": arr })))
}

/// 同步一个 KB 下指定 idx 的 VCS 绑定。返回该绑定的 SyncReport。
async fn sync_kb_vcs_one(
    AxState(state): AxState<AppState>,
    AxPath((kb_id, idx)): AxPath<(String, usize)>,
) -> Result<Response, ApiError> {
    let Some(kb) = state.config.get_kb(&kb_id) else {
        return Ok(not_found("知识库不存在"));
    };
    if idx >= kb.vcs_bindings.len() {
        return Ok(bad_request("VCS 绑定 idx 越界"));
    }
    let st = state.clone();
    let report = tokio::task::spawn_blocking(move || {
        crate::vcs::sync_binding_with_record(&st, &kb, idx, "manual")
    })
    .await
    .map_err(|e| anyhow::anyhow!("VCS 同步任务失败: {e}"))??;
    Ok(json_ok(json!({ "ok": true, "report": report })))
}

/// 同步一个 KB 下所有 VCS 绑定。逐个执行,失败的塞 errors,不阻塞其它。
async fn sync_kb_vcs_all(
    AxState(state): AxState<AppState>,
    AxPath(kb_id): AxPath<String>,
) -> Result<Response, ApiError> {
    let Some(kb) = state.config.get_kb(&kb_id) else {
        return Ok(not_found("知识库不存在"));
    };
    if kb.vcs_bindings.is_empty() {
        return Ok(json_ok(json!({ "ok": true, "reports": [], "errors": [] })));
    }
    let st = state.clone();
    let outcomes = tokio::task::spawn_blocking(move || {
        crate::vcs::sync_kb_all_with_record(&st, &kb, "manual")
    })
    .await
    .map_err(|e| anyhow::anyhow!("VCS 同步任务失败: {e}"))?;
    let mut reports = Vec::new();
    let mut errors = Vec::new();
    for (i, r) in outcomes.into_iter().enumerate() {
        match r {
            Ok(rep) => reports.push(rep),
            Err(e) => errors.push(json!({ "idx": i, "error": e.to_string() })),
        }
    }
    Ok(json_ok(json!({ "ok": true, "reports": reports, "errors": errors })))
}
