use std::fs;
use std::path::Path;

use md5::{Digest, Md5};

use crate::config::KnowledgeBase;
use crate::convert;
use crate::kbmeta;
use crate::state::AppState;
use crate::store::{Document, NewDocument};

const SUMMARY_LEN: usize = 200;

/// 路径组件白名单:拒绝空串、`..`、路径分隔符,防止目录穿越。
/// 供 HTTP 上传与 MCP 上传共用(单层文件名/目录名)。
pub(crate) fn safe_component(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t.contains("..") || t.contains('/') || t.contains('\\') {
        None
    } else {
        Some(t.to_string())
    }
}

/// 多层相对路径白名单:每一段都必须是安全组件。返回规范化的正斜杠路径。
pub(crate) fn safe_rel_path(s: &str) -> Option<String> {
    let parts: Vec<String> = s
        .split(['/', '\\'])
        .filter(|p| !p.is_empty())
        .map(|p| safe_component(p))
        .collect::<Option<Vec<_>>>()?;
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// 文本类扩展名:不转换时直接把原文当作可索引正文。
fn is_textual(ext: &str) -> bool {
    matches!(
        ext,
        "md" | "markdown" | "txt" | "html" | "htm" | "json" | "csv" | "log"
    )
}

/// 在 `docs/<rel_path>` 处建立一个指向 `src/<rel_path>` 的镜像。
///
/// 三层 fallback,优先级从高到低:
/// 1. **相对软链**(symlink):Linux / macOS 原生支持;Windows NTFS 需要管理员或开发者模式。
/// 2. **硬链接**(hard_link):无需特权,但要求同卷且只能链接文件 —— 知识库内部完全够用。
/// 3. **文件复制**(copy):前两种都失败时的最后兜底,代价是 src 改了 docs 不会自动跟。
///
/// 这样 Windows 普通用户(没开发者模式)也能跑通整个 ingest 流程。
fn mirror_into_docs(kb_root: &Path, rel_path: &str) -> std::io::Result<()> {
    let docs_abs = kb_root.join("docs").join(rel_path);
    if let Some(parent) = docs_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&docs_abs); // 删旧(普通文件或旧链)

    let depth = rel_path.matches('/').count();
    let up = "../".repeat(depth + 1);
    let link_target = format!("{up}src/{rel_path}");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&link_target, &docs_abs)
    }
    #[cfg(windows)]
    {
        let src_abs = kb_root.join("src").join(rel_path);
        if std::os::windows::fs::symlink_file(&link_target, &docs_abs).is_ok() {
            return Ok(());
        }
        if fs::hard_link(&src_abs, &docs_abs).is_ok() {
            return Ok(());
        }
        fs::copy(&src_abs, &docs_abs).map(|_| ())
    }
}

/// 把 rel_path 的父目录 + 新文件名拼成相对路径(正斜杠)。
fn with_name(rel_path: &str, name: &str) -> String {
    match Path::new(rel_path).parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => format!("{}/{}", dir.to_string_lossy().replace('\\', "/"), name),
        None => name.to_string(),
    }
}

/// 把 src/<rel_path> 下的一个文件纳入知识库:
/// 计算 md5 → (可选)转 Markdown 到 docs/、资源到 ref/ → 更新 .ktree/manifest.json
/// → 刷新 SQLite + tantivy 缓存。
///
/// 不刷新 INDEX.md / KEYWORDS.md —— 由调用方在批量结束后调 `refresh_kb_meta`。
/// `force` 为 true 时即使 md5 未变也重新处理。
pub fn ingest_file(
    state: &AppState,
    kb: &KnowledgeBase,
    rel_path: &str,
    source: &str,
    convert_md: bool,
    force: bool,
) -> anyhow::Result<Document> {
    let src_abs = kb.root.join("src").join(rel_path);
    let bytes = fs::read(&src_abs)?;
    let size = bytes.len() as i64;
    let md5 = format!("{:x}", Md5::digest(&bytes));
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
    let category = kbmeta::category_of(rel_path);

    // 增量:manifest md5 未变且 SQLite 已有 → 跳过
    let mut manifest = kbmeta::load_manifest(&kb.root);
    if !force {
        if let Some(entry) = manifest.get(rel_path) {
            if entry.md5 == md5 {
                if let Some(doc) = state.store.get_by_path(&kb.id, rel_path)? {
                    return Ok(doc);
                }
            }
        }
    }

    // docs/ 下的 md 相对路径:rel_path 换扩展名为 .md
    let rel_md = with_name(rel_path, &format!("{stem}.md"));
    // ref/ 下的资源目录相对路径:rel_path 去掉扩展名
    let rel_ref = with_name(rel_path, &stem);

    // 优先尝试转换(非文本可转换格式)。失败/不支持时回落到「软链镜像」。
    let converted = if !is_textual(&ext) && convert_md {
        let ref_abs = kb.root.join("ref").join(&rel_ref);
        let depth = rel_md.matches('/').count();
        let up = "../".repeat(depth + 1);
        let ref_prefix = format!("{up}ref/{rel_ref}");
        match convert::convert_file(&src_abs, &ext, &ref_abs, &ref_prefix) {
            Ok(r) if r.ok => Some(r),
            _ => None,
        }
    } else {
        None
    };

    let (md_path, body, summary, tags): (Option<String>, String, String, Vec<String>) =
        if let Some(r) = converted {
            // 转换成功(docx/pdf/xlsx…):写 docs/<rel_md>(带 frontmatter)
            let docs_abs = kb.root.join("docs").join(&rel_md);
            if let Some(parent) = docs_abs.parent() {
                fs::create_dir_all(parent)?;
            }
            let _ = fs::remove_file(&docs_abs); // 防止旧软链残留
            let tags = kbmeta::extract_tags(&stem);
            let fm = kbmeta::build_frontmatter(&stem, &category, &tags, &r.summary);
            fs::write(&docs_abs, format!("{fm}{}", r.markdown))?;
            (Some(format!("docs/{rel_md}")), r.markdown, r.summary, tags)
        } else {
            // 文本 / 转换失败 / 不支持转换:在 docs/<rel_path> 镜像 src 原文。
            // Linux/macOS 走相对软链;Windows 普通用户回落到硬链 → 复制。
            mirror_into_docs(&kb.root, rel_path)?;

            let tags = kbmeta::extract_tags(&stem);
            let (body, summary) = if is_textual(&ext) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let s: String = text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(SUMMARY_LEN)
                    .collect();
                (text, s)
            } else {
                (String::new(), String::new())
            };
            (Some(format!("docs/{rel_path}")), body, summary, tags)
        };

    // 更新 manifest.json
    manifest.insert(
        rel_path.to_string(),
        kbmeta::ManifestEntry {
            md5: md5.clone(),
            output: md_path.clone().unwrap_or_default(),
            converted_at: kbmeta::timestamp_str(),
        },
    );
    kbmeta::save_manifest(&kb.root, &manifest)?;

    // 刷新 SQLite + tantivy 缓存
    let doc_id = state.store.upsert_document(&NewDocument {
        kb_id: kb.id.clone(),
        rel_path: rel_path.to_string(),
        title: stem.clone(),
        ext,
        size,
        md5,
        summary: summary.clone(),
        tags: tags.join(","),
        md_path,
        source: source.to_string(),
    })?;
    state
        .index
        .add_or_update(&kb.id, doc_id, &stem, &category, &body, &summary)?;
    state.index.commit()?;

    state
        .store
        .get_document(doc_id)?
        .ok_or_else(|| anyhow::anyhow!("入库后无法读回文档 id={doc_id}"))
}

/// 从 SQLite 全量重算某知识库的 .ktree/INDEX.md 与 KEYWORDS.md。
pub fn refresh_kb_meta(state: &AppState, kb: &KnowledgeBase) -> anyhow::Result<()> {
    let docs = state.store.list_documents(&kb.id, None)?;
    kbmeta::regenerate_meta(kb, &docs)
}

/// 递归删除 src/ 下的一个文件夹及其在 docs/ ref/ manifest SQLite tantivy 里的所有关联。
/// `src_rel` 是相对 src 的子路径(如 "T1/sub"),不能为空(避免误删整个 src)。
pub fn delete_folder(
    state: &AppState,
    kb: &KnowledgeBase,
    src_rel: &str,
) -> anyhow::Result<usize> {
    if src_rel.trim().is_empty() {
        anyhow::bail!("不能删除 src 根目录");
    }
    // 先从 SQLite + tantivy 删除该目录下所有文档记录
    let docs = state.store.list_documents(&kb.id, Some(src_rel))?;
    let count = docs.len();
    for doc in &docs {
        let _ = state.store.delete_document(doc.id);
        let _ = state.index.delete(doc.id);
    }
    if count > 0 {
        let _ = state.index.commit();
    }

    // 清理三个真实目录(不存在也不报错)
    let _ = fs::remove_dir_all(kb.root.join("src").join(src_rel));
    let _ = fs::remove_dir_all(kb.root.join("docs").join(src_rel));
    let _ = fs::remove_dir_all(kb.root.join("ref").join(src_rel));

    // 清理 manifest:删掉所有 key 以 src_rel/ 开头或等于 src_rel 的条目
    let mut manifest = kbmeta::load_manifest(&kb.root);
    let prefix = format!("{src_rel}/");
    manifest.retain(|k, _| !(k.starts_with(&prefix) || k == src_rel));
    kbmeta::save_manifest(&kb.root, &manifest)?;

    Ok(count)
}

/// 从知识库删除一个文档:删 src 原件、docs 转换产物、ref 资源、manifest 条目、
/// SQLite + tantivy 缓存。
pub fn delete_doc(state: &AppState, kb: &KnowledgeBase, doc: &Document) -> anyhow::Result<()> {
    let _ = fs::remove_file(kb.root.join("src").join(&doc.rel_path));
    if let Some(md) = &doc.md_path {
        let _ = fs::remove_file(kb.root.join(md));
    }
    let stem = Path::new(&doc.rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !stem.is_empty() {
        let rel_ref = with_name(&doc.rel_path, stem);
        let _ = fs::remove_dir_all(kb.root.join("ref").join(&rel_ref));
    }

    let mut manifest = kbmeta::load_manifest(&kb.root);
    manifest.remove(&doc.rel_path);
    kbmeta::save_manifest(&kb.root, &manifest)?;

    state.store.delete_document(doc.id)?;
    state.index.delete(doc.id)?;
    state.index.commit()?;
    Ok(())
}
