use std::fs;
use std::path::Path;

use md5::{Digest, Md5};

use crate::config::KnowledgeBase;
use crate::convert;
use crate::kbmeta;
use crate::state::AppState;
use crate::store::{Document, NewDocument};
use crate::textproc;

/// 送去算语义向量的文本上限(字符)。模型自身只吃 ~512 token,
/// 这里截一刀只是别把整篇大文档塞进管道。
const EMBED_TEXT_LIMIT: usize = 1500;

/// 一篇文档取多少个关键词。
const KEYWORD_COUNT: usize = 8;

/// 由正文派生标签:正文非空走 jieba 关键词,为空(图片等)退化为文件名拆词。
fn derive_tags(body: &str, stem: &str) -> Vec<String> {
    if body.trim().is_empty() {
        return kbmeta::extract_tags(stem);
    }
    let kw = textproc::keywords(body, KEYWORD_COUNT);
    if kw.is_empty() {
        kbmeta::extract_tags(stem)
    } else {
        kw
    }
}

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
            let body = r.markdown;
            let summary = textproc::summarize(&state.embedder, &body);
            let tags = derive_tags(&body, &stem);
            let fm = kbmeta::build_frontmatter(&stem, &category, &tags, &summary);
            fs::write(&docs_abs, format!("{fm}{body}"))?;
            (Some(format!("docs/{rel_md}")), body, summary, tags)
        } else {
            // 文本 / 转换失败 / 不支持转换:在 docs/<rel_path> 镜像 src 原文。
            // Linux/macOS 走相对软链;Windows 普通用户回落到硬链 → 复制。
            mirror_into_docs(&kb.root, rel_path)?;

            // HTML 抽纯文本再索引(原文件不动);其它文本类原样;二进制无正文。
            let body = if is_textual(&ext) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                textproc::clean_body(&text, &ext)
            } else {
                String::new()
            };
            let summary = if body.trim().is_empty() {
                String::new()
            } else {
                textproc::summarize(&state.embedder, &body)
            };
            let tags = derive_tags(&body, &stem);
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

    // 语义向量:标题 + 正文片段编码后存进 doc_vectors。
    // 失败不阻断入库 —— BM25 检索仍可用,向量可后续补算。
    let embed_text: String = format!("{stem}\n{body}")
        .chars()
        .take(EMBED_TEXT_LIMIT)
        .collect();
    match state.embedder.embed(std::slice::from_ref(&embed_text), "") {
        Ok(mut vs) => {
            if let Some(v) = vs.pop() {
                let _ = state.store.set_vector(doc_id, &v);
            }
        }
        Err(e) => eprintln!("[ktree] 文档 #{doc_id} 向量化失败(不影响入库): {e}"),
    }

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

/// 去掉文件开头的 YAML frontmatter(`---` 包起来的块)。
fn strip_frontmatter(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("---\n").or_else(|| s.strip_prefix("---\r\n")) {
        for marker in ["\n---\n", "\n---\r\n"] {
            if let Some(i) = rest.find(marker) {
                return &rest[i + marker.len()..];
            }
        }
    }
    s
}

/// 读一篇文档的「可索引正文」:优先 docs/ 下的产物,回落到 src 原文,
/// 去掉 frontmatter,HTML 抽纯文本。读不到(图片等二进制)返回空串。
fn read_doc_body(kb: &KnowledgeBase, doc: &Document) -> String {
    let raw = doc
        .md_path
        .as_ref()
        .and_then(|md| fs::read_to_string(kb.root.join(md)).ok())
        .or_else(|| fs::read_to_string(kb.root.join("src").join(&doc.rel_path)).ok())
        .unwrap_or_default();
    textproc::clean_body(strip_frontmatter(&raw), &doc.ext)
}

/// 取一篇文档用于嵌入的文本:「标题 + 正文」截到 `EMBED_TEXT_LIMIT`。
/// 读不到正文(图片等二进制)时退化为只用标题。
fn doc_embed_text(kb: &KnowledgeBase, doc: &Document) -> String {
    format!("{}\n{}", doc.title, read_doc_body(kb, doc))
        .chars()
        .take(EMBED_TEXT_LIMIT)
        .collect()
}

/// 给 summary 为空的存量文档补算摘要 / 关键词,并用纯文本重建索引 + 向量。
/// 主要修两件事:① 缓存重建遗留的空摘要;② HTML 此前用带标签原文做索引。
/// 启动时后台调用,返回处理篇数。
pub fn backfill_meta(state: &AppState) -> usize {
    let kbs = state.config.snapshot().knowledge_bases;
    let mut done = 0usize;
    for kb in &kbs {
        let docs = match state.store.list_documents(&kb.id, None) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for doc in docs {
            if !doc.summary.trim().is_empty() {
                continue; // 已有摘要,跳过
            }
            let body = read_doc_body(kb, &doc);
            if body.trim().is_empty() {
                continue; // 图片 / 二进制,无正文可抽
            }
            let summary = textproc::summarize(&state.embedder, &body);
            let tags = derive_tags(&body, &doc.title);
            if state
                .store
                .update_meta(doc.id, &summary, &tags.join(","))
                .is_err()
            {
                continue;
            }
            let category = kbmeta::category_of(&doc.rel_path);
            let _ =
                state
                    .index
                    .add_or_update(&kb.id, doc.id, &doc.title, &category, &body, &summary);
            // 用纯文本重算向量(HTML 此前的向量含标签噪音)
            let etext: String = format!("{}\n{}", doc.title, body)
                .chars()
                .take(EMBED_TEXT_LIMIT)
                .collect();
            if let Ok(mut vs) = state.embedder.embed(std::slice::from_ref(&etext), "") {
                if let Some(v) = vs.pop() {
                    let _ = state.store.set_vector(doc.id, &v);
                }
            }
            done += 1;
        }
    }
    if done > 0 {
        let _ = state.index.commit();
    }
    done
}

/// 给 store 里还没有语义向量的文档补算并写入,返回 (成功数, 失败数)。
/// 启动时后台调用。某篇读不到文件 / embed 失败只是跳过,不中断。
pub fn backfill_vectors(state: &AppState) -> (usize, usize) {
    let docs = match state.store.docs_missing_vector() {
        Ok(d) => d,
        Err(_) => return (0, 0),
    };
    let (mut done, mut failed) = (0usize, 0usize);
    for doc in docs {
        let Some(kb) = state.config.get_kb(&doc.kb_id) else {
            continue; // KB 已不存在(孤儿应已清,稳妥起见仍跳过)
        };
        let text = doc_embed_text(&kb, &doc);
        match state.embedder.embed(std::slice::from_ref(&text), "") {
            Ok(mut vs) => match vs.pop() {
                Some(v) if state.store.set_vector(doc.id, &v).is_ok() => done += 1,
                _ => failed += 1,
            },
            Err(_) => failed += 1,
        }
    }
    (done, failed)
}
