use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::config::ConfigStore;
use crate::embed::Embedder;
use crate::index::SearchIndex;
use crate::store::Store;

/// 最近一次某 VCS 绑定的同步快照。给 webui 在「知识库操作」里展示,自动 + 手动同步共用。
#[derive(Clone, Debug, Serialize)]
pub struct LastVcsSync {
    /// Unix 毫秒时间戳。前端自己根据当前时间换算成"X 分钟前"。
    pub at_unix_ms: u64,
    /// "auto"(scheduler 定时)或 "manual"(用户点击/REST 调用)。
    pub source: String,
    pub ok: bool,
    pub revision: String,
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub failed: usize,
    /// ok=false 时填错误摘要;ok=true 但 ingest 中有失败项时,这里仍是 None。
    pub error: Option<String>,
}

/// 全局共享状态。同时给 Tauri(GUI 命令)和 axum(HTTP API)使用。
/// 字段均为 Arc,Clone 廉价。
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ConfigStore>,
    pub store: Arc<Store>,
    pub index: Arc<SearchIndex>,
    /// 语义向量嵌入客户端(常驻 embed sidecar)。
    pub embedder: Arc<Embedder>,
    /// HTTP 服务实际绑定的端口(启动后由 http::serve 写入),供前端 GUI 拼接 API 地址。
    pub http_port: Arc<Mutex<Option<u16>>>,
    /// 最近一次 VCS 同步状态:key=(kb_id, binding_idx)。进程内存,重启丢失。
    pub last_vcs_sync: Arc<Mutex<HashMap<(String, usize), LastVcsSync>>>,
}
