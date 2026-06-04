use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::feishu;
use crate::state::AppState;
use crate::vcs;

/// 后台定时同步循环。每分钟 tick 一次,从 config snapshot 重新读最新设置 ——
/// 改 config 不用重启应用就能生效。
///
/// 触发条件(均为逐条绑定):
/// - 云文档绑定:`sync_interval_minutes > 0` 且凭证完整,且距上次同步 ≥ 该间隔。
/// - VCS 绑定:`sync_interval_minutes > 0`,且距上次同步 ≥ 该间隔。
///
/// 上次同步时间只活在进程内存,重启进程会从零计时,首轮在 interval 到期后才跑。
pub async fn run(state: AppState) {
    println!("[ktree] 后台调度器启动(60s 粒度)");

    let start = Instant::now();
    let mut last_cloud: HashMap<(String, usize), Instant> = HashMap::new();
    let mut last_vcs: HashMap<(String, usize), Instant> = HashMap::new();

    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.tick().await; // 第一次立即返回,跳过

    loop {
        ticker.tick().await;
        let cfg = state.config.snapshot();
        let now = Instant::now();

        // ---- 云文档绑定:逐条按自己的间隔同步 ----
        let mut cloud_due: Vec<(crate::config::KnowledgeBase, usize)> = Vec::new();
        for kb in &cfg.knowledge_bases {
            for (idx, b) in kb.cloud_bindings.iter().enumerate() {
                if b.sync_interval_minutes == 0 || !b.is_complete() {
                    continue;
                }
                let key = (kb.id.clone(), idx);
                let elapsed = match last_cloud.get(&key) {
                    Some(t) => now.duration_since(*t),
                    None => now.duration_since(start),
                };
                if elapsed >= Duration::from_secs(b.sync_interval_minutes * 60) {
                    cloud_due.push((kb.clone(), idx));
                }
            }
        }
        for (kb, idx) in cloud_due {
            let st = state.clone();
            let kb_name = kb.name.clone();
            let kb_id = kb.id.clone();
            let binding_name = kb
                .cloud_bindings
                .get(idx)
                .map(|b| b.name.clone())
                .unwrap_or_default();
            let result = tokio::task::spawn_blocking(move || {
                feishu::sync_binding_with_record(&st, &kb, idx, "sync", "auto")
            })
            .await;
            match result {
                Ok(Ok(r)) => println!(
                    "[ktree] 云文档定时同步「{kb_name}/{binding_name}」: \
                     新增 {} 更新 {} 删除 {} 跳过 {} 失败 {}",
                    r.added.len(),
                    r.updated.len(),
                    r.deleted.len(),
                    r.skipped,
                    r.failed.len()
                ),
                Ok(Err(e)) => {
                    eprintln!("[ktree] 云文档定时同步「{kb_name}/{binding_name}」失败: {e}")
                }
                Err(e) => eprintln!("[ktree] 云文档定时同步任务异常: {e}"),
            }
            last_cloud.insert((kb_id, idx), Instant::now());
        }

        // ---- 每个 VCS 绑定按自己的间隔同步 ----
        // 先收集到期的 (kb, idx),再异步触发,避免在 for 中可变借用 last_vcs
        let mut due_list: Vec<(crate::config::KnowledgeBase, usize)> = Vec::new();
        for kb in &cfg.knowledge_bases {
            for (idx, b) in kb.vcs_bindings.iter().enumerate() {
                if b.sync_interval_minutes == 0 {
                    continue;
                }
                let key = (kb.id.clone(), idx);
                let interval = Duration::from_secs(b.sync_interval_minutes * 60);
                let elapsed = match last_vcs.get(&key) {
                    Some(t) => now.duration_since(*t),
                    None => now.duration_since(start),
                };
                if elapsed >= interval {
                    due_list.push((kb.clone(), idx));
                }
            }
        }

        for (kb, idx) in due_list {
            let st = state.clone();
            let kb_name = kb.name.clone();
            let kb_id = kb.id.clone();
            let url = kb.vcs_bindings[idx].url.clone();
            let result = tokio::task::spawn_blocking(move || {
                vcs::sync_binding_with_record(&st, &kb, idx, "auto")
            })
            .await;
            match result {
                Ok(Ok(r)) => println!(
                    "[ktree] VCS 定时同步「{kb_name}/{}」({url}): \
                     新增 {} 更新 {} 删除 {} 失败 {} @ {}",
                    r.name,
                    r.added.len(),
                    r.updated.len(),
                    r.deleted.len(),
                    r.failed.len(),
                    r.revision
                ),
                Ok(Err(e)) => eprintln!("[ktree] VCS 定时同步「{kb_name}#{idx}」失败: {e}"),
                Err(e) => eprintln!("[ktree] VCS 定时同步任务异常: {e}"),
            }
            last_vcs.insert((kb_id, idx), Instant::now());
        }
    }
}
