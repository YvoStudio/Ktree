//! 飞书云文档同步:按 CloudBinding 把飞书文件夹 / 单篇文档严格镜像到
//! KB 的 src/cloud/feishu/<绑定名>/ 目录。
//!
//! 实际拉取与转换由 Node sidecar(feishu-sync.js)完成:
//! - md 写到 src/cloud/feishu/<绑定名>/<层级>/<名>.md
//! - 图片附件写到 docs/cloud/feishu/<绑定名>/<层级>/<名>.assets/(md 内同目录相对引用)
//! - 飞书端已删除 / 本地多余的文件由 sidecar 严格镜像清掉
//! Rust 侧负责 ingest 入库与 store / 索引清理。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use crate::config::{CloudBinding, KnowledgeBase, AREA_CLOUD};
use crate::ingest::{self, safe_component};
use crate::state::{AppState, LastSync};

#[derive(Serialize)]
struct SyncRequest<'a> {
    app_id: &'a str,
    app_secret: &'a str,
    /// "folder"(共享文件夹,递归)或 "doc"(单篇文档)
    target_type: &'a str,
    target_token: &'a str,
    /// 知识库根目录
    kb_root: &'a str,
    /// 相对 src/ 与 docs/ 的目标前缀,如 "cloud/feishu/产品需求"
    dest_prefix: &'a str,
    mode: &'a str,
}

#[derive(Deserialize)]
struct SyncDoc {
    /// 相对 src/ 的路径(带 dest_prefix),如 "cloud/feishu/产品需求/xxx.md"
    rel_path: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct SyncOutput {
    ok: bool,
    /// 本轮新增 / 有变化、需要 ingest 的文档
    #[serde(default)]
    documents: Vec<SyncDoc>,
    /// 同步后实际存在的全部文档(含未变化跳过的),用于 store 端的删除检测
    #[serde(default)]
    present: Vec<String>,
    #[serde(default)]
    skipped: usize,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
    #[serde(default)]
    error: String,
}

/// 一次云文档同步的结果汇总。
#[derive(Debug, Serialize)]
pub struct CloudSyncReport {
    pub kb_id: String,
    pub binding_idx: usize,
    pub provider: String,
    /// 绑定名(= src/cloud/<provider>/ 下的目录名)
    pub name: String,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    pub skipped: usize,
    pub failed: Vec<String>,
    pub messages: Vec<String>,
}

/// 绑定的目标前缀(相对 src/):"cloud/feishu/<绑定名>"。
fn binding_prefix(b: &CloudBinding) -> anyhow::Result<String> {
    let name = safe_component(&b.name)
        .ok_or_else(|| anyhow::anyhow!("云文档绑定名「{}」不合法", b.name))?;
    Ok(format!("{AREA_CLOUD}/{}/{name}", b.provider))
}

/// 解析 feishu-sync.js sidecar 的调用方式。
/// 开发期(debug):`node <project>/sidecar/feishu-sync.js`
/// 打包后(release):主程序同目录的自包含二进制 `feishu-sync`。
fn sidecar_command() -> (String, Vec<String>) {
    #[cfg(debug_assertions)]
    {
        let script: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.join("sidecar").join("feishu-sync.js"))
            .unwrap_or_else(|| PathBuf::from("sidecar/feishu-sync.js"));
        ("node".to_string(), vec![script.to_string_lossy().into_owned()])
    }
    #[cfg(not(debug_assertions))]
    {
        let bin = crate::convert::sidecar_binary_path("feishu-sync");
        (bin.to_string_lossy().into_owned(), Vec::new())
    }
}

/// 对一条云文档绑定执行一次同步。
/// `mode` 取 "full"(全量)或 "sync"(增量)。此函数阻塞,调用方应放进 spawn_blocking。
pub fn sync_binding(
    state: &AppState,
    kb: &KnowledgeBase,
    binding_idx: usize,
    mode: &str,
) -> anyhow::Result<CloudSyncReport> {
    let b = kb
        .cloud_bindings
        .get(binding_idx)
        .ok_or_else(|| anyhow::anyhow!("云文档绑定 idx={binding_idx} 不存在"))?
        .clone();
    if b.provider != "feishu" {
        anyhow::bail!("不支持的云文档提供方「{}」(目前只支持 feishu)", b.provider);
    }
    if !b.is_complete() {
        anyhow::bail!("知识库「{}」的云文档绑定「{}」凭证不完整", kb.name, b.name);
    }
    let prefix = binding_prefix(&b)?;

    // 同步前 store 里这个绑定目录下所有 rel_path,作为"删除"检测的基线
    let before: HashSet<String> = state
        .store
        .list_documents(&kb.id, Some(&prefix))?
        .into_iter()
        .map(|d| d.rel_path)
        .collect();

    let kb_root = kb.root.to_string_lossy().into_owned();
    let req = serde_json::to_string(&SyncRequest {
        app_id: &b.app_id,
        app_secret: &b.app_secret,
        target_type: &b.target_type,
        target_token: &b.target_token,
        kb_root: &kb_root,
        dest_prefix: &prefix,
        mode,
    })?;

    let (program, args) = sidecar_command();
    let mut cmd = crate::convert::sidecar_process(&program);
    let mut child = cmd
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("启动飞书同步 sidecar 失败({program}): {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 sidecar stdin"))?;
        stdin.write_all(req.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "飞书同步 sidecar 异常退出: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let result: SyncOutput = serde_json::from_str(last_line.trim())
        .map_err(|e| anyhow::anyhow!("解析飞书同步输出失败: {e}; 原始 stdout: {stdout}"))?;
    if !result.ok {
        anyhow::bail!("飞书同步失败: {}", result.error);
    }

    let mut report = CloudSyncReport {
        kb_id: kb.id.clone(),
        binding_idx,
        provider: b.provider.clone(),
        name: b.name.trim().to_string(),
        added: Vec::new(),
        updated: Vec::new(),
        deleted: Vec::new(),
        skipped: result.skipped,
        failed: Vec::new(),
        messages: Vec::new(),
    };

    // ingest 本轮有变化的文档(md 是文本 → 软链镜像进 docs/)
    for doc in &result.documents {
        let was_present = before.contains(&doc.rel_path);
        match ingest::ingest_file(state, kb, &doc.rel_path, "feishu", true, false) {
            Ok(_) => {
                if was_present {
                    report.updated.push(doc.rel_path.clone());
                } else {
                    report.added.push(doc.rel_path.clone());
                }
            }
            Err(e) => {
                report.failed.push(doc.rel_path.clone());
                report
                    .messages
                    .push(format!("入库失败「{}」: {e}", doc.title));
            }
        }
    }

    // 删除检测:store 里有、但同步后已不存在的 → 清 store / 索引 / docs 产物。
    // sidecar 已做盘上的严格镜像,这里收口缓存层。
    let present: HashSet<String> = result.present.iter().cloned().collect();
    for rel_path in before.difference(&present) {
        if let Ok(Some(doc)) = state.store.get_by_path(&kb.id, rel_path) {
            if let Err(e) = ingest::delete_doc(state, kb, &doc) {
                report.failed.push(rel_path.clone());
                report.messages.push(format!("清理失败「{rel_path}」: {e}"));
            } else {
                report.deleted.push(rel_path.clone());
            }
        }
    }

    for e in &result.errors {
        report.messages.push(format!("同步错误: {e}"));
    }

    // 严格镜像收尾:清掉 docs 区里 store 已不认识的孤儿文件/空目录
    let _ = ingest::prune_docs_orphans(state, kb, &prefix);

    // 刷新 .ktree 元数据
    let _ = ingest::refresh_kb_meta(state, kb);

    Ok(report)
}

/// 删除一条云文档绑定的本地内容:src/cloud/<provider>/<name>/ 目录、docs 产物、
/// store / 索引记录、同步状态文件。供「删除绑定」接口在删配置后调用。
pub fn purge_binding_data(
    state: &AppState,
    kb: &KnowledgeBase,
    b: &CloudBinding,
) -> anyhow::Result<usize> {
    let prefix = binding_prefix(b)?;
    let removed = ingest::delete_folder(state, kb, &prefix)?;
    // 清掉 sidecar 的增量同步状态文件
    let state_name = prefix.replace('/', "_");
    let _ = std::fs::remove_file(
        kb.root
            .join(".ktree")
            .join(format!(".cloud-sync-{state_name}.json")),
    );
    let _ = ingest::refresh_kb_meta(state, kb);
    Ok(removed)
}

/// 调 `sync_binding`,无论成败都把结果记到 `state.last_cloud_sync`,供 webui 展示。
/// 定时与手动同步的统一收口。
pub fn sync_binding_with_record(
    state: &AppState,
    kb: &KnowledgeBase,
    binding_idx: usize,
    mode: &str,
    source: &str,
) -> anyhow::Result<CloudSyncReport> {
    // 并发保护:同一绑定同时只允许一个同步在跑
    if !state.try_begin_sync("cloud", &kb.id, binding_idx) {
        anyhow::bail!("该绑定正在同步中,请等当前同步结束");
    }
    let result = sync_binding(state, kb, binding_idx, mode);
    state.end_sync("cloud", &kb.id, binding_idx);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let entry = match &result {
        Ok(r) => LastSync {
            at_unix_ms: now_ms,
            source: source.to_string(),
            ok: true,
            revision: String::new(),
            added: r.added.len(),
            updated: r.updated.len(),
            deleted: r.deleted.len(),
            failed: r.failed.len(),
            error: None,
        },
        Err(e) => LastSync {
            at_unix_ms: now_ms,
            source: source.to_string(),
            ok: false,
            revision: String::new(),
            added: 0,
            updated: 0,
            deleted: 0,
            failed: 0,
            error: Some(e.to_string()),
        },
    };
    // 写内存 map + 持久化到 SQLite(重启后仍能显示最近同步时间)
    state.record_sync("cloud", &kb.id, binding_idx, entry);
    result
}
