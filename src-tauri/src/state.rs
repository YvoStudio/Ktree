use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::config::ConfigStore;
use crate::embed::Embedder;
use crate::index::SearchIndex;
use crate::store::Store;

/// 最近一次某绑定(VCS / 云文档)的同步快照。给 webui 在「知识库操作」里展示,
/// 自动 + 手动同步共用。同步完成后会持久化到 SQLite(sync_state 表),重启不丢。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastSync {
    /// Unix 毫秒时间戳。前端自己根据当前时间换算成"X 分钟前"。
    pub at_unix_ms: u64,
    /// "auto"(scheduler 定时)或 "manual"(用户点击/REST 调用)。
    pub source: String,
    pub ok: bool,
    /// VCS:HEAD sha / svn 修订号;云文档绑定恒为空串。
    pub revision: String,
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub failed: usize,
    /// ok=false 时填错误摘要;ok=true 但 ingest 中有失败项时,这里仍是 None。
    pub error: Option<String>,
}

/// 兼容旧名(vcs.rs 等处仍按 LastVcsSync 引用)。
pub type LastVcsSync = LastSync;

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
    pub last_vcs_sync: Arc<Mutex<HashMap<(String, usize), LastSync>>>,
    /// 最近一次云文档同步状态:key=(kb_id, binding_idx)。进程内存,重启丢失。
    pub last_cloud_sync: Arc<Mutex<HashMap<(String, usize), LastSync>>>,
    /// 正在同步中的绑定:key=(kind "vcs"|"cloud", kb_id, binding_idx)。
    /// 防止同一绑定并发同步(并发的 git/svn 进程会互相打架、留锁)。
    pub syncing: Arc<Mutex<HashSet<(String, String, usize)>>>,
}

impl AppState {
    /// 记录一次同步结果:更新内存 map + 持久化到 SQLite,供 webui 展示(重启不丢)。
    pub fn record_sync(&self, kind: &str, kb_id: &str, idx: usize, entry: LastSync) {
        let map = if kind == "cloud" {
            &self.last_cloud_sync
        } else {
            &self.last_vcs_sync
        };
        if let Ok(mut m) = map.lock() {
            m.insert((kb_id.to_string(), idx), entry.clone());
        }
        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = self.store.save_sync_state(kind, kb_id, idx, &json);
        }
    }

    /// 启动时从 SQLite 恢复历次同步状态到内存 map。
    pub fn restore_sync_states(&self) {
        for (kind, map) in [("vcs", &self.last_vcs_sync), ("cloud", &self.last_cloud_sync)] {
            let Ok(rows) = self.store.load_sync_states(kind) else {
                continue;
            };
            if let Ok(mut m) = map.lock() {
                for (kb_id, idx, json) in rows {
                    if let Ok(entry) = serde_json::from_str::<LastSync>(&json) {
                        m.insert((kb_id, idx), entry);
                    }
                }
            }
        }
    }

    /// 尝试标记某绑定开始同步。已在同步中返回 false。
    pub fn try_begin_sync(&self, kind: &str, kb_id: &str, idx: usize) -> bool {
        self.syncing
            .lock()
            .map(|mut s| s.insert((kind.to_string(), kb_id.to_string(), idx)))
            .unwrap_or(false)
    }

    /// 标记某绑定同步结束。
    pub fn end_sync(&self, kind: &str, kb_id: &str, idx: usize) {
        if let Ok(mut s) = self.syncing.lock() {
            s.remove(&(kind.to_string(), kb_id.to_string(), idx));
        }
    }
}
