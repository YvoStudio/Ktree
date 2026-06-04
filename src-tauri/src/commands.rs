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

/// 删除一条绑定(VCS / 云文档)并清理本地镜像数据,返回清理的文档数。
/// 设置窗口(Tauri WebView)走这个 invoke 通道,不绕 HTTP —— 避免本机 127.0.0.1
/// 被其它进程占用(如安卓模拟器)时删除失败。`kind`:"vcs" | "cloud"。
#[tauri::command]
pub async fn delete_binding(
    state: State<'_, AppState>,
    kind: String,
    kb_id: String,
    idx: usize,
) -> Result<u64, String> {
    let st = state.inner().clone();
    let kb = st
        .config
        .get_kb(&kb_id)
        .ok_or_else(|| "知识库不存在".to_string())?;
    let purged = if kind == "cloud" {
        let removed = st
            .config
            .remove_cloud_binding(&kb_id, idx)
            .map_err(|e| e.to_string())?;
        let _ = st.store.delete_sync_state("cloud", &kb_id, idx);
        tokio::task::spawn_blocking(move || crate::feishu::purge_binding_data(&st, &kb, &removed))
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or(0)
    } else {
        let removed = st
            .config
            .remove_vcs_binding(&kb_id, idx)
            .map_err(|e| e.to_string())?;
        let _ = st.store.delete_sync_state("vcs", &kb_id, idx);
        tokio::task::spawn_blocking(move || crate::vcs::purge_binding_data(&st, &kb, &removed))
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or(0)
    };
    Ok(purged as u64)
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
