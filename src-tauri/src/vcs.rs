//! VCS(git / svn)同步:把仓库工作副本映射到 KB 的 src 子目录,
//! 拉取后跟 store 里的 doc 列表 diff,新增/变更走 ingest,VCS 端删了的本地清掉。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use md5::{Digest, Md5};
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
    /// 仅 git 子目录同步:同步的仓库内子目录;其它情况为空
    pub repo_sub_path: String,
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

/// git 子目录同步用的隐藏工作副本目录 —— 放 `.ktree/vcs-cache/` 下,`.git` 不进 src。
/// 按 url+branch+repo_sub_path 取哈希,改了任一项就换一个目录,互不干扰。
fn git_cache_dir(kb: &KnowledgeBase, b: &VcsBinding) -> PathBuf {
    let key = format!(
        "{:x}",
        Md5::digest(
            format!(
                "{}\u{0}{}\u{0}{}",
                b.url.trim(),
                b.branch.trim(),
                b.repo_sub_path.trim()
            )
            .as_bytes()
        )
    );
    kb.root
        .join(".ktree")
        .join("vcs-cache")
        .join(format!("git-{}", &key[..16]))
}

/// git 同步入口:`repo_sub_path` 为空走整仓克隆(工作副本就在 target);
/// 非空走稀疏检出 —— 在隐藏目录里只拉那个子目录,再扁平镜像到 target。
fn git_sync(kb: &KnowledgeBase, target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    let repo_sub = b.repo_sub_path.trim().trim_matches('/').to_string();
    if repo_sub.is_empty() {
        return git_whole_repo(target, b);
    }
    if safe_rel_path(&repo_sub).is_none() {
        anyhow::bail!("VCS 绑定 repo_sub_path「{repo_sub}」不合法");
    }
    let workdir = git_cache_dir(kb, b);
    let from = workdir.join(&repo_sub);
    // 拉取前先记下上一轮同步过的文件 —— 拉取后据此把 git 端已删除的文件从 target 精确清掉,
    // 只动本绑定自己同步过的文件,不会误删 target 里其它来源(用户上传 / 飞书 / 别的绑定)的内容。
    let prev_files = if from.is_dir() {
        list_repo_files(&from).unwrap_or_default()
    } else {
        Vec::new()
    };
    let revision = git_sparse_checkout(&workdir, b, &repo_sub)?;
    if !from.is_dir() {
        anyhow::bail!("git 仓库里找不到子目录「{repo_sub}」(分支 / 路径填对了吗?)");
    }
    // 把仓库子目录扁平镜像到 src 目标目录:补齐 / 覆盖文件,并清掉 git 端已删除的。
    mirror_tree(&from, target, &prev_files)?;
    Ok(revision)
}

/// 在隐藏工作副本里对一个仓库做稀疏检出,工作树里只保留 `repo_sub` 这一个子目录。
fn git_sparse_checkout(workdir: &Path, b: &VcsBinding, repo_sub: &str) -> anyhow::Result<String> {
    let url = git_url_with_creds(&b.url, &b.username, &b.password);
    let branch = b.branch.trim();
    if workdir.join(".git").exists() {
        // 已有工作副本:刷新稀疏集合(repo_sub 可能改过)→ fetch → reset 到上游
        let mut sp = Command::new("git");
        sp.current_dir(workdir)
            .args(["sparse-checkout", "set", repo_sub]);
        run_cmd(sp, "git sparse-checkout set")?;
        let mut fetch = Command::new("git");
        fetch.current_dir(workdir).args(["fetch", "--prune"]);
        run_cmd(fetch, "git fetch")?;
        let mut reset = Command::new("git");
        reset.current_dir(workdir).arg("reset").arg("--hard");
        if branch.is_empty() {
            reset.arg("@{u}");
        } else {
            reset.arg(format!("origin/{branch}"));
        }
        let _ = run_cmd(reset, "git reset --hard");
    } else {
        if let Some(parent) = workdir.parent() {
            fs::create_dir_all(parent)?;
        }
        // 优先 --filter=blob:none(只拉用到的 blob,省带宽);服务端不支持就回落整克隆
        if git_sparse_clone(&url, workdir, branch, true).is_err() {
            let _ = fs::remove_dir_all(workdir);
            git_sparse_clone(&url, workdir, branch, false)?;
        }
        let mut sp = Command::new("git");
        sp.current_dir(workdir)
            .args(["sparse-checkout", "set", repo_sub]);
        run_cmd(sp, "git sparse-checkout set")?;
        let mut co = Command::new("git");
        co.current_dir(workdir).arg("checkout");
        run_cmd(co, "git checkout")?;
    }
    let mut head = Command::new("git");
    head.current_dir(workdir).args(["rev-parse", "HEAD"]);
    Ok(run_cmd(head, "git rev-parse HEAD")?.trim().to_string())
}

/// 一次稀疏 clone(不检出工作树)。`partial=true` 时带 `--filter=blob:none`。
fn git_sparse_clone(url: &str, workdir: &Path, branch: &str, partial: bool) -> anyhow::Result<()> {
    let mut clone = Command::new("git");
    clone.args(["clone", "--no-checkout", "--sparse"]);
    if partial {
        clone.arg("--filter=blob:none");
    }
    if !branch.is_empty() {
        clone.arg("--branch").arg(branch);
    }
    clone.arg(url).arg(workdir);
    run_cmd(clone, "git clone --sparse")?;
    Ok(())
}

/// 把 `from` 目录树镜像进 `to`:复制 / 覆盖所有文件,并按 `prev_files`(上一轮同步过的
/// 相对路径)清掉 git 端已删除的文件。只动本绑定同步过的文件,`to` 里其它来源的内容
/// 不受影响 —— 因此 `to` 可以是 src 根目录。
fn mirror_tree(from: &Path, to: &Path, prev_files: &[String]) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    copy_tree(from, to, Path::new(""))?;
    for rel in prev_files {
        if from.join(rel).is_file() {
            continue; // git 里还有这个文件 → 保留
        }
        let p = to.join(rel);
        let _ = fs::remove_file(&p);
        // 顺手清掉因此空掉的目录(向上回溯到 `to` 为止)
        let mut dir = p.parent();
        while let Some(d) = dir {
            if d == to || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    Ok(())
}

/// 递归把 `from` 下的文件复制进 `to`(覆盖同名);遇到类型冲突先清掉旧的。
fn copy_tree(from: &Path, to: &Path, rel: &Path) -> anyhow::Result<()> {
    for e in fs::read_dir(from.join(rel))?.flatten() {
        let r = rel.join(e.file_name());
        let dst = to.join(&r);
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if dst.is_file() {
                    let _ = fs::remove_file(&dst);
                }
                fs::create_dir_all(&dst)?;
                copy_tree(from, to, &r)?;
            }
            Ok(_) => {
                if dst.is_dir() {
                    let _ = fs::remove_dir_all(&dst);
                } else {
                    let _ = fs::remove_file(&dst);
                }
                fs::copy(from.join(&r), &dst)?;
            }
            Err(_) => {}
        }
    }
    Ok(())
}

/// 整仓克隆 / 更新:工作副本(含 `.git`)就落在 target。
fn git_whole_repo(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
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

    let repo_sub = b.repo_sub_path.trim().trim_matches('/').to_string();

    let mut report = VcsSyncReport {
        kb_id: kb.id.clone(),
        binding_idx,
        vcs_type: b.vcs_type.clone(),
        url: b.url.clone(),
        sub_dir: sub_norm.clone(),
        repo_sub_path: repo_sub.clone(),
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
        "git" => git_sync(kb, &target, &b)?,
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
