use serde::Serialize;
use tauri::State;

use crate::state::AppState;

/// 前端 GUI 启动时拉取的服务概况:用 http_port 拼接本机 API 地址(桌面壳跳转用)。
#[derive(Serialize)]
pub struct ServiceInfo {
    pub http_port: Option<u16>,
    pub knowledge_bases: usize,
    pub documents: i64,
    /// 用户在「全局 → 自定义域名」里填的值(已规范化)。空 = 未启用。
    pub custom_domain: String,
}

#[tauri::command]
pub fn get_service_info(state: State<'_, AppState>) -> ServiceInfo {
    let cfg = state.config.snapshot();
    ServiceInfo {
        http_port: *state.http_port.lock().unwrap(),
        knowledge_bases: cfg.knowledge_bases.len(),
        documents: state.store.count_documents(None).unwrap_or(0),
        custom_domain: normalize_custom_domain(&cfg.custom_domain),
    }
}

/// 规范化自定义域名:补 scheme,去掉末尾 `/`。空串原样返回。
pub fn normalize_custom_domain(raw: &str) -> String {
    let t = raw.trim().trim_end_matches('/').to_string();
    if t.is_empty() {
        return String::new();
    }
    if t.starts_with("http://") || t.starts_with("https://") {
        t
    } else {
        format!("http://{t}")
    }
}

/// 设置界面触发某知识库的飞书全量同步。
#[tauri::command]
pub async fn trigger_feishu_sync(
    state: State<'_, AppState>,
    kb_id: String,
) -> Result<serde_json::Value, String> {
    let st = state.inner().clone();
    let kb = st
        .config
        .get_kb(&kb_id)
        .ok_or_else(|| "知识库不存在".to_string())?;
    let report = tokio::task::spawn_blocking(move || crate::feishu::sync(&st, &kb, "full"))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    serde_json::to_value(report).map_err(|e| e.to_string())
}

/// 取本机局域网 IPv4(供设置界面拼接 web 访问地址)。
#[tauri::command]
pub fn get_local_ip() -> String {
    use std::net::UdpSocket;
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}
