// 知识库 .ktree/ 元数据:manifest.json / INDEX.md / KEYWORDS.md
// 以及 frontmatter 构建/解析、标签提取、缓存重建。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::KnowledgeBase;
use crate::state::AppState;
use crate::store::{Document, NewDocument};

// ============ manifest.json ============

/// manifest 的一条记录:key 是 src/ 下的相对路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub md5: String,
    /// 转换产物相对知识库根的路径(docs/<rel>.md),未转换为空串
    #[serde(default)]
    pub output: String,
    pub converted_at: String,
}

pub type Manifest = BTreeMap<String, ManifestEntry>;

pub fn manifest_path(kb_root: &Path) -> PathBuf {
    kb_root.join(".ktree").join("manifest.json")
}

pub fn load_manifest(kb_root: &Path) -> Manifest {
    let p = manifest_path(kb_root);
    fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_manifest(kb_root: &Path, m: &Manifest) -> anyhow::Result<()> {
    let p = manifest_path(kb_root);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&p, serde_json::to_string_pretty(m)?)?;
    Ok(())
}

pub fn timestamp_str() -> String {
    // 简单的 "秒级 epoch" 字符串即可,无需引入 chrono
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

// ============ 标签提取 ============

/// 从文件名(去扩展名)按常见分隔符拆出候选标签。沿用 chibo-kb 的思路。
pub fn extract_tags(stem: &str) -> Vec<String> {
    let mut tags = Vec::new();
    for part in stem.split(|c: char| {
        c.is_whitespace() || "_-—–·|【】()()[]{}<>、,,/\\".contains(c)
    }) {
        let t = part.trim();
        let len = t.chars().count();
        if len >= 2 && len <= 12 && !tags.iter().any(|x| x == t) {
            tags.push(t.to_string());
        }
    }
    tags
}

// ============ frontmatter ============

/// 取 rel_path 的顶层目录作为分类;根目录下文档归为「未分类」。
///
/// rel_path 带区前缀(upload/、vcs/、cloud/<provider>/),分类时跳过这层
/// 「来源渠道」前缀,取内容本身的顶层目录:
///   upload/策划/需求.docx        → 策划
///   upload/需求.docx             → 未分类
///   vcs/策划文档/子目录/需求.md   → 策划文档(绑定名)
///   cloud/feishu/产品需求/xx.md  → 产品需求(绑定名)
pub fn category_of(rel_path: &str) -> String {
    let parts: Vec<&str> = rel_path.split('/').filter(|p| !p.is_empty()).collect();
    let rest: &[&str] = match parts.first() {
        Some(&"upload") => &parts[1..],
        // vcs 区:绑定名就是分类
        Some(&"vcs") => &parts[1..],
        // cloud 区:跳过 cloud/<provider>,绑定名就是分类
        Some(&"cloud") if parts.len() > 2 => &parts[2..],
        Some(&"cloud") => &parts[1..],
        _ => &parts[..],
    };
    if rest.len() > 1 && !rest[0].is_empty() {
        rest[0].to_string()
    } else {
        "未分类".to_string()
    }
}

pub fn build_frontmatter(title: &str, category: &str, tags: &[String], summary: &str) -> String {
    let safe = |s: &str| s.replace('\n', " ").replace('\r', "");
    let mut fm = String::from("---\n");
    fm.push_str(&format!("title: {}\n", safe(title)));
    fm.push_str(&format!("category: {}\n", safe(category)));
    fm.push_str(&format!("tags: [{}]\n", tags.join(", ")));
    fm.push_str(&format!("summary: {}\n", safe(summary)));
    fm.push_str("---\n\n");
    fm
}

/// 解析 md 文件开头的 YAML frontmatter,返回 (title, category, tags, summary, body)。
/// 没有 frontmatter 时 body 即原文。
pub fn parse_frontmatter(content: &str) -> (String, String, Vec<String>, String, String) {
    let mut title = String::new();
    let mut category = String::new();
    let mut tags = Vec::new();
    let mut summary = String::new();

    if let Some(rest) = content.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            let fm = &rest[..end];
            let body = &rest[end + 5..];
            for line in fm.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("title:") {
                    title = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("category:") {
                    category = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("summary:") {
                    summary = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("tags:") {
                    let v = v.trim().trim_start_matches('[').trim_end_matches(']');
                    tags = v
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
            return (title, category, tags, summary, body.to_string());
        }
    }
    (title, category, tags, summary, content.to_string())
}

// ============ INDEX.md / KEYWORDS.md ============

/// 由某知识库的全部文档元信息重新生成 .ktree/INDEX.md 与 KEYWORDS.md。
pub fn regenerate_meta(kb: &KnowledgeBase, docs: &[Document]) -> anyhow::Result<()> {
    let meta_dir = kb.root.join(".ktree");
    fs::create_dir_all(&meta_dir)?;

    // ---- INDEX.md:按顶层目录分组 ----
    let mut by_cat: BTreeMap<String, Vec<&Document>> = BTreeMap::new();
    for d in docs {
        by_cat
            .entry(category_of(&d.rel_path))
            .or_default()
            .push(d);
    }
    let mut idx = String::from("# 知识库索引\n\n> 每篇文档一行(路径 — 摘要 [标签])。勿手动编辑,由 Ktree 自动生成。\n");
    for (cat, list) in &by_cat {
        idx.push_str(&format!("\n## {cat}\n\n"));
        for d in list {
            let tags = if d.tags.is_empty() {
                String::new()
            } else {
                format!(" [[{}]]", d.tags)
            };
            idx.push_str(&format!("- `{}` — {}{}\n", d.rel_path, d.summary, tags));
        }
    }
    fs::write(meta_dir.join("INDEX.md"), idx)?;

    // KEYWORDS.md 已废弃(无人读,标签倒排价值被混合检索覆盖):存量库里若残留则顺手删掉。
    let _ = fs::remove_file(meta_dir.join("KEYWORDS.md"));

    Ok(())
}

// ============ 缓存重建 ============

/// 启动时:若某知识库的 SQLite 缓存为空但 manifest 有记录,从 docs/ 的 md 重建
/// SQLite + tantivy 缓存(不重新跑文档转换)。
pub fn rebuild_cache_if_needed(state: &AppState, kb: &KnowledgeBase) -> anyhow::Result<usize> {
    let manifest = load_manifest(&kb.root);
    if manifest.is_empty() {
        return Ok(0);
    }
    let cached = state.store.count_documents(Some(&kb.id))?;
    if cached as usize >= manifest.len() {
        return Ok(0); // 缓存看起来已是最新
    }

    state.store.delete_by_kb(&kb.id)?;
    state.index.delete_by_kb(&kb.id)?;
    let mut rebuilt = 0usize;
    for (rel_path, entry) in &manifest {
        let src_abs = kb.root.join("src").join(rel_path);
        let size = fs::metadata(&src_abs).map(|m| m.len() as i64).unwrap_or(0);
        let ext = Path::new(rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let stem = Path::new(rel_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("untitled")
            .to_string();

        let (title, category, tags, summary, body) = if entry.output.is_empty() {
            (stem.clone(), category_of(rel_path), Vec::new(), String::new(), String::new())
        } else {
            let md_abs = kb.root.join(&entry.output);
            match fs::read_to_string(&md_abs) {
                Ok(content) => {
                    let (t, c, tg, s, b) = parse_frontmatter(&content);
                    (
                        if t.is_empty() { stem.clone() } else { t },
                        if c.is_empty() { category_of(rel_path) } else { c },
                        tg,
                        s,
                        b,
                    )
                }
                Err(_) => (stem.clone(), category_of(rel_path), Vec::new(), String::new(), String::new()),
            }
        };

        // HTML 等用纯文本做索引(原文件不动)。摘要若为空留待 backfill_meta 补算。
        let body = crate::textproc::clean_body(&body, &ext);

        let md_path = if entry.output.is_empty() {
            None
        } else {
            Some(entry.output.clone())
        };
        let tags_str = tags.join(",");
        let doc_id = state.store.upsert_document(&NewDocument {
            kb_id: kb.id.clone(),
            rel_path: rel_path.clone(),
            title: title.clone(),
            ext,
            size,
            md5: entry.md5.clone(),
            summary: summary.clone(),
            tags: tags_str,
            md_path,
            source: "local".to_string(),
        })?;
        state
            .index
            .add_or_update(&kb.id, doc_id, &title, &category, &body, &summary)?;
        rebuilt += 1;
    }
    state.index.commit()?;
    Ok(rebuilt)
}
