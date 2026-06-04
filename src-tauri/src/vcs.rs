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

use crate::config::{KnowledgeBase, VcsBinding, AREA_VCS};
use crate::ingest::{self, safe_component};
use crate::state::{AppState, LastVcsSync};

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

/// 校验绑定名并返回 (目标绝对路径, 相对 src 的前缀 "vcs/<name>")。
fn binding_target_dir(kb: &KnowledgeBase, b: &VcsBinding) -> anyhow::Result<(PathBuf, String)> {
    let name = safe_component(&b.name)
        .ok_or_else(|| anyhow::anyhow!("VCS 绑定名「{}」不合法", b.name))?;
    let prefix = format!("{AREA_VCS}/{name}");
    let target = kb.root.join("src").join(&prefix);
    Ok((target, prefix))
}

fn vcs_command(program: &str) -> Command {
    Command::new(resolve_vcs_program(program))
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

fn run_cmd_bytes(mut cmd: Command, ctx: &str) -> anyhow::Result<Vec<u8>> {
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("{ctx}:启动失败: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{ctx} 失败: {stderr}");
    }
    Ok(out.stdout)
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

fn svn_update_or_checkout(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    if target.join(".svn").exists() {
        // 上次 checkout / update 中断会留下锁(E155004),先 cleanup 再 update。
        // cleanup 自身失败不管,让 update 报真实错误。
        let mut cleanup = vcs_command("svn");
        cleanup
            .current_dir(target)
            .arg("cleanup")
            .arg("--non-interactive");
        let _ = run_cmd(cleanup, "svn cleanup");

        if let Err(e) = svn_update(target, b) {
            // 只有"工作副本损坏"类错误(E155xxx,如锁损坏、wc.db 坏)才清掉重建;
            // 远端错误(E170000 URL 不存在、E175xxx 网络不通、认证失败等)直接报错,
            // 保留本地镜像 —— 删了也下载不回来,反而丢数据。
            let msg = e.to_string();
            if msg.contains("E155") {
                eprintln!("[ktree] svn 工作副本损坏({e}),清掉重新 checkout");
                fs::remove_dir_all(target)?;
                svn_checkout_or_export(target, b)?;
            } else {
                return Err(e);
            }
        } else {
            // 严格镜像:清掉 unversioned 文件(svn status 第一列 '?')
            svn_clean_unversioned(target);
        }
    } else if svn_export_marker(target).exists() {
        let remote_revision = svn_remote_revision(b)?;
        if svn_export_marker_revision(target).as_deref() != Some(remote_revision.as_str()) {
            svn_export_without_workcopy(target, b)?;
        }
        return Ok(remote_revision);
    } else {
        svn_checkout_or_export(target, b)?;
    }
    if !target.join(".svn").exists() {
        return svn_remote_revision(b);
    }
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

fn svn_update(target: &Path, b: &VcsBinding) -> anyhow::Result<()> {
    let mut up = vcs_command("svn");
    up.current_dir(target).arg("update");
    svn_add_auth(&mut up, b);
    run_cmd(up, "svn update")?;
    Ok(())
}

fn svn_checkout_or_export(target: &Path, b: &VcsBinding) -> anyhow::Result<()> {
    if target.exists() && !target.join(".svn").exists() {
        fs::remove_dir_all(target)?;
    }
    match svn_checkout(target, b) {
        Ok(()) => {
            let _ = fs::remove_file(svn_export_marker(target));
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            if !msg.contains("E155") {
                return Err(e);
            }
            eprintln!("[ktree] svn checkout 遇到 Windows 非法路径({e}),改用无工作副本导出");
            svn_export_without_workcopy(target, b).map(|_| ())
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

fn svn_export_marker_revision(target: &Path) -> Option<String> {
    let marker = fs::read_to_string(svn_export_marker(target)).ok()?;
    marker
        .lines()
        .find_map(|line| line.strip_prefix("Revision:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
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

fn svn_list_recursive_files(b: &VcsBinding) -> anyhow::Result<Vec<String>> {
    let mut ls = vcs_command("svn");
    ls.arg("list").arg("--xml").arg("-R").arg(&b.url);
    svn_add_auth(&mut ls, b);
    parse_svn_list_xml_files(&run_cmd(ls, "svn list --xml")?)
}

fn parse_svn_list_xml_files(xml: &str) -> anyhow::Result<Vec<String>> {
    let mut files = Vec::new();
    let mut rest = xml;
    while let Some(entry_start) = rest.find("<entry") {
        rest = &rest[entry_start..];
        let Some(entry_end) = rest.find("</entry>") else {
            anyhow::bail!("svn list --xml 输出不完整:缺少 </entry>");
        };
        let entry = &rest[..entry_end + "</entry>".len()];
        if entry.contains("kind=\"file\"") {
            let name_start = entry
                .find("<name>")
                .ok_or_else(|| anyhow::anyhow!("svn list --xml 输出不完整:缺少 <name>"))?
                + "<name>".len();
            let name_end = entry[name_start..]
                .find("</name>")
                .ok_or_else(|| anyhow::anyhow!("svn list --xml 输出不完整:缺少 </name>"))?
                + name_start;
            files.push(xml_unescape(&entry[name_start..name_end])?);
        }
        rest = &rest[entry_end + "</entry>".len()..];
    }
    Ok(files)
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

fn svn_cat(b: &VcsBinding, rel: &str) -> anyhow::Result<Vec<u8>> {
    let mut cat = vcs_command("svn");
    cat.arg("cat").arg(svn_url_with_rel(&b.url, rel));
    svn_add_auth(&mut cat, b);
    run_cmd_bytes(cat, "svn cat")
}

fn svn_url_with_rel(base: &str, rel: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), percent_encode_svn_path(rel))
}

fn percent_encode_svn_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

fn svn_export_without_workcopy(target: &Path, b: &VcsBinding) -> anyhow::Result<String> {
    let revision = svn_remote_revision(b)?;
    let _ = fs::remove_dir_all(target);
    fs::create_dir_all(target)?;
    let mut skipped = Vec::new();
    for rel in svn_list_recursive_files(b)? {
        if rel.is_empty() || rel.ends_with('/') {
            continue;
        }
        let Some(dst_rel) = svn_export_rel_path(&rel) else {
            skipped.push(rel);
            continue;
        };
        let dst = target.join(dst_rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if dst.is_dir() {
            let _ = fs::remove_dir_all(&dst);
        } else {
            let _ = fs::remove_file(&dst);
        }
        fs::write(dst, svn_cat(b, &rel)?)?;
    }
    let marker = format!(
        "SVN working copy fallback export.\nRevision: {revision}\nSkipped Windows-invalid paths:\n{}\n",
        skipped.join("\n")
    );
    fs::write(svn_export_marker(target), marker)?;
    if !skipped.is_empty() {
        eprintln!("[ktree] svn 跳过 Windows 非法路径: {}", skipped.join(", "));
    }
    Ok(revision)
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

/// 对一个绑定执行一次同步:拉取/更新(严格镜像)→ diff store → 入库 / 删除。
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

    // 拉取前 store 里这个绑定目录下所有 rel_path,作为"删除"检测的基线
    let before: HashSet<String> = state
        .store
        .list_documents(&kb.id, Some(&prefix))?
        .into_iter()
        .map(|d| d.rel_path)
        .collect();

    // 拉取 / 更新(内部已做严格镜像,盘上只剩仓库里的文件)
    report.revision = match b.vcs_type.as_str() {
        "git" => git_sync(kb, &target, &b)?,
        "svn" => svn_update_or_checkout(&target, &b)?,
        other => anyhow::bail!("不支持的 VCS 类型「{other}」(只支持 git / svn)"),
    };

    // 列出工作副本里所有文件 → 拼成相对 src/ 的路径
    let files = list_repo_files(&target)?;
    let after: HashSet<String> = files.iter().map(|r| format!("{prefix}/{r}")).collect();

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

    // 算 deleted:before - after,这些文件已不在仓库里,清 store + docs 产物
    let deleted: Vec<String> = before.difference(&after).cloned().collect();
    for rel_path in &deleted {
        if let Ok(Some(doc)) = state.store.get_by_path(&kb.id, rel_path) {
            // delete_doc 会清 src(严格镜像已删过 → 文件不存在也没事)、docs/、manifest、SQLite、tantivy
            if let Err(e) = ingest::delete_doc(state, kb, &doc) {
                report.failed.push(rel_path.clone());
                report.messages.push(format!("清理失败「{rel_path}」: {e}"));
            } else {
                report.deleted.push(rel_path.clone());
            }
        }
    }

    // 严格镜像收尾:清掉 docs 区里 store 已不认识的孤儿文件/空目录(历史失步自愈)
    let _ = ingest::prune_docs_orphans(state, kb, &prefix);

    // 刷新 .ktree 元数据
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
    // 并发保护:同一绑定同时只允许一个同步在跑(并发 git/svn 进程会互相打架、留锁)
    if !state.try_begin_sync("vcs", &kb.id, binding_idx) {
        anyhow::bail!("该绑定正在同步中,请等当前同步结束");
    }
    let result = sync_binding(state, kb, binding_idx);
    state.end_sync("vcs", &kb.id, binding_idx);
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
