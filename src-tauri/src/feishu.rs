use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::config::KnowledgeBase;
use crate::ingest;
use crate::state::AppState;

#[derive(Serialize)]
struct SyncRequest<'a> {
    app_id: &'a str,
    app_secret: &'a str,
    folder_token: &'a str,
    /// 知识库根目录;feishu-sync.js 把文档写到 <kb_root>/src/feishu/、资源写到 ref/feishu/
    kb_root: &'a str,
    mode: &'a str,
}

#[derive(Deserialize)]
struct SyncDoc {
    /// 相对 src/ 的路径,如 "feishu/系统文档/xxx.md"
    rel_path: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct SyncOutput {
    ok: bool,
    #[serde(default)]
    documents: Vec<SyncDoc>,
    #[serde(default)]
    skipped: usize,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
    /// 飞书端已删除、需从本地移除的文档(相对 src/ 的路径)
    #[serde(default)]
    deleted: Vec<String>,
    #[serde(default)]
    error: String,
}

/// 一次飞书同步的结果汇总。
#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub kb_id: String,
    pub ingested: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub failed: usize,
    pub messages: Vec<String>,
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

/// 对单个知识库执行一次飞书同步:spawn feishu-sync.js 拉取并转换文档到 src/feishu/
/// → 逐个 ingest 入库建索引 → 处理删除 → 刷新 .ktree 元数据。
/// `mode` 取 "full"(全量)或 "sync"(增量)。此函数阻塞,调用方应放进 spawn_blocking。
pub fn sync(state: &AppState, kb: &KnowledgeBase, mode: &str) -> anyhow::Result<SyncReport> {
    if !kb.feishu.is_complete() {
        anyhow::bail!("知识库「{}」未配置飞书凭证", kb.name);
    }
    let kb_root = kb.root.to_string_lossy().into_owned();
    let req = serde_json::to_string(&SyncRequest {
        app_id: &kb.feishu.app_id,
        app_secret: &kb.feishu.app_secret,
        folder_token: &kb.feishu.folder_token,
        kb_root: &kb_root,
        mode,
    })?;

    let (program, args) = sidecar_command();
    let mut child = Command::new(&program)
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

    let mut report = SyncReport {
        kb_id: kb.id.clone(),
        ingested: 0,
        skipped: result.skipped,
        deleted: 0,
        failed: result.errors.len(),
        messages: Vec::new(),
    };

    for doc in &result.documents {
        match ingest::ingest_file(state, kb, &doc.rel_path, "feishu", true, false) {
            Ok(_) => report.ingested += 1,
            Err(e) => {
                report.failed += 1;
                report
                    .messages
                    .push(format!("入库失败「{}」: {e}", doc.title));
            }
        }
    }

    // 飞书端已删除的文档:sidecar 已删本地文件,这里清理 SQLite + 索引
    for rel_path in &result.deleted {
        if let Ok(Some(doc)) = state.store.get_by_path(&kb.id, rel_path) {
            let _ = state.store.delete_document(doc.id);
            let _ = state.index.delete(doc.id);
            report.deleted += 1;
            report.messages.push(format!("已移除「{}」", doc.title));
        }
    }
    if report.deleted > 0 {
        let _ = state.index.commit();
    }

    for e in &result.errors {
        report.messages.push(format!("同步错误: {e}"));
    }

    // 刷新 .ktree/INDEX.md 与 KEYWORDS.md
    let _ = ingest::refresh_kb_meta(state, kb);

    Ok(report)
}
