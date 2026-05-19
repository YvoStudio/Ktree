use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::feishu;
use crate::state::AppState;
use crate::vcs;

/// 后台定时同步循环。每分钟 tick 一次,从 config snapshot 重新读最新设置 ——
/// 改 config 不用重启应用就能生效。
///
/// 触发条件:
/// - 飞书全局:`config.sync_interval_minutes > 0`,且距上次同步 ≥ 该间隔(分钟)。
/// - VCS 绑定逐个:每条绑定的 `sync_interval_minutes > 0`,且距上次同步 ≥ 该间隔。
///
/// 上次同步时间只活在进程内存,重启进程会从零计时,首轮在 interval 到期后才跑。
pub async fn run(state: AppState) {
    println!("[ktree] 后台调度器启动(60s 粒度)");

    let start = Instant::now();
    let mut last_feishu: Option<Instant> = None;
    let mut last_vcs: HashMap<(String, usize), Instant> = HashMap::new();

    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.tick().await; // 第一次立即返回,跳过

    loop {
        ticker.tick().await;
        let cfg = state.config.snapshot();
        let now = Instant::now();

        // ---- 全局飞书定时同步(沿用旧行为)----
        if cfg.sync_interval_minutes > 0 {
            let due = match last_feishu {
                Some(t) => now.duration_since(t) >= Duration::from_secs(cfg.sync_interval_minutes * 60),
                None => now.duration_since(start) >= Duration::from_secs(cfg.sync_interval_minutes * 60),
            };
            if due {
                for kb in cfg
                    .knowledge_bases
                    .iter()
                    .filter(|k| k.feishu.is_complete())
                    .cloned()
                    .collect::<Vec<_>>()
                {
                    let st = state.clone();
                    let name = kb.name.clone();
                    match tokio::task::spawn_blocking(move || feishu::sync(&st, &kb, "sync")).await {
                        Ok(Ok(r)) => println!(
                            "[ktree] 飞书定时同步「{name}」: 入库 {} 跳过 {} 删除 {} 失败 {}",
                            r.ingested, r.skipped, r.deleted, r.failed
                        ),
                        Ok(Err(e)) => eprintln!("[ktree] 飞书定时同步「{name}」失败: {e}"),
                        Err(e) => eprintln!("[ktree] 飞书定时同步任务异常: {e}"),
                    }
                }
                last_feishu = Some(Instant::now());
            }
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
            let result =
                tokio::task::spawn_blocking(move || vcs::sync_binding(&st, &kb, idx)).await;
            match result {
                Ok(Ok(r)) => println!(
                    "[ktree] VCS 定时同步「{kb_name}#{idx}」({url}): \
                     新增 {} 更新 {} 删除 {} 失败 {} @ {}",
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
