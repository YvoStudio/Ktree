use std::path::Path;

use axum::{
    extract::{Multipart, Path as AxPath, Query, State as AxState},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

use md5::{Digest, Md5};

use crate::config::KnowledgeBase;
use crate::ingest::{self, safe_component, safe_rel_path};
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

fn require_kb(state: &AppState, kb_id: &str) -> Result<KnowledgeBase, Response> {
    state
        .config
        .get_kb(kb_id)
        .ok_or_else(|| not_found("知识库不存在"))
}

/// 文件夹顶级名称排序权重:docs 最上,然后 src / ref / .ktree,其它按字母。
fn folder_order(s: &str) -> u8 {
    match s.split('/').next().unwrap_or("") {
        "docs" => 0,
        "src" => 1,
        "ref" => 2,
        ".ktree" => 3,
        _ => 9,
    }
}

/// 递归收集目录下所有子目录的相对路径(正斜杠),跳过隐藏目录。
fn collect_folders(base: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
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
        .route("/api/sync/feishu/trigger", post(feishu_trigger))
        .route("/api/kb/:kb_id/vcs", get(list_kb_vcs))
        .route("/api/kb/:kb_id/vcs/sync", post(sync_kb_vcs_all))
        .route("/api/kb/:kb_id/vcs/:idx/sync", post(sync_kb_vcs_one))
        .route("/lib/marked.min.js", get(lib_marked))
        .route("/lib/highlight.min.js", get(lib_hljs))
        .route("/lib/github.min.css", get(lib_css))
        .route("/mcp", post(mcp::handle))
        .route("/mcp", get(mcp::handle_get))
        // 知识库文件直链:/<知识库名>/<相对知识库根的路径>。放最后,优先匹配上面的固定路由。
        .route("/:kb_id/*path", get(serve_kb_file))
        .layer(CorsLayer::permissive())
        .with_state(state);

    if let Err(e) = axum::serve(listener, app).await {
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
            "POST /api/folder?kb=<id>&path=src/<dir>",
            "POST /api/upload?kb=<id>&path=src/<dir>&convert=md",
            "GET  /api/search?kb=<id>&q=<kw>&limit=<n>",
            "GET  /api/doc/:id  /md  /raw",
            "DELETE /api/doc/:id",
            "POST /api/sync/feishu/trigger?kb=<id>"
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
    let total = state.store.count_documents(None)?;
    let cfg = state.config.snapshot();
    Ok(json_ok(json!({
        "ok": true,
        "documents": total,
        "knowledge_bases": cfg.knowledge_bases.len(),
        "sync_interval_minutes": cfg.sync_interval_minutes,
    })))
}

async fn get_config(AxState(state): AxState<AppState>) -> Result<Response, ApiError> {
    Ok(json_ok(serde_json::to_value(state.config.snapshot())?))
}

async fn put_config(
    AxState(state): AxState<AppState>,
    Json(cfg): Json<crate::config::AppConfig>,
) -> Result<Response, ApiError> {
    state.config.replace(cfg)?;
    Ok(json_ok(json!({ "ok": true })))
}

async fn list_kbs(AxState(state): AxState<AppState>) -> Result<Response, ApiError> {
    let cfg = state.config.snapshot();
    let mut kbs = Vec::new();
    for kb in &cfg.knowledge_bases {
        let docs = state.store.count_documents(Some(&kb.id))?;
        kbs.push(json!({
            "id": kb.id,
            "name": kb.name,
            "root": kb.root,
            "documents": docs,
            "feishu_configured": kb.feishu.is_complete(),
        }));
    }
    Ok(json_ok(json!({ "ok": true, "knowledge_bases": kbs })))
}

#[derive(Deserialize)]
struct KbQuery {
    kb: String,
}

/// 返回某知识库 src/docs/ref 三个区下的全部目录(相对知识库根,正斜杠)。
async fn tree(
    AxState(state): AxState<AppState>,
    Query(q): Query<KbQuery>,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &q.kb) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let mut folders = Vec::new();
    for base in ["docs", "src", "ref", ".ktree"] {
        let base_abs = kb.root.join(base);
        if base_abs.is_dir() {
            folders.push(base.to_string());
            collect_folders(&base_abs, base, &mut folders);
        }
    }
    folders.sort_by(|a, b| {
        let (oa, ob) = (folder_order(a), folder_order(b));
        if oa != ob {
            oa.cmp(&ob)
        } else {
            a.cmp(b)
        }
    });
    Ok(json_ok(json!({ "ok": true, "folders": folders })))
}

#[derive(Deserialize)]
struct FilesQuery {
    kb: String,
    #[serde(default)]
    path: String,
}

/// 列某目录(相对知识库根)下的子目录与文件。src 下的文件附带 doc_id;
/// 只有 src 区可写(可上传/建文件夹)。
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
    let dir_abs = kb.root.join(&rel);
    if !dir_abs.is_dir() {
        return Ok(not_found("目录不存在"));
    }

    let in_src = rel == "src" || rel.starts_with("src/");
    let mut folders: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir_abs) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // .ktree 是知识库元数据目录,允许浏览;其它隐藏文件跳过
            if name.starts_with('.') && name != ".ktree" {
                continue;
            }
            let p = e.path();
            // 跟随软链取真实文件元数据(docs/ 下大量软链指向 src/ 原文件)。
            // 软链断裂时回落到软链自身的元数据,避免整目录崩。
            let meta = std::fs::metadata(&p).or_else(|_| e.metadata()).ok();
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
                let (doc_id, md_path, ref_dir) = if in_src {
                    let src_rel = rel_file.strip_prefix("src/").unwrap_or(&rel_file);
                    match state.store.get_by_path(&kb.id, src_rel).ok().flatten() {
                        Some(d) => {
                            // ref 资源目录:ref/<src 相对路径去扩展名>/,存在才返回
                            let sp = Path::new(src_rel);
                            let stem =
                                sp.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                            let ref_rel = match sp
                                .parent()
                                .filter(|x| !x.as_os_str().is_empty())
                            {
                                Some(dir) => format!(
                                    "{}/{}",
                                    dir.to_string_lossy().replace('\\', "/"),
                                    stem
                                ),
                                None => stem.to_string(),
                            };
                            let rd = if kb.root.join("ref").join(&ref_rel).is_dir() {
                                Some(format!("ref/{ref_rel}"))
                            } else {
                                None
                            };
                            (Some(d.id), d.md_path, rd)
                        }
                        None => (None, None, None),
                    }
                } else {
                    (None, None, None)
                };
                files.push(json!({
                    "name": name, "rel_path": rel_file, "size": size,
                    "modified": modified, "ext": ext,
                    "doc_id": doc_id, "md_path": md_path, "ref_dir": ref_dir,
                }));
            }
        }
    }
    let name_of = |v: &Value| {
        v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    folders.sort_by(|a, b| {
        let (na, nb) = (name_of(a), name_of(b));
        let (oa, ob) = (folder_order(&na), folder_order(&nb));
        if oa != ob {
            oa.cmp(&ob)
        } else {
            na.cmp(&nb)
        }
    });
    files.sort_by(|a, b| name_of(a).cmp(&name_of(b)));

    Ok(json_ok(json!({
        "ok": true, "path": rel, "writable": in_src,
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
    let bytes = match tokio::fs::read(&abs).await {
        Ok(b) => b,
        Err(_) => return Ok(not_found("文件不存在")),
    };
    Ok((
        [(header::CONTENT_TYPE, file_content_type(&abs))],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
struct FolderQuery {
    kb: String,
    path: String,
}

/// 递归删除 src/ 下的一个文件夹(连同 docs/ ref/ 对应目录、SQLite + tantivy + manifest 联动)
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

/// 在 src/ 下创建文件夹(仅 src 区可写)。path 相对知识库根,须以 src 开头。
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
    if rel != "src" && !rel.starts_with("src/") {
        return Ok(bad_request("只能在 src 目录下创建文件夹"));
    }
    tokio::fs::create_dir_all(kb.root.join(&rel)).await?;
    Ok(json_ok(json!({ "ok": true, "path": rel })))
}

#[derive(Deserialize)]
struct UploadQuery {
    kb: String,
    /// 上传目标目录,相对知识库根,须为 src 或 src/ 下
    #[serde(default)]
    path: String,
    convert: Option<String>,
}

/// 上传文件到某知识库 src/<dir>/ 下。
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

    // path 相对知识库根,解析出相对 src 的子目录
    let src_dir = {
        let p = q.path.trim();
        if p.is_empty() || p == "src" {
            String::new()
        } else {
            match safe_rel_path(p) {
                Some(rp) if rp == "src" => String::new(),
                Some(rp) if rp.starts_with("src/") => rp["src/".len()..].to_string(),
                _ => return Ok(bad_request("只能上传到 src 目录下")),
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
        let data = field.bytes().await?;

        let rel_path = if src_dir.is_empty() {
            filename.clone()
        } else {
            format!("{src_dir}/{filename}")
        };
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
    let index = state.index.clone();
    let query = q.q.clone();
    let kb = if q.kb.is_empty() {
        None
    } else {
        Some(q.kb.clone())
    };
    let hits = tokio::task::spawn_blocking(move || index.search(kb.as_deref(), &query, limit))
        .await
        .map_err(|e| anyhow::anyhow!("搜索任务失败: {e}"))??;
    // 为每条 hit 补上 store 里的 ext / rel_path / md_path / kb_id,
    // 前端就能按真实类型走 preview(html → iframe, md → markdown, 图片 → 图片浏览…)。
    let store = state.store.clone();
    let enriched: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            let doc = store.get_document(h.doc_id).ok().flatten();
            json!({
                "doc_id": h.doc_id,
                "title": h.title,
                "category": h.category,
                "summary": h.summary,
                "score": h.score,
                "kb_id": doc.as_ref().map(|d| d.kb_id.clone()),
                "rel_path": doc.as_ref().map(|d| d.rel_path.clone()),
                "md_path": doc.as_ref().and_then(|d| d.md_path.clone()),
                "ext": doc.as_ref().map(|d| d.ext.clone()),
            })
        })
        .collect();
    Ok(json_ok(json!({ "ok": true, "query": q.q, "hits": enriched })))
}

async fn get_doc(
    AxState(state): AxState<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    match state.store.get_document(id)? {
        Some(doc) => Ok(json_ok(json!({ "ok": true, "document": doc }))),
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
    let Some(md_rel) = doc.md_path else {
        return Ok(not_found("该文档没有 Markdown 转换结果"));
    };
    let Some(kb) = state.config.get_kb(&doc.kb_id) else {
        return Ok(not_found("文档所属知识库不存在"));
    };
    // 用字节读 + lossy,避免非 UTF-8(GBK 等)源文件直接报 5xx。
    let bytes = tokio::fs::read(kb.root.join(md_rel)).await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
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
    let Some(kb) = state.config.get_kb(&doc.kb_id) else {
        return Ok(not_found("文档所属知识库不存在"));
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        ingest::delete_doc(&st, &kb, &doc)?;
        ingest::refresh_kb_meta(&st, &kb)
    })
    .await
    .map_err(|e| anyhow::anyhow!("删除任务失败: {e}"))??;
    Ok(json_ok(json!({ "ok": true, "deleted": id })))
}

async fn feishu_trigger(
    AxState(state): AxState<AppState>,
    Query(q): Query<KbQuery>,
) -> Result<Response, ApiError> {
    let kb = match require_kb(&state, &q.kb) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    if !kb.feishu.is_complete() {
        return Ok(bad_request("该知识库未配置飞书凭证"));
    }
    let st = state.clone();
    let report = tokio::task::spawn_blocking(move || crate::feishu::sync(&st, &kb, "full"))
        .await
        .map_err(|e| anyhow::anyhow!("飞书同步任务失败: {e}"))??;
    Ok(json_ok(json!({ "ok": true, "report": report })))
}

/// 列出某知识库下所有 VCS 绑定。**不会回显 password**(避免泄露),保留其他字段。
async fn list_kb_vcs(
    AxState(state): AxState<AppState>,
    AxPath(kb_id): AxPath<String>,
) -> Result<Response, ApiError> {
    let Some(kb) = state.config.get_kb(&kb_id) else {
        return Ok(not_found("知识库不存在"));
    };
    let arr: Vec<Value> = kb
        .vcs_bindings
        .iter()
        .enumerate()
        .map(|(i, b)| {
            json!({
                "idx": i,
                "vcs_type": b.vcs_type,
                "url": b.url,
                "sub_dir": b.sub_dir,
                "branch": b.branch,
                "has_credentials": !b.username.is_empty() || !b.password.is_empty(),
                "sync_interval_minutes": b.sync_interval_minutes,
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
    let report = tokio::task::spawn_blocking(move || crate::vcs::sync_binding(&st, &kb, idx))
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
    let outcomes = tokio::task::spawn_blocking(move || crate::vcs::sync_kb_all(&st, &kb))
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
