//! VCS(git / svn)同步:把仓库工作副本严格镜像到 KB 的 src/vcs/<绑定名>/ 目录。
//! 该目录由绑定独占(不允许上传 / 手动修改),因此同步采用严格镜像:
//! 仓库里没有的文件 —— 不管是 VCS 端删除的还是外部混进来的 —— 一律清掉
//! (盘 + SQLite + tantivy + docs 产物)。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use md5::{Digest, Md5};
use serde::Serialize;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use crate::config::{KnowledgeBase, VcsBinding, AREA_VCS};
use crate::ingest::{self, safe_component};
use crate::kbmeta;
use crate::state::{AppState, LastVcsSync};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 一次 VCS 同步的结果汇总。
#[derive(Debug, Clone, Serialize)]
pub struct VcsSyncReport {
    pub kb_id: String,
    pub binding_idx: usize,
    pub vcs_type: String,
    pub url: String,
    /// 绑定名(= src/vcs/ 下的目录名)
    pub name: String,
    /// 仅 git 子目录同步:同步的仓库内子目录;其它情况为空
    pub repo_sub_path: String,
    /// VCS 命令拉取后报告的修订(git: HEAD sha;svn: working copy 修订号)
    pub revision: String,
    /// 新加进 store 的文件(rel_path,相对 src/)
    pub added: Vec<String>,
    /// 内容有变化、被重新 ingest 的文件
    pub updated: Vec<String>,
    /// 仓库里没有、被严格镜像清掉的文件
    pub deleted: Vec<String>,
    pub failed: Vec<String>,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone)]
enum ReconcileMode {
    Full,
    Incremental {
        changed: HashSet<String>,
        deleted: HashSet<String>,
    },
}

#[derive(Debug, Clone)]
struct VcsPullResult {
    revision: String,
    reconcile: ReconcileMode,
}

impl VcsPullResult {
    fn full(revision: String) -> Self {
        Self {
            revision,
            reconcile: ReconcileMode::Full,
        }
    }

    fn incremental(revision: String, changed: HashSet<String>, deleted: HashSet<String>) -> Self {
        Self {
            revision,
            reconcile: ReconcileMode::Incremental { changed, deleted },
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SvnEntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone)]
struct SvnListEntry {
    rel: String,
    kind: SvnEntryKind,
}

/// 校验绑定名并返回 (目标绝对路径, 相对 src 的前缀 "vcs/<name>")。
fn binding_target_dir(kb: &KnowledgeBase, b: &VcsBinding) -> anyhow::Result<(PathBuf, String)> {
    let name = safe_component(&b.name)
        .ok_or_else(|| anyhow::anyhow!("VCS 绑定名「{}」不合法", b.name))?;
    let prefix = format!("{AREA_VCS}/{name}");
    let target = kb.root.join("src").join(&prefix);
    Ok((target, prefix))
}

fn vcs_command(program: &str) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new(resolve_vcs_program(program));
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new(resolve_vcs_program(program))
    }
}

fn resolve_vcs_program(program: &str) -> PathBuf {
    let direct = Path::new(program);
    if direct.components().count() > 1 {
        return direct.to_path_buf();
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if candidate.exists() {
                return candidate;
            }
            #[cfg(target_os = "windows")]
            {
                let candidate = dir.join(format!("{program}.exe"));
                if candidate.exists() {
                    return candidate;
                }
            }
        }
    }

    // macOS GUI apps do not inherit the user's shell PATH, so Homebrew tools
    // are often invisible unless we probe the common install locations.
    #[cfg(target_os = "macos")]
    for candidate in [
        format!("/opt/homebrew/bin/{program}"),
        format!("/usr/local/bin/{program}"),
        format!("/usr/bin/{program}"),
        format!("/bin/{program}"),
    ] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return path;
        }
    }

    #[cfg(target_os = "windows")]
    for candidate in match program {
        "git" => [
            r"C:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files\Git\bin\git.exe",
            r"C:\Program Files (x86)\Git\cmd\git.exe",
            r"C:\Program Files (x86)\Git\bin\git.exe",
            "",
            "",
        ],
        "svn" => [
            r"C:\Program Files\TortoiseSVN\bin\svn.exe",
            r"C:\Program Files\SlikSvn\bin\svn.exe",
            r"C:\Program Files\Subversion\bin\svn.exe",
            r"C:\Program Files (x86)\TortoiseSVN\bin\svn.exe",
            r"C:\Program Files (x86)\SlikSvn\bin\svn.exe",
            r"C:\Program Files (x86)\Subversion\bin\svn.exe",
        ],
        _ => ["", "", "", "", "", ""],
    } {
        if candidate.is_empty() {
            continue;
        }
        let path = PathBuf::from(candidate);
        if path.exists() {
            return path;
        }
    }

    PathBuf::from(program)
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
/// 按 绑定名+url+branch+repo_sub_path 取哈希,改了任一项就换一个目录,互不干扰。
fn git_cache_dir(kb: &KnowledgeBase, b: &VcsBinding) -> PathBuf {
    let key = format!(
        "{:x}",
        Md5::digest(
            format!(
                "{}\u{0}{}\u{0}{}\u{0}{}",
                b.name.trim(),
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
/// 非空走稀疏检出 —— 在隐藏目录里只拉那个子目录,再严格镜像到 target。
fn git_sync(kb: &KnowledgeBase, target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    let repo_sub = b.repo_sub_path.trim().trim_matches('/').to_string();
    if repo_sub.is_empty() {
        return git_whole_repo(target, b);
    }
    if crate::ingest::safe_rel_path(&repo_sub).is_none() {
        anyhow::bail!("VCS 绑定 repo_sub_path「{repo_sub}」不合法");
    }
    let workdir = git_cache_dir(kb, b);
    let revision = git_sparse_checkout(&workdir, b, &repo_sub)?;
    let from = workdir.join(&repo_sub);
    if !from.is_dir() {
        anyhow::bail!("git 仓库里找不到子目录「{repo_sub}」(分支 / 路径填对了吗?)");
    }
    // 严格镜像:仓库子目录 → src/vcs/<name>/,多余文件清掉
    mirror_tree_strict(&from, target)?;
    Ok(revision)
}

/// 在隐藏工作副本里对一个仓库做稀疏检出,工作树里只保留 `repo_sub` 这一个子目录。
fn git_sparse_checkout(workdir: &Path, b: &VcsBinding, repo_sub: &str) -> anyhow::Result<String> {
    let url = git_url_with_creds(&b.url, &b.username, &b.password);
    let branch = b.branch.trim();
    if workdir.join(".git").exists() {
        // 已有工作副本:刷新稀疏集合(repo_sub 可能改过)→ fetch → reset 到上游
        let mut sp = vcs_command("git");
        sp.current_dir(workdir)
            .args(["sparse-checkout", "set", repo_sub]);
        run_cmd(sp, "git sparse-checkout set")?;
        let mut fetch = vcs_command("git");
        fetch.current_dir(workdir).args(["fetch", "--prune"]);
        run_cmd(fetch, "git fetch")?;
        let mut reset = vcs_command("git");
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
        let mut sp = vcs_command("git");
        sp.current_dir(workdir)
            .args(["sparse-checkout", "set", repo_sub]);
        run_cmd(sp, "git sparse-checkout set")?;
        let mut co = vcs_command("git");
        co.current_dir(workdir).arg("checkout");
        run_cmd(co, "git checkout")?;
    }
    let mut head = vcs_command("git");
    head.current_dir(workdir).args(["rev-parse", "HEAD"]);
    Ok(run_cmd(head, "git rev-parse HEAD")?.trim().to_string())
}

/// 一次稀疏 clone(不检出工作树)。`partial=true` 时带 `--filter=blob:none`。
fn git_sparse_clone(url: &str, workdir: &Path, branch: &str, partial: bool) -> anyhow::Result<()> {
    let mut clone = vcs_command("git");
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

/// 把 `from` 目录树严格镜像进 `to`:复制 / 覆盖所有文件,并删除 `to` 里
/// 不存在于 `from` 的文件(及因此空掉的目录)。`to` 由本绑定独占,可以放心清。
fn mirror_tree_strict(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    copy_tree(from, to, Path::new(""))?;
    // 清掉 to 里 from 没有的文件
    for rel in list_repo_files(to)? {
        if from.join(&rel).is_file() {
            continue;
        }
        let p = to.join(&rel);
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
        let name = e.file_name().to_string_lossy().into_owned();
        // VCS 元数据不进 src
        if name == ".git" || name == ".svn" {
            continue;
        }
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
/// 更新后跑 `git clean -fd` 清掉一切 untracked 内容 —— 严格镜像。
fn git_whole_repo(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    let url = git_url_with_creds(&b.url, &b.username, &b.password);
    if target.join(".git").exists() {
        // 已有工作副本 → fetch + reset 到上游 + clean
        let branch = if b.branch.trim().is_empty() {
            None
        } else {
            Some(b.branch.trim().to_string())
        };
        let mut fetch = vcs_command("git");
        fetch.current_dir(target).arg("fetch").arg("--prune");
        run_cmd(fetch, "git fetch")?;
        let mut reset = vcs_command("git");
        reset.current_dir(target).arg("reset").arg("--hard");
        if let Some(br) = &branch {
            reset.arg(format!("origin/{br}"));
        } else {
            reset.arg("@{u}");
        }
        let _ = run_cmd(reset, "git reset --hard");
        // 严格镜像:untracked 文件 / 目录全部清掉
        let mut clean = vcs_command("git");
        clean.current_dir(target).args(["clean", "-fd"]);
        let _ = run_cmd(clean, "git clean -fd");
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // clone 要求目标目录不存在或为空;残留旧目录(非 git)时先清掉
        if target.exists() && fs::read_dir(target).map(|mut d| d.next().is_some()).unwrap_or(false)
        {
            fs::remove_dir_all(target)?;
        }
        let mut clone = vcs_command("git");
        clone.arg("clone").arg(&url).arg(target);
        if !b.branch.trim().is_empty() {
            clone.arg("--branch").arg(b.branch.trim());
        }
        run_cmd(clone, "git clone")?;
    }
    // 取当前 HEAD sha 当 revision
    let mut head = vcs_command("git");
    head.current_dir(target).arg("rev-parse").arg("HEAD");
    Ok(run_cmd(head, "git rev-parse HEAD")?.trim().to_string())
}

fn svn_update_or_checkout(target: &Path, b: &VcsBinding) -> anyhow::Result<VcsPullResult> {
    if target.join(".svn").exists() {
        // 上次 checkout / update 中断会留下锁(E155004),先 cleanup 再 update。
        // cleanup 自身失败不管,让 update 报真实错误。
        let mut cleanup = vcs_command("svn");
        cleanup
            .current_dir(target)
            .arg("cleanup")
            .arg("--non-interactive");
        let _ = run_cmd(cleanup, "svn cleanup");

        let update_output = match svn_update(target, b) {
            Ok(out) => out,
            Err(e) => {
                // 只有"工作副本损坏"类错误(E155xxx,如锁损坏、wc.db 坏)才清掉重建;
                // 远端错误(E170000 URL 不存在、E175xxx 网络不通、认证失败等)直接报错,
                // 保留本地镜像 —— 删了也下载不回来,反而丢数据。
                let msg = e.to_string();
                if msg.contains("E155") {
                    eprintln!("[ktree] svn 工作副本损坏({e}),清掉重新 checkout");
                    fs::remove_dir_all(target)?;
                    return svn_checkout_or_export(target, b);
                } else {
                    return Err(e);
                }
            }
        };
        // 严格镜像:清掉 unversioned 文件(svn status 第一列 '?')
        svn_clean_unversioned(target);
        let revision = svn_working_copy_revision(target)?;
        let (changed, deleted) = parse_svn_update_changes(&update_output);
        if svn_export_marker(target).exists() {
            let (changed, deleted) = svn_refresh_sparse_workcopy(target, b, &revision, changed, deleted)?;
            return Ok(VcsPullResult::incremental(revision, changed, deleted));
        }
        return Ok(VcsPullResult::incremental(revision, changed, deleted));
    } else if svn_export_marker(target).exists() {
        return svn_checkout_or_export(target, b);
    } else {
        return svn_checkout_or_export(target, b);
    }
}

fn svn_working_copy_revision(target: &Path) -> anyhow::Result<String> {
    let mut info = vcs_command("svn");
    info.current_dir(target)
        .arg("info")
        .arg("--show-item")
        .arg("revision");
    Ok(run_cmd(info, "svn info revision")?.trim().to_string())
}

fn svn_add_auth(cmd: &mut Command, b: &VcsBinding) {
    cmd.arg("--non-interactive");
    if !b.username.is_empty() {
        cmd.arg("--username").arg(&b.username);
    }
    if !b.password.is_empty() {
        cmd.arg("--password").arg(&b.password);
    }
}

fn svn_update(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    let mut up = vcs_command("svn");
    up.current_dir(target).arg("update");
    svn_add_auth(&mut up, b);
    run_cmd(up, "svn update")
}

fn svn_checkout_or_export(target: &Path, b: &VcsBinding) -> anyhow::Result<VcsPullResult> {
    if target.exists() && !target.join(".svn").exists() {
        fs::remove_dir_all(target)?;
    }
    match svn_checkout(target, b) {
        Ok(()) => {
            let _ = fs::remove_file(svn_export_marker(target));
            svn_working_copy_revision(target).map(VcsPullResult::full)
        }
        Err(e) => {
            let msg = e.to_string();
            if !msg.contains("E155") {
                return Err(e);
            }
            eprintln!("[ktree] svn checkout 遇到 Windows 非法路径({e}),改用稀疏工作副本");
            svn_sparse_checkout_valid_paths(target, b).map(VcsPullResult::full)
        }
    }
}

fn svn_checkout(target: &Path, b: &VcsBinding) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut co = vcs_command("svn");
    co.arg("checkout")
        .arg(&b.url)
        .arg(target);
    svn_add_auth(&mut co, b);
    run_cmd(co, "svn checkout")?;
    Ok(())
}

fn svn_export_marker(target: &Path) -> PathBuf {
    target.join(".ktree-svn-export-fallback")
}

fn svn_remote_revision(b: &VcsBinding) -> anyhow::Result<String> {
    let mut info = vcs_command("svn");
    info.arg("info")
        .arg("--show-item")
        .arg("revision")
        .arg(&b.url);
    svn_add_auth(&mut info, b);
    Ok(run_cmd(info, "svn info revision")?.trim().to_string())
}

fn svn_list_recursive_entries(b: &VcsBinding) -> anyhow::Result<Vec<SvnListEntry>> {
    let mut ls = vcs_command("svn");
    ls.arg("list").arg("--xml").arg("-R").arg(&b.url);
    svn_add_auth(&mut ls, b);
    parse_svn_list_xml_entries(&run_cmd(ls, "svn list --xml")?)
}

fn parse_svn_update_changes(output: &str) -> (HashSet<String>, HashSet<String>) {
    let mut changed = HashSet::new();
    let mut deleted = HashSet::new();
    for line in output.lines() {
        let prefix: String = line.chars().take(4).collect();
        if prefix.is_empty()
            || !prefix
                .chars()
                .all(|c| matches!(c, ' ' | 'A' | 'B' | 'C' | 'D' | 'E' | 'G' | 'R' | 'U'))
        {
            continue;
        }
        let Some(status) = prefix.chars().find(|c| *c != ' ') else {
            continue;
        };
        let raw_path: String = line.chars().skip(4).collect();
        let Some(path) = normalize_svn_update_path(&raw_path) else {
            continue;
        };
        if status == 'D' {
            deleted.insert(path);
        } else {
            changed.insert(path);
        }
    }
    (changed, deleted)
}

fn normalize_svn_update_path(raw: &str) -> Option<String> {
    let mut s = raw.trim().trim_matches('"').trim_matches('\'').replace('\\', "/");
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    let s = s.trim_matches('/').to_string();
    if s.is_empty() || s == "." {
        return None;
    }
    Some(s)
}

fn parse_svn_list_xml_entries(xml: &str) -> anyhow::Result<Vec<SvnListEntry>> {
    let mut entries = Vec::new();
    let mut rest = xml;
    while let Some(entry_start) = rest.find("<entry") {
        rest = &rest[entry_start..];
        let Some(entry_end) = rest.find("</entry>") else {
            anyhow::bail!("svn list --xml 输出不完整:缺少 </entry>");
        };
        let entry = &rest[..entry_end + "</entry>".len()];
        let kind = if entry.contains("kind=\"file\"") {
            Some(SvnEntryKind::File)
        } else if entry.contains("kind=\"dir\"") {
            Some(SvnEntryKind::Dir)
        } else {
            None
        };
        if let Some(kind) = kind {
            let name_start = entry
                .find("<name>")
                .ok_or_else(|| anyhow::anyhow!("svn list --xml 输出不完整:缺少 <name>"))?
                + "<name>".len();
            let name_end = entry[name_start..]
                .find("</name>")
                .ok_or_else(|| anyhow::anyhow!("svn list --xml 输出不完整:缺少 </name>"))?
                + name_start;
            let rel = xml_unescape(&entry[name_start..name_end])?;
            if !rel.is_empty() {
                entries.push(SvnListEntry { rel, kind });
            }
        }
        rest = &rest[entry_end + "</entry>".len()..];
    }
    Ok(entries)
}

fn xml_unescape(s: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos + 1..];
        let Some(end) = rest.find(';') else {
            anyhow::bail!("XML 转义不完整");
        };
        let entity = &rest[..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if entity.starts_with("#x") => {
                let code = u32::from_str_radix(&entity[2..], 16)
                    .map_err(|_| anyhow::anyhow!("非法 XML 字符引用: &{entity};"))?;
                out.push(
                    char::from_u32(code)
                        .ok_or_else(|| anyhow::anyhow!("非法 XML 字符引用: &{entity};"))?,
                );
            }
            _ if entity.starts_with('#') => {
                let code = entity[1..]
                    .parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("非法 XML 字符引用: &{entity};"))?;
                out.push(
                    char::from_u32(code)
                        .ok_or_else(|| anyhow::anyhow!("非法 XML 字符引用: &{entity};"))?,
                );
            }
            _ => anyhow::bail!("未知 XML 实体: &{entity};"),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn windows_reserved_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return true;
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return true;
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || c.is_control())
    {
        return true;
    }
    let base = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
}

fn svn_export_rel_path(rel: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        if windows_reserved_component(part) {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

/// 路径任一组件以 '.' 开头(隐藏文件/目录,如 .claude/.obsidian)。
///
/// `list_repo_files` 扫描本地镜像时跳过隐藏路径,远端枚举、变更集必须用同一口径:
/// 否则远端点文件每轮都被判成"本地缺失"→ 重复拉取入库,而审计的期望集合又不含
/// 它们 → store 记录每轮被判残留 → 每轮都触发全库对账,同步永远做不完。
fn path_has_hidden_component(rel: &str) -> bool {
    rel.split('/')
        .filter(|p| !p.is_empty())
        .any(|p| p.starts_with('.'))
}

fn split_svn_entries_for_windows(
    entries: Vec<SvnListEntry>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries {
        // 隐藏路径不属于镜像范围(与 list_repo_files 口径一致),静默略过
        if path_has_hidden_component(&entry.rel) {
            continue;
        }
        if svn_export_rel_path(&entry.rel).is_none() {
            skipped.push(entry.rel);
            continue;
        }
        match entry.kind {
            SvnEntryKind::Dir => dirs.push(entry.rel),
            SvnEntryKind::File => files.push(entry.rel),
        }
    }
    dirs.sort();
    files.sort();
    skipped.sort();
    (dirs, files, skipped)
}

fn path_depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

fn svn_update_targets(
    target: &Path,
    b: &VcsBinding,
    targets: &[String],
    set_depth: Option<&str>,
    ctx: &str,
) -> anyhow::Result<String> {
    if targets.is_empty() {
        return Ok(String::new());
    }
    let mut combined = String::new();
    let mut batch = Vec::new();
    let mut batch_len = 0usize;
    for t in targets {
        let next_len = batch_len + t.len() + 8;
        if !batch.is_empty() && (batch.len() >= 200 || next_len > 12_000) {
            combined.push_str(&svn_update_target_batch(target, b, &batch, set_depth, ctx)?);
            batch.clear();
            batch_len = 0;
        }
        batch.push(t.clone());
        batch_len += t.len() + 8;
    }
    if !batch.is_empty() {
        combined.push_str(&svn_update_target_batch(target, b, &batch, set_depth, ctx)?);
    }
    Ok(combined)
}

fn svn_update_target_batch(
    target: &Path,
    b: &VcsBinding,
    targets: &[String],
    set_depth: Option<&str>,
    ctx: &str,
) -> anyhow::Result<String> {
    let mut up = vcs_command("svn");
    up.current_dir(target).arg("update");
    svn_add_auth(&mut up, b);
    if let Some(depth) = set_depth {
        up.arg("--set-depth").arg(depth);
    }
    for t in targets {
        up.arg(t);
    }
    run_cmd(up, ctx)
}

fn svn_update_dirs_by_depth(
    target: &Path,
    b: &VcsBinding,
    dirs: &[String],
) -> anyhow::Result<String> {
    let mut dirs = dirs.to_vec();
    dirs.sort_by(|a, b| path_depth(a).cmp(&path_depth(b)).then_with(|| a.cmp(b)));
    let mut combined = String::new();
    let mut start = 0usize;
    while start < dirs.len() {
        let depth = path_depth(&dirs[start]);
        let mut end = start + 1;
        while end < dirs.len() && path_depth(&dirs[end]) == depth {
            end += 1;
        }
        combined.push_str(&svn_update_targets(
            target,
            b,
            &dirs[start..end],
            Some("empty"),
            "svn update sparse dirs",
        )?);
        start = end;
    }
    Ok(combined)
}

fn ancestor_dirs_for_files(files: &HashSet<String>, valid_dirs: &HashSet<String>) -> Vec<String> {
    let mut dirs = HashSet::new();
    for file in files {
        let mut parts: Vec<&str> = file.split('/').collect();
        parts.pop();
        while !parts.is_empty() {
            let dir = parts.join("/");
            if valid_dirs.contains(&dir) {
                dirs.insert(dir);
            }
            parts.pop();
        }
    }
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by(|a, b| path_depth(a).cmp(&path_depth(b)).then_with(|| a.cmp(b)));
    dirs
}

fn write_svn_sparse_marker(target: &Path, revision: &str, skipped: &[String]) -> anyhow::Result<()> {
    let marker = format!(
        "SVN sparse working copy fallback.\nRevision: {revision}\nSkipped Windows-invalid paths:\n{}\n",
        skipped.join("\n")
    );
    fs::write(svn_export_marker(target), marker)?;
    if !skipped.is_empty() {
        eprintln!("[ktree] svn 跳过 Windows 非法路径: {}", skipped.join(", "));
    }
    Ok(())
}

fn svn_sparse_checkout_valid_paths(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    let revision = svn_remote_revision(b)?;
    let _ = fs::remove_dir_all(target);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut checkout = vcs_command("svn");
    checkout
        .arg("checkout")
        .arg("--depth")
        .arg("empty")
        .arg(&b.url)
        .arg(target);
    svn_add_auth(&mut checkout, b);
    run_cmd(checkout, "svn checkout --depth empty")?;

    let (dirs, files, skipped) = split_svn_entries_for_windows(svn_list_recursive_entries(b)?);
    svn_update_dirs_by_depth(target, b, &dirs)?;
    svn_update_targets(target, b, &files, None, "svn update sparse files")?;
    write_svn_sparse_marker(target, &revision, &skipped)?;
    Ok(revision)
}

fn svn_refresh_sparse_workcopy(
    target: &Path,
    b: &VcsBinding,
    revision: &str,
    mut changed: HashSet<String>,
    mut deleted: HashSet<String>,
) -> anyhow::Result<(HashSet<String>, HashSet<String>)> {
    let (dirs, files, skipped) = split_svn_entries_for_windows(svn_list_recursive_entries(b)?);
    let remote_files: HashSet<String> = files.into_iter().collect();
    let valid_dirs: HashSet<String> = dirs.into_iter().collect();
    let local_files: HashSet<String> = list_repo_files(target)?.into_iter().collect();

    // 本地有、远端没有 → 上游删除的残留。必须先 --set-depth exclude 从工作副本
    // 反注册再删:仍登记在工作副本里的文件若只做文件系统删除,下一次 `svn update`
    // 会按"丢失文件"从 pristine 原样恢复,和这里的清理形成删除/恢复的无限振荡。
    let stale: Vec<String> = local_files.difference(&remote_files).cloned().collect();
    for rel in &stale {
        if let Some(path) = safe_vcs_rel_path(rel) {
            let mut ex = vcs_command("svn");
            ex.current_dir(target)
                .arg("update")
                .arg("--set-depth")
                .arg("exclude");
            svn_add_auth(&mut ex, b);
            ex.arg(rel);
            let _ = run_cmd(ex, "svn exclude stale");
            let p = target.join(path);
            let _ = fs::remove_file(&p);
            deleted.insert(rel.clone());
        }
    }

    let missing: HashSet<String> = remote_files.difference(&local_files).cloned().collect();
    // 只物化盘上尚不存在的目录。对已有内容的目录执行 --set-depth empty 会把
    // 整棵子树从工作副本清掉(仅按 missing 名单拉回部分文件):上游在大目录下
    // 新增一个文件,就会引发整个子树被清空重拉 → 同步永远在全量重灌。
    let dirs_to_update: Vec<String> = ancestor_dirs_for_files(&missing, &valid_dirs)
        .into_iter()
        .filter(|d| {
            safe_vcs_rel_path(d)
                .map(|p| !target.join(p).exists())
                .unwrap_or(false)
        })
        .collect();
    svn_update_dirs_by_depth(target, b, &dirs_to_update)?;
    let mut missing_files: Vec<String> = missing.iter().cloned().collect();
    missing_files.sort();
    let out = svn_update_targets(
        target,
        b,
        &missing_files,
        None,
        "svn update sparse missing files",
    )?;
    let (more_changed, more_deleted) = parse_svn_update_changes(&out);
    changed.extend(missing);
    changed.extend(more_changed);
    deleted.extend(more_deleted);
    write_svn_sparse_marker(target, revision, &skipped)?;
    Ok((changed, deleted))
}

/// 删除 svn 工作副本里所有 unversioned 的文件 / 目录(`svn status` 的 '?' 行)。
fn svn_clean_unversioned(target: &Path) {
    let mut status = vcs_command("svn");
    status.current_dir(target).arg("status");
    let Ok(out) = run_cmd(status, "svn status") else {
        return;
    };
    for line in out.lines() {
        if let Some(rel) = line.strip_prefix('?') {
            let rel = rel.trim();
            if rel.is_empty() {
                continue;
            }
            if rel == ".ktree-svn-export-fallback" {
                continue;
            }
            let p = target.join(rel);
            if p.is_dir() {
                let _ = fs::remove_dir_all(&p);
            } else {
                let _ = fs::remove_file(&p);
            }
        }
    }
}

/// 递归列出 `base/` 下所有文件,过滤掉 VCS 元数据目录与隐藏文件。
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

fn safe_vcs_rel_path(rel: &str) -> Option<PathBuf> {
    let rel = normalize_svn_update_path(rel)?;
    let path = Path::new(&rel);
    if path.is_absolute() {
        return None;
    }
    if path.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(PathBuf::from(rel))
}

fn expand_changed_repo_files(
    target: &Path,
    changed: &HashSet<String>,
) -> anyhow::Result<HashSet<String>> {
    let mut out = HashSet::new();
    for rel in changed {
        // svn update 输出里可能出现 .claude 等隐藏路径,不属于镜像范围
        if path_has_hidden_component(rel) {
            continue;
        }
        let Some(rel_path) = safe_vcs_rel_path(rel) else {
            continue;
        };
        let p = target.join(&rel_path);
        if p.is_dir() {
            let base = rel.trim_end_matches('/');
            for child in list_repo_files(&p)? {
                out.insert(format!("{base}/{child}"));
            }
        } else if p.is_file() {
            out.insert(rel.clone());
        }
    }
    Ok(out)
}

fn collect_deleted_rel_paths(
    state: &AppState,
    kb: &KnowledgeBase,
    prefix: &str,
    deleted: &HashSet<String>,
) -> anyhow::Result<HashSet<String>> {
    let mut out = HashSet::new();
    for rel in deleted {
        let Some(_) = safe_vcs_rel_path(rel) else {
            continue;
        };
        let rel_path = format!("{}/{}", prefix.trim_end_matches('/'), rel.trim_matches('/'));
        if state.store.get_by_path(&kb.id, &rel_path)?.is_some() {
            out.insert(rel_path.clone());
        }
        for doc in state.store.list_documents(&kb.id, Some(&rel_path))? {
            out.insert(doc.rel_path);
        }
    }
    Ok(out)
}

fn ingest_vcs_rel_path(
    state: &AppState,
    kb: &KnowledgeBase,
    rel_path: &str,
    report: &mut VcsSyncReport,
    manifest: &mut kbmeta::Manifest,
    manifest_dirty: &mut bool,
) {
    if ingest::path_has_ignored_component(rel_path) {
        match ingest::forget_path_artifacts_with_manifest(state, kb, rel_path, manifest, manifest_dirty) {
            Ok(true) => {
                report.deleted.push(rel_path.to_string());
                report
                    .messages
                    .push(format!("按忽略规则移出索引「{rel_path}」"));
            }
            Ok(false) => {}
            Err(e) => {
                report.failed.push(rel_path.to_string());
                report.messages.push(format!("清理忽略文档失败「{rel_path}」: {e}"));
            }
        }
        return;
    }

    let old_doc = state.store.get_by_path(&kb.id, rel_path).ok().flatten();
    let old_md5 = old_doc.as_ref().map(|d| d.md5.as_str());
    let old_output_ok = old_doc
        .as_ref()
        .and_then(|d| d.md_path.as_deref())
        .map(|md| ingest::docs_artifact_present(&kb.root, md))
        .unwrap_or(false);
    match ingest::ingest_file_with_manifest(
        state, kb, rel_path, "vcs", true, false, manifest, manifest_dirty,
    ) {
        Ok(doc) => {
            if old_doc.is_none() {
                report.added.push(rel_path.to_string());
            } else if old_md5 != Some(doc.md5.as_str()) || !old_output_ok {
                report.updated.push(rel_path.to_string());
            }
        }
        Err(e) => {
            report.failed.push(rel_path.to_string());
            report.messages.push(format!("入库失败「{rel_path}」: {e}"));
        }
    }
}

fn delete_vcs_rel_path(
    state: &AppState,
    kb: &KnowledgeBase,
    rel_path: &str,
    report: &mut VcsSyncReport,
    manifest: &mut kbmeta::Manifest,
    manifest_dirty: &mut bool,
) {
    if let Ok(Some(doc)) = state.store.get_by_path(&kb.id, rel_path) {
        // 隐藏路径(.claude/.obsidian…)不属于索引范围,但仍是仓库里合法的版本化文件:
        // 只清索引产物、保留 src 原件。否则会把它们从工作副本删掉、下轮又被
        // svn update 恢复,来回空转。真正被上游删除的文件才连 src 一起清。
        let src_exists = kb.root.join("src").join(rel_path).exists();
        let keep_source = src_exists && path_has_hidden_component(rel_path);
        let res = if keep_source {
            ingest::forget_doc_artifacts_with_manifest(state, kb, &doc, manifest, manifest_dirty)
        } else {
            ingest::delete_doc_with_manifest(state, kb, &doc, manifest, manifest_dirty)
        };
        if let Err(e) = res {
            report.failed.push(rel_path.to_string());
            report.messages.push(format!("清理失败「{rel_path}」: {e}"));
        } else {
            report.deleted.push(rel_path.to_string());
        }
    }
}

/// 批量入库/清理过程中每积累这么多次 manifest 变更就落盘一次:
/// 全库对账可能长达几十分钟,中途崩溃时已完成的工作不至于全部作废重来。
const MANIFEST_FLUSH_EVERY: usize = 500;

fn reconcile_full(
    state: &AppState,
    kb: &KnowledgeBase,
    target: &Path,
    prefix: &str,
    binding_idx: usize,
    manifest: &mut kbmeta::Manifest,
    report: &mut VcsSyncReport,
) -> anyhow::Result<()> {
    let before: HashSet<String> = state
        .store
        .list_documents(&kb.id, Some(prefix))?
        .into_iter()
        .map(|d| d.rel_path)
        .collect();
    let files = list_repo_files(target)?;
    let after: HashSet<String> = files.iter().map(|r| format!("{prefix}/{r}")).collect();

    let total = after.len();
    let mut dirty_ops = 0usize;
    let mut flushed_at = 0usize;
    for (i, rel_path) in after.iter().enumerate() {
        if i % 100 == 0 {
            state.set_sync_progress(
                "vcs",
                &kb.id,
                binding_idx,
                &format!("全库对账:核对入库 {i}/{total}"),
            );
        }
        let mut dirty = false;
        ingest_vcs_rel_path(state, kb, rel_path, report, manifest, &mut dirty);
        if dirty {
            dirty_ops += 1;
        }
        if dirty_ops - flushed_at >= MANIFEST_FLUSH_EVERY {
            kbmeta::save_manifest(&kb.root, manifest)?;
            flushed_at = dirty_ops;
        }
    }

    let stale: Vec<&String> = before.difference(&after).collect();
    let total_del = stale.len();
    for (i, rel_path) in stale.into_iter().enumerate() {
        if i % 100 == 0 {
            state.set_sync_progress(
                "vcs",
                &kb.id,
                binding_idx,
                &format!("全库对账:清理残留 {i}/{total_del}"),
            );
        }
        let mut dirty = false;
        delete_vcs_rel_path(state, kb, rel_path, report, manifest, &mut dirty);
        if dirty {
            dirty_ops += 1;
        }
        if dirty_ops - flushed_at >= MANIFEST_FLUSH_EVERY {
            kbmeta::save_manifest(&kb.root, manifest)?;
            flushed_at = dirty_ops;
        }
    }

    // docs 是 VCS 的严格镜像;清理失败必须让本次同步失败并在 UI 中可见,
    // 不能静默留下点开后报“文件不存在”的断链目录。
    state.set_sync_progress("vcs", &kb.id, binding_idx, "全库对账:清理 docs 孤儿产物…");
    ingest::prune_docs_orphans(state, kb, prefix)?;
    Ok(())
}

fn reconcile_incremental(
    state: &AppState,
    kb: &KnowledgeBase,
    target: &Path,
    prefix: &str,
    binding_idx: usize,
    changed: &HashSet<String>,
    deleted: &HashSet<String>,
    manifest: &mut kbmeta::Manifest,
    report: &mut VcsSyncReport,
) -> anyhow::Result<()> {
    let to_ingest = expand_changed_repo_files(target, changed)?;
    let total = to_ingest.len();
    let mut dirty_ops = 0usize;
    let mut flushed_at = 0usize;
    for (i, rel) in to_ingest.iter().enumerate() {
        if i % 100 == 0 {
            state.set_sync_progress(
                "vcs",
                &kb.id,
                binding_idx,
                &format!("增量入库 {i}/{total}"),
            );
        }
        let mut dirty = false;
        ingest_vcs_rel_path(state, kb, &format!("{prefix}/{rel}"), report, manifest, &mut dirty);
        if dirty {
            dirty_ops += 1;
        }
        if dirty_ops - flushed_at >= MANIFEST_FLUSH_EVERY {
            kbmeta::save_manifest(&kb.root, manifest)?;
            flushed_at = dirty_ops;
        }
    }
    for rel_path in collect_deleted_rel_paths(state, kb, prefix, deleted)? {
        let mut dirty = false;
        delete_vcs_rel_path(state, kb, &rel_path, report, manifest, &mut dirty);
        if dirty {
            dirty_ops += 1;
        }
        if dirty_ops - flushed_at >= MANIFEST_FLUSH_EVERY {
            kbmeta::save_manifest(&kb.root, manifest)?;
            flushed_at = dirty_ops;
        }
    }
    Ok(())
}

/// SVN 增量同步后的逐文件一致性审计。
///
/// 过去只比较 src / docs 文件总数,当“漏一个新产物 + 留一个旧产物”时数量会抵消,
/// 仍会错误地判定同步完整。这里以 src 为真相,逐项核对 store、manifest、内容 md5
/// 和 docs 主产物,也检查 store 中是否残留已从 src 消失的记录。
#[derive(Debug, Default, Eq, PartialEq)]
struct ReconcileAudit {
    source_files: usize,
    stored_docs: usize,
    missing_store: Vec<String>,
    stale_store: Vec<String>,
    stale_content: Vec<String>,
    missing_outputs: Vec<String>,
    manifest_mismatches: Vec<String>,
    unreadable_sources: Vec<String>,
}

impl ReconcileAudit {
    fn is_consistent(&self) -> bool {
        self.missing_store.is_empty()
            && self.stale_store.is_empty()
            && self.stale_content.is_empty()
            && self.missing_outputs.is_empty()
            && self.manifest_mismatches.is_empty()
            && self.unreadable_sources.is_empty()
    }

    fn summary(&self) -> String {
        format!(
            "src {}, store {}; 缺记录 {}, 多余记录 {}, 内容失步 {}, 缺产物 {}, manifest 失步 {}, 源文件不可读 {}",
            self.source_files,
            self.stored_docs,
            self.missing_store.len(),
            self.stale_store.len(),
            self.stale_content.len(),
            self.missing_outputs.len(),
            self.manifest_mismatches.len(),
            self.unreadable_sources.len(),
        )
    }
}

fn audit_reconcile_state(
    store: &crate::store::Store,
    kb: &KnowledgeBase,
    target: &Path,
    prefix: &str,
    manifest: &kbmeta::Manifest,
) -> anyhow::Result<ReconcileAudit> {
    let docs = store.list_documents(&kb.id, Some(prefix))?;
    let stored_by_path: std::collections::HashMap<&str, &crate::store::Document> = docs
        .iter()
        .map(|doc| (doc.rel_path.as_str(), doc))
        .collect();

    let mut audit = ReconcileAudit {
        stored_docs: docs.len(),
        ..ReconcileAudit::default()
    };
    let mut expected = HashSet::new();

    for repo_rel in list_repo_files(target)? {
        let rel_path = format!("{prefix}/{repo_rel}");
        if ingest::path_has_ignored_component(&rel_path) {
            continue;
        }
        audit.source_files += 1;
        expected.insert(rel_path.clone());

        let Some(doc) = stored_by_path.get(rel_path.as_str()).copied() else {
            audit.missing_store.push(rel_path);
            continue;
        };

        let source_md5 = match fs::read(target.join(&repo_rel)) {
            Ok(bytes) => format!("{:x}", Md5::digest(&bytes)),
            Err(_) => {
                audit.unreadable_sources.push(rel_path);
                continue;
            }
        };
        if doc.md5 != source_md5 {
            audit.stale_content.push(rel_path.clone());
        }

        let output = doc.md_path.as_deref().unwrap_or_default();
        // 存在性判定与 ingest 的短路口径一致(见 docs_artifact_present):
        // 这里若用 is_file(),Windows 上"原样镜像"的产物会被整批判成缺失,
        // 逐文件核对每轮都报不一致 → 每轮触发全库对账重灌。
        if !ingest::docs_artifact_present(&kb.root, output) {
            audit.missing_outputs.push(rel_path.clone());
        }

        let manifest_ok = manifest
            .get(&rel_path)
            .map(|entry| entry.md5 == source_md5 && entry.output == output)
            .unwrap_or(false);
        if !manifest_ok {
            audit.manifest_mismatches.push(rel_path);
        }
    }

    for doc in &docs {
        if !expected.contains(&doc.rel_path) {
            audit.stale_store.push(doc.rel_path.clone());
        }
    }

    Ok(audit)
}

/// 对一个绑定执行一次同步:拉取/更新(严格镜像)→ diff store → 入库 / 删除。
/// 阻塞调用,放在 spawn_blocking 里跑。
fn sync_binding_inner(
    state: &AppState,
    kb: &KnowledgeBase,
    binding_idx: usize,
    force_full_check: bool,
) -> anyhow::Result<VcsSyncReport> {
    let b = kb
        .vcs_bindings
        .get(binding_idx)
        .ok_or_else(|| anyhow::anyhow!("VCS 绑定 idx={binding_idx} 不存在"))?
        .clone();
    let (target, prefix) = binding_target_dir(kb, &b)?;

    let repo_sub = b.repo_sub_path.trim().trim_matches('/').to_string();

    let mut report = VcsSyncReport {
        kb_id: kb.id.clone(),
        binding_idx,
        vcs_type: b.vcs_type.clone(),
        url: b.url.clone(),
        name: b.name.trim().to_string(),
        repo_sub_path: repo_sub.clone(),
        revision: String::new(),
        added: Vec::new(),
        updated: Vec::new(),
        deleted: Vec::new(),
        failed: Vec::new(),
        messages: Vec::new(),
    };

    // 拉取 / 更新(内部已做严格镜像,盘上只剩仓库里的文件)
    state.set_sync_progress("vcs", &kb.id, binding_idx, "拉取远端更新…");
    let pull = match b.vcs_type.as_str() {
        "git" => VcsPullResult::full(git_sync(kb, &target, &b)?),
        "svn" => svn_update_or_checkout(&target, &b)?,
        other => anyhow::bail!("不支持的 VCS 类型「{other}」(只支持 git / svn)"),
    };
    report.revision = pull.revision.clone();

    // manifest 一轮同步只读一次、结束落一次盘(循环内按批落盘防崩溃丢工作);
    // 过去每个文件 load+save 整个 manifest,大库一轮就是几十 GB 的 JSON 序列化。
    let mut manifest = kbmeta::load_manifest(&kb.root);

    // 本进程此前核对通过的修订号(读完即撤销标记:中途出错就不会留下"已核对"的假象,
    // 下一轮会重新核对)。
    let audited_before = state
        .audited_revision
        .lock()
        .ok()
        .and_then(|m| m.get(&(kb.id.clone(), binding_idx)).cloned());
    state.clear_audited(&kb.id, binding_idx);

    match pull.reconcile {
        ReconcileMode::Full => {
            reconcile_full(state, kb, &target, &prefix, binding_idx, &mut manifest, &mut report)?;
        }
        ReconcileMode::Incremental { changed, deleted } => {
            if force_full_check {
                report
                    .messages
                    .push("手动全库检查:比对 src 与 docs/store".to_string());
                reconcile_full(state, kb, &target, &prefix, binding_idx, &mut manifest, &mut report)?;
            } else if changed.is_empty()
                && deleted.is_empty()
                && audited_before.as_deref() == Some(report.revision.as_str())
            {
                // 本进程已在该修订号上核对一致过,且 SVN 没报任何变更 → 真的无事可做。
                // 逐文件核对要通读全库算 md5,定时同步每几分钟一轮,不能每轮全库扫描。
                // 注意这里只认**本进程内**的核对结论:重启后必须重新核对一次,
                // 否则旧版本或异常退出留下的"看着干净"的持久化记录会让漂移永远不自愈。
                report
                    .messages
                    .push("SVN 未返回文件变更(本进程已核对过该修订,跳过全库核对)".to_string());
                state.mark_audited(&kb.id, binding_idx, &report.revision);
            } else {
                report.messages.push(format!(
                    "SVN 增量对账:变更 {} 项,删除 {} 项",
                    changed.len(),
                    deleted.len()
                ));
                reconcile_incremental(
                    state, kb, &target, &prefix, binding_idx, &changed, &deleted, &mut manifest,
                    &mut report,
                )?;
                // 不信任 SVN 增量报告是否完整:逐文件核对 src / store / manifest / docs。
                // 数量相等也可能是一漏一残留互相抵消;内容 md5 还能发现 SVN 漏报修改。
                state.set_sync_progress("vcs", &kb.id, binding_idx, "逐文件核对一致性…");
                let audit = audit_reconcile_state(&state.store, kb, &target, &prefix, &manifest)?;
                if audit.is_consistent() {
                    state.mark_audited(&kb.id, binding_idx, &report.revision);
                } else {
                    report.messages.push(format!(
                        "逐文件核对不一致({}),触发全库对账补漏",
                        audit.summary()
                    ));
                    reconcile_full(
                        state, kb, &target, &prefix, binding_idx, &mut manifest, &mut report,
                    )?;

                    let remaining =
                        audit_reconcile_state(&state.store, kb, &target, &prefix, &manifest)?;
                    if remaining.is_consistent() {
                        state.mark_audited(&kb.id, binding_idx, &report.revision);
                    } else {
                        report.messages.push(format!(
                            "全库补漏后仍有不一致: {}",
                            remaining.summary()
                        ));
                    }
                }
            }
        }
    }

    kbmeta::save_manifest(&kb.root, &manifest)?;

    // 刷新 .ktree 元数据
    state.set_sync_progress("vcs", &kb.id, binding_idx, "刷新知识库元数据…");
    let _ = ingest::refresh_kb_meta(state, kb);

    Ok(report)
}

/// 删除一条 VCS 绑定的本地内容:src/vcs/<name>/ 目录、docs 产物、store / 索引记录、
/// git 稀疏缓存。供「删除绑定」的 HTTP / MCP 接口在删配置后调用。
pub fn purge_binding_data(
    state: &AppState,
    kb: &KnowledgeBase,
    b: &VcsBinding,
) -> anyhow::Result<usize> {
    let (_, prefix) = binding_target_dir(kb, b)?;
    let removed = ingest::delete_folder(state, kb, &prefix)?;
    // 清掉稀疏检出缓存
    let _ = fs::remove_dir_all(git_cache_dir(kb, b));
    let _ = ingest::refresh_kb_meta(state, kb);
    Ok(removed)
}

/// 调 `sync_binding`,无论成败都把结果记到 `state.last_vcs_sync`,供 webui 展示。
///
/// `source`:"auto"(scheduler 定时)或 "manual"(用户/REST 触发)。
/// 这是定时与手动同步的统一收口;直接调 `sync_binding` 不会更新 last_sync,
/// 所以新增调用方必须走这个函数。
pub fn sync_binding_with_record(
    state: &AppState,
    kb: &KnowledgeBase,
    binding_idx: usize,
    source: &str,
) -> anyhow::Result<VcsSyncReport> {
    run_binding_with_record(state, kb, binding_idx, source, false)
}

pub fn check_binding_full_with_record(
    state: &AppState,
    kb: &KnowledgeBase,
    binding_idx: usize,
    source: &str,
) -> anyhow::Result<VcsSyncReport> {
    run_binding_with_record(state, kb, binding_idx, source, true)
}

fn run_binding_with_record(
    state: &AppState,
    kb: &KnowledgeBase,
    binding_idx: usize,
    source: &str,
    force_full_check: bool,
) -> anyhow::Result<VcsSyncReport> {
    // 并发保护:同一绑定同时只允许一个同步在跑(并发 git/svn 进程会互相打架、留锁)
    if !state.try_begin_sync("vcs", &kb.id, binding_idx) {
        anyhow::bail!("该绑定正在同步中,请等当前同步结束");
    }
    // 用 Drop 释放:同步体 panic 时若直接跳过 end_sync,该绑定会被永久标记成
    // "同步中",此后每次同步都被并发保护挡掉 —— 只能重启进程才能恢复。
    struct SyncGuard<'a>(&'a AppState, &'a str, usize);
    impl Drop for SyncGuard<'_> {
        fn drop(&mut self) {
            self.0.end_sync("vcs", self.1, self.2);
        }
    }
    let _guard = SyncGuard(state, &kb.id, binding_idx);

    let result = sync_binding_inner(state, kb, binding_idx, force_full_check);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let entry = match &result {
        Ok(r) => LastVcsSync {
            at_unix_ms: now_ms,
            source: source.to_string(),
            ok: true,
            revision: r.revision.clone(),
            added: r.added.len(),
            updated: r.updated.len(),
            deleted: r.deleted.len(),
            failed: r.failed.len(),
            error: None,
            note: (!r.messages.is_empty()).then(|| r.messages.join(" | ")),
        },
        Err(e) => LastVcsSync {
            at_unix_ms: now_ms,
            source: source.to_string(),
            ok: false,
            revision: String::new(),
            added: 0,
            updated: 0,
            deleted: 0,
            failed: 0,
            error: Some(e.to_string()),
            note: None,
        },
    };
    // 写内存 map + 持久化到 SQLite(重启后 webui 仍能显示最近同步时间)
    state.record_sync("vcs", &kb.id, binding_idx, entry);
    result
}

/// 同步一个 KB 下所有 VCS 绑定,每条绑定都走 `sync_binding_with_record`。
pub fn sync_kb_all_with_record(
    state: &AppState,
    kb: &KnowledgeBase,
    source: &str,
) -> Vec<anyhow::Result<VcsSyncReport>> {
    (0..kb.vcs_bindings.len())
        .map(|i| sync_binding_with_record(state, kb, i, source))
        .collect()
}

pub fn check_kb_all_full_with_record(
    state: &AppState,
    kb: &KnowledgeBase,
    source: &str,
) -> Vec<anyhow::Result<VcsSyncReport>> {
    (0..kb.vcs_bindings.len())
        .map(|i| check_binding_full_with_record(state, kb, i, source))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir() -> std::path::PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ktree_vcs_{}_{}", std::process::id(), n))
    }

    #[test]
    fn audit_detects_equal_counts_with_different_paths() {
        let root = tmp_dir();
        let prefix = "vcs/demo";
        let target = root.join("src").join(prefix);
        std::fs::create_dir_all(target.join("new-dir")).unwrap();
        std::fs::create_dir_all(root.join("docs").join(prefix).join("old-dir")).unwrap();
        std::fs::write(target.join("new-dir/missed.txt"), "new").unwrap();
        std::fs::write(
            root.join("docs").join(prefix).join("old-dir/stale.txt"),
            "old",
        )
        .unwrap();

        let store = crate::store::Store::open(&root.join("test.db")).unwrap();
        store
            .upsert_document(&crate::store::NewDocument {
                kb_id: "kb".to_string(),
                rel_path: format!("{prefix}/old-dir/stale.txt"),
                title: "stale".to_string(),
                ext: "txt".to_string(),
                size: 3,
                md5: format!("{:x}", Md5::digest(b"old")),
                summary: String::new(),
                tags: String::new(),
                props: String::new(),
                md_path: Some(format!("docs/{prefix}/old-dir/stale.txt")),
                source: "vcs".to_string(),
            })
            .unwrap();

        // 旧实现只看数量:src=1、docs=1,会误判正常。逐文件审计必须同时发现漏项与残留。
        let kb = KnowledgeBase {
            id: "kb".to_string(),
            name: "kb".to_string(),
            root: root.clone(),
            vcs_bindings: Vec::new(),
            cloud_bindings: Vec::new(),
        };
        let audit =
            audit_reconcile_state(&store, &kb, &target, prefix, &kbmeta::Manifest::new()).unwrap();
        assert_eq!(audit.source_files, 1);
        assert_eq!(audit.stored_docs, 1);
        assert_eq!(audit.missing_store, vec![format!("{prefix}/new-dir/missed.txt")]);
        assert_eq!(audit.stale_store, vec![format!("{prefix}/old-dir/stale.txt")]);
        assert!(!audit.is_consistent());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn audit_detects_whole_missing_directory() {
        let root = tmp_dir();
        let prefix = "vcs/demo";
        let target = root.join("src").join(prefix);
        std::fs::create_dir_all(target.join("missed-dir/sub-dir")).unwrap();
        std::fs::write(target.join("missed-dir/a.docx"), "a").unwrap();
        std::fs::write(target.join("missed-dir/sub-dir/b.xlsx"), "b").unwrap();

        let store = crate::store::Store::open(&root.join("test.db")).unwrap();
        let kb = KnowledgeBase {
            id: "kb".to_string(),
            name: "kb".to_string(),
            root: root.clone(),
            vcs_bindings: Vec::new(),
            cloud_bindings: Vec::new(),
        };

        let mut audit =
            audit_reconcile_state(&store, &kb, &target, prefix, &kbmeta::Manifest::new()).unwrap();
        audit.missing_store.sort();
        assert_eq!(audit.source_files, 2);
        assert_eq!(audit.stored_docs, 0);
        assert_eq!(
            audit.missing_store,
            vec![
                format!("{prefix}/missed-dir/a.docx"),
                format!("{prefix}/missed-dir/sub-dir/b.xlsx"),
            ]
        );
        assert!(!audit.is_consistent());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_drops_hidden_and_reserved_paths() {
        // 隐藏路径要与 list_repo_files 口径一致地静默排除;Windows 保留名进 skipped。
        // 否则远端点文件每轮被判"本地缺失"反复拉取入库,审计永远不一致 → 同步死循环。
        let entries = vec![
            SvnListEntry {
                rel: ".claude/agents/x.md".into(),
                kind: SvnEntryKind::File,
            },
            SvnListEntry {
                rel: ".obsidian".into(),
                kind: SvnEntryKind::Dir,
            },
            SvnListEntry {
                rel: "docs/a.md".into(),
                kind: SvnEntryKind::File,
            },
            SvnListEntry {
                rel: "docs".into(),
                kind: SvnEntryKind::Dir,
            },
            SvnListEntry {
                rel: "scripts/nul".into(),
                kind: SvnEntryKind::File,
            },
        ];
        let (dirs, files, skipped) = split_svn_entries_for_windows(entries);
        assert_eq!(dirs, vec!["docs".to_string()]);
        assert_eq!(files, vec!["docs/a.md".to_string()]);
        assert_eq!(skipped, vec!["scripts/nul".to_string()]);
    }

    #[test]
    fn expand_changed_skips_hidden_paths() {
        let root = tmp_dir();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(root.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(root.join("a.md"), "hi").unwrap();

        let changed: HashSet<String> =
            [".claude/settings.json".to_string(), "a.md".to_string()].into();
        let out = expand_changed_repo_files(&root, &changed).unwrap();
        assert_eq!(out, ["a.md".to_string()].into());

        let _ = std::fs::remove_dir_all(&root);
    }
}
