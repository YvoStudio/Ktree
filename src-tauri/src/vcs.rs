//! VCS(git / svn)同步:把仓库工作副本映射到 KB 的 src 子目录,
//! 拉取后跟 store 里的 doc 列表 diff,新增/变更走 ingest,VCS 端删了的本地清掉。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::config::{KnowledgeBase, VcsBinding};
use crate::ingest::{self, safe_rel_path};
use crate::state::AppState;

/// 一次 VCS 同步的结果汇总。
#[derive(Debug, Clone, Serialize)]
pub struct VcsSyncReport {
    pub kb_id: String,
    pub binding_idx: usize,
    pub vcs_type: String,
    pub url: String,
    pub sub_dir: String,
    /// VCS 命令拉取后报告的修订(git: HEAD sha;svn: working copy 修订号)
    pub revision: String,
    /// 新加进 store 的文件(rel_path,相对 src/)
    pub added: Vec<String>,
    /// 内容有变化、被重新 ingest 的文件
    pub updated: Vec<String>,
    /// VCS 端删了、本地从 store 也清掉的文件
    pub deleted: Vec<String>,
    pub failed: Vec<String>,
    pub messages: Vec<String>,
}

/// 校验一个 VCS 绑定的 sub_dir 合法且唯一指向 src 子目录。
fn binding_target_dir(kb: &KnowledgeBase, b: &VcsBinding) -> anyhow::Result<(PathBuf, String)> {
    let sub = b.sub_dir.trim().trim_matches('/').to_string();
    if !sub.is_empty() {
        // 路径里不能有 .. 段
        if safe_rel_path(&sub).is_none() {
            anyhow::bail!("VCS 绑定 sub_dir「{sub}」不合法");
        }
    }
    let target = if sub.is_empty() {
        kb.root.join("src")
    } else {
        kb.root.join("src").join(&sub)
    };
    Ok((target, sub))
}

/// 把可选凭证注入到 https://… 形式的 URL 里(git 用)。
/// 已经带 user:pass 的 URL 不动。其他形式(ssh://、git@host:…)也不动,留给系统凭证。
fn git_url_with_creds(url: &str, user: &str, pass: &str) -> String {
    if user.is_empty() && pass.is_empty() {
        return url.to_string();
    }
    if let Some(rest) = url.strip_prefix("https://") {
        // 已经带凭证?保持原样
        if rest.contains('@') && rest.split('@').next().map(|s| s.contains(':')).unwrap_or(false) {
            return url.to_string();
        }
        let u = urlencoding_minimal(user);
        let p = urlencoding_minimal(pass);
        let creds = if pass.is_empty() { u } else { format!("{u}:{p}") };
        return format!("https://{creds}@{rest}");
    }
    if let Some(rest) = url.strip_prefix("http://") {
        if rest.contains('@') && rest.split('@').next().map(|s| s.contains(':')).unwrap_or(false) {
            return url.to_string();
        }
        let u = urlencoding_minimal(user);
        let p = urlencoding_minimal(pass);
        let creds = if pass.is_empty() { u } else { format!("{u}:{p}") };
        return format!("http://{creds}@{rest}");
    }
    url.to_string()
}

/// 仅做最少 URL 编码:`/ : @ %` 这几个会破坏 URL 结构的字符。完整 URL-encode 用三方库未免太重。
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' | ':' | '@' | '%' | '?' | '#' | ' ' => {
                out.push('%');
                out.push_str(&format!("{:02X}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out
}

/// 跑一条命令,捕获 stdout/stderr。失败时把两者一起塞进 anyhow error。
fn run_cmd(mut cmd: Command, ctx: &str) -> anyhow::Result<String> {
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("{ctx}:启动失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{ctx} 失败: {stderr}");
    }
    Ok(stdout)
}

fn git_pull_or_clone(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    let url = git_url_with_creds(&b.url, &b.username, &b.password);
    if target.join(".git").exists() {
        // 已有工作副本 → fetch + reset 到上游
        let branch = if b.branch.trim().is_empty() {
            None
        } else {
            Some(b.branch.trim().to_string())
        };
        let mut fetch = Command::new("git");
        fetch.current_dir(target).arg("fetch").arg("--prune");
        run_cmd(fetch, "git fetch")?;
        let mut reset = Command::new("git");
        reset.current_dir(target).arg("reset").arg("--hard");
        if let Some(br) = &branch {
            reset.arg(format!("origin/{br}"));
        } else {
            reset.arg("@{u}");
        }
        let _ = run_cmd(reset, "git reset --hard");
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut clone = Command::new("git");
        clone.arg("clone").arg(&url).arg(target);
        if !b.branch.trim().is_empty() {
            clone.arg("--branch").arg(b.branch.trim());
        }
        run_cmd(clone, "git clone")?;
    }
    // 取当前 HEAD sha 当 revision
    let mut head = Command::new("git");
    head.current_dir(target).arg("rev-parse").arg("HEAD");
    Ok(run_cmd(head, "git rev-parse HEAD")?.trim().to_string())
}

fn svn_update_or_checkout(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    if target.join(".svn").exists() {
        let mut up = Command::new("svn");
        up.current_dir(target)
            .arg("update")
            .arg("--non-interactive");
        if !b.username.is_empty() {
            up.arg("--username").arg(&b.username);
        }
        if !b.password.is_empty() {
            up.arg("--password").arg(&b.password);
        }
        run_cmd(up, "svn update")?;
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut co = Command::new("svn");
        co.arg("checkout")
            .arg(&b.url)
            .arg(target)
            .arg("--non-interactive");
        if !b.username.is_empty() {
            co.arg("--username").arg(&b.username);
        }
        if !b.password.is_empty() {
            co.arg("--password").arg(&b.password);
        }
        run_cmd(co, "svn checkout")?;
    }
    let mut info = Command::new("svn");
    info.current_dir(target)
        .arg("info")
        .arg("--show-item")
        .arg("revision");
    Ok(run_cmd(info, "svn info revision")?.trim().to_string())
}

/// 递归列出 `base/` 下所有文件,过滤掉 VCS 元数据目录。
/// 返回的路径是相对 `base` 的正斜杠形式。
fn list_repo_files(base: &Path) -> anyhow::Result<Vec<String>> {
    fn walk(base: &Path, rel: &Path, out: &mut Vec<String>) -> anyhow::Result<()> {
        let dir = base.join(rel);
        if !dir.is_dir() {
            return Ok(());
        }
        for e in fs::read_dir(&dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // VCS 元数据 + 隐藏文件直接跳过
            if name == ".git" || name == ".svn" {
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            let p = rel.join(&name);
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                walk(base, &p, out)?;
            } else if ft.is_file() || ft.is_symlink() {
                out.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(base, &PathBuf::new(), &mut out)?;
    Ok(out)
}

/// 对一个绑定执行一次同步:拉取/更新 → diff store → 入库 / 删除。
/// 阻塞调用,放在 spawn_blocking 里跑。
pub fn sync_binding(
    state: &AppState,
    kb: &KnowledgeBase,
    binding_idx: usize,
) -> anyhow::Result<VcsSyncReport> {
    let b = kb
        .vcs_bindings
        .get(binding_idx)
        .ok_or_else(|| anyhow::anyhow!("VCS 绑定 idx={binding_idx} 不存在"))?
        .clone();
    let (target, sub_norm) = binding_target_dir(kb, &b)?;

    let mut report = VcsSyncReport {
        kb_id: kb.id.clone(),
        binding_idx,
        vcs_type: b.vcs_type.clone(),
        url: b.url.clone(),
        sub_dir: sub_norm.clone(),
        revision: String::new(),
        added: Vec::new(),
        updated: Vec::new(),
        deleted: Vec::new(),
        failed: Vec::new(),
        messages: Vec::new(),
    };

    // 拉取前 store 里这个 sub 下所有 rel_path,作为"删除"检测的基线
    let before: HashSet<String> = state
        .store
        .list_documents(&kb.id, if sub_norm.is_empty() { None } else { Some(&sub_norm) })?
        .into_iter()
        .map(|d| d.rel_path)
        .collect();

    // 拉取 / 更新
    report.revision = match b.vcs_type.as_str() {
        "git" => git_pull_or_clone(&target, &b)?,
        "svn" => svn_update_or_checkout(&target, &b)?,
        other => anyhow::bail!("不支持的 VCS 类型「{other}」(只支持 git / svn)"),
    };

    // 列出工作副本里所有文件 → 拼成相对 src/ 的路径
    let files = list_repo_files(&target)?;
    let after: HashSet<String> = files
        .iter()
        .map(|r| {
            if sub_norm.is_empty() {
                r.clone()
            } else {
                format!("{sub_norm}/{r}")
            }
        })
        .collect();

    // ingest 每个文件;ingest_file 内的 md5 短路会区分新增/无变化
    for rel_path in &after {
        // 在 ingest 之前看 store 里有没有这条记录 → 决定 added vs updated
        let was_present = before.contains(rel_path);
        let old_md5 = state
            .store
            .get_by_path(&kb.id, rel_path)
            .ok()
            .flatten()
            .map(|d| d.md5);
        match ingest::ingest_file(state, kb, rel_path, "vcs", true, false) {
            Ok(doc) => {
                if !was_present {
                    report.added.push(rel_path.clone());
                } else if old_md5.as_deref() != Some(&doc.md5) {
                    report.updated.push(rel_path.clone());
                }
            }
            Err(e) => {
                report.failed.push(rel_path.clone());
                report.messages.push(format!("入库失败「{rel_path}」: {e}"));
            }
        }
    }

    // 算 deleted:before - after,这些是 VCS 端被删掉的,清 store + 软链/资源
    let deleted: Vec<String> = before.difference(&after).cloned().collect();
    for rel_path in &deleted {
        if let Ok(Some(doc)) = state.store.get_by_path(&kb.id, rel_path) {
            // delete_doc 会清 src(已被 VCS 删过 → 文件不存在也没事)、docs/、ref/、manifest、SQLite、tantivy
            if let Err(e) = ingest::delete_doc(state, kb, &doc) {
                report.failed.push(rel_path.clone());
                report.messages.push(format!("清理失败「{rel_path}」: {e}"));
            } else {
                report.deleted.push(rel_path.clone());
            }
        }
    }

    // 刷新 .ktree 元数据
    let _ = ingest::refresh_kb_meta(state, kb);

    Ok(report)
}

/// 对一个 KB 的所有绑定依次同步。任意一个失败不影响其它。
pub fn sync_kb_all(state: &AppState, kb: &KnowledgeBase) -> Vec<anyhow::Result<VcsSyncReport>> {
    (0..kb.vcs_bindings.len())
        .map(|i| sync_binding(state, kb, i))
        .collect()
}
