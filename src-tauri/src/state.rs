use std::sync::{Arc, Mutex};

use crate::config::ConfigStore;
use crate::index::SearchIndex;
use crate::store::Store;

/// 全局共享状态。同时给 Tauri(GUI 命令)和 axum(HTTP API)使用。
/// 字段均为 Arc,Clone 廉价。
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ConfigStore>,
    pub store: Arc<Store>,
    pub index: Arc<SearchIndex>,
    /// HTTP 服务实际绑定的端口(启动后由 http::serve 写入),供前端 GUI 拼接 API 地址。
    pub http_port: Arc<Mutex<Option<u16>>>,
}
