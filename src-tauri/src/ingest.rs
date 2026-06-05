use std::collections::HashSet;
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

/// 使用者显式排除知识库索引的前缀。
/// 任一路径层级的目录名或文件名以这些前缀开头时,不转换、不索引。
const IGNORE_PREFIX_ASCII: &str = "##!";
const IGNORE_PREFIX_FULLWIDTH: &str = "##！";

pub(crate) fn is_ignored_component(name: &str) -> bool {
    name.starts_with(IGNORE_PREFIX_ASCII) || name.starts_with(IGNORE_PREFIX_FULLWIDTH)
}

/// 判断相对路径任一层级是否带有显式忽略前缀。
/// 接受相对 src/ 的路径,也接受相对知识库根的 src/... / docs/... 路径。
pub(crate) fn path_has_ignored_component(path: &str) -> bool {
    path.split(['/', '\\'])
        .filter(|p| !p.is_empty())
        .any(is_ignored_component)
}

pub(crate) fn ignore_rule_description() -> &'static str {
    "路径任一目录或文件名以 ##! 或 ##！ 开头,按显式忽略规则不转换、不索引"
}

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

/// 判断 src/ 下的相对路径是否落在用户上传区(src/upload/)内。
/// vcs/、cloud/ 区只读,一切写操作(上传 / 建目录 / 删除)只允许 upload 区。
pub(crate) fn in_upload_area(src_rel: &str) -> bool {
    src_rel == crate::config::AREA_UPLOAD
        || src_rel.starts_with(&format!("{}/", crate::config::AREA_UPLOAD))
}

/// 文档 md 的伴生资源目录(相对 docs/ 的路径):`<父目录>/<文件名去扩展名>.assets`。
/// 转换出的图片附件都放这里,md 内用同目录相对路径 `<stem>.assets/xxx.png` 引用。
pub(crate) fn assets_rel_of(rel_path: &str) -> String {
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    with_name(rel_path, &format!("{stem}.assets"))
}

/// 文本类扩展名:不转换时直接把原文当作可索引正文。
pub(crate) fn is_textual(ext: &str) -> bool {
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
    if path_has_ignored_component(rel_path) {
        let _ = forget_path_artifacts(state, kb, rel_path);
        anyhow::bail!("{}", ignore_rule_description());
    }

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

    // 增量:manifest md5 未变 且 SQLite 已有 且 docs 产物文件仍在 → 跳过。
    // 多加一道"docs 产物存在性"检查:换 URL / 改结构 / 软链断裂等导致 docs 失步时,
    // 不能因 md5 匹配就短路 —— 那样缺失的 docs 产物永远补不回来。docs 不在则重新生成。
    let mut manifest = kbmeta::load_manifest(&kb.root);
    if !force {
        if let Some(entry) = manifest.get(rel_path) {
            if entry.md5 == md5 {
                if let Some(doc) = state.store.get_by_path(&kb.id, rel_path)? {
                    let docs_ok = doc
                        .md_path
                        .as_ref()
                        .map(|md| kb.root.join(md).exists())
                        .unwrap_or(true);
                    if docs_ok {
                        return Ok(doc);
                    }
                }
            }
        }
    }

    // docs/ 下的 md 相对路径:rel_path 换扩展名为 .md
    let rel_md = with_name(rel_path, &format!("{stem}.md"));
    // 图片附件的伴生资源目录(放 md 旁边):docs/<父目录>/<stem>.assets/
    let rel_assets = assets_rel_of(rel_path);

    // 优先尝试转换(非文本可转换格式)。失败/不支持时回落到「软链镜像」。
    let converted = if !is_textual(&ext) && convert_md {
        let assets_abs = kb.root.join("docs").join(&rel_assets);
        // md 与 .assets 同目录,引用前缀就是目录名本身
        let assets_prefix = format!("{stem}.assets");
        match convert::convert_file(&src_abs, &ext, &assets_abs, &assets_prefix) {
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

/// 严格镜像收尾:清理 docs/<prefix> 区下不属于当前 store 文档的孤儿文件与空目录。
/// vcs/cloud 区是只读镜像,docs 产物应与 store 严格一一对应;基于 store diff 的
/// delete_doc 清不掉"store 已无记录"的历史失步残留,这里以 store 为真相兜底自愈。
/// `prefix` 是相对 src/ 的区前缀(如 "vcs/svn"、"cloud/feishu/xxx")。返回清理的文件数。
pub fn prune_docs_orphans(
    state: &AppState,
    kb: &KnowledgeBase,
    prefix: &str,
) -> anyhow::Result<usize> {
    // 当前 store 里该区的全部文档 → 合法的 docs 产物路径(相对 docs/)+ .assets 目录
    let docs = state.store.list_documents(&kb.id, Some(prefix))?;
    let mut keep_files: HashSet<String> = HashSet::new();
    let mut keep_assets: HashSet<String> = HashSet::new();
    for d in &docs {
        if let Some(md) = &d.md_path {
            if let Some(rel) = md.strip_prefix("docs/") {
                keep_files.insert(rel.to_string());
            }
        }
        keep_assets.insert(assets_rel_of(&d.rel_path)); // 相对 docs/ 的 .assets 目录
    }

    let docs_base = kb.root.join("docs");
    let scan_root = docs_base.join(prefix);
    if !scan_root.is_dir() {
        return Ok(0);
    }

    // 递归收集 docs/<prefix> 下所有文件(相对 docs/ 的正斜杠路径)
    fn walk(base: &Path, rel: &str, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(base.join(rel)) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(base, &child, out),
                Ok(_) => out.push(child),
                _ => {}
            }
        }
    }
    let mut files = Vec::new();
    walk(&docs_base, prefix, &mut files);

    let mut removed = 0;
    for f in files {
        if keep_files.contains(&f) {
            continue;
        }
        // 在某个合法 .assets 目录下的资源文件 → 保留
        if keep_assets.iter().any(|a| f.starts_with(&format!("{a}/"))) {
            continue;
        }
        if fs::remove_file(docs_base.join(&f)).is_ok() {
            removed += 1;
        }
    }

    // 自底向上删空目录(scan_root 本身若空也删,下次同步会重建)
    fn rm_empty(dir: &Path) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    rm_empty(&e.path());
                }
            }
        }
        let _ = fs::remove_dir(dir); // 非空会失败,忽略
    }
    rm_empty(&scan_root);

    Ok(removed)
}

/// 从 SQLite 全量重算某知识库的 .ktree/INDEX.md。
pub fn refresh_kb_meta(state: &AppState, kb: &KnowledgeBase) -> anyhow::Result<()> {
    let docs: Vec<_> = state
        .store
        .list_documents(&kb.id, None)?
        .into_iter()
        .filter(|d| !path_has_ignored_component(&d.rel_path))
        .collect();
    kbmeta::regenerate_meta(kb, &docs)
}

/// 递归删除 src/ 下的一个文件夹及其在 docs/ manifest SQLite tantivy 里的所有关联。
/// `src_rel` 是相对 src 的子路径(如 "upload/T1/sub"),不能为空(避免误删整个 src)。
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

    // 清理两个真实目录(不存在也不报错)。docs 目录里的 .assets 伴生目录一并删掉。
    let _ = fs::remove_dir_all(kb.root.join("src").join(src_rel));
    let _ = fs::remove_dir_all(kb.root.join("docs").join(src_rel));

    // 清理 manifest:删掉所有 key 以 src_rel/ 开头或等于 src_rel 的条目
    let mut manifest = kbmeta::load_manifest(&kb.root);
    let prefix = format!("{src_rel}/");
    manifest.retain(|k, _| !(k.starts_with(&prefix) || k == src_rel));
    kbmeta::save_manifest(&kb.root, &manifest)?;

    Ok(count)
}

/// 从知识库删除一个文档:删 src 原件、docs 转换产物及其 .assets 伴生资源、
/// manifest 条目、SQLite + tantivy 缓存。
pub fn delete_doc(state: &AppState, kb: &KnowledgeBase, doc: &Document) -> anyhow::Result<()> {
    let _ = fs::remove_file(kb.root.join("src").join(&doc.rel_path));
    forget_doc_artifacts(state, kb, doc)
}

/// 只清理某文档的 docs 产物 / manifest / SQLite / tantivy,保留 src 原件。
/// 用于显式忽略规则:文件仍可留在 VCS 工作副本里,但不再属于知识库索引。
pub(crate) fn forget_doc_artifacts(
    state: &AppState,
    kb: &KnowledgeBase,
    doc: &Document,
) -> anyhow::Result<()> {
    if let Some(md) = &doc.md_path {
        let _ = fs::remove_file(kb.root.join(md));
    }
    // 伴生资源目录:docs/<父目录>/<stem>.assets/
    let _ = fs::remove_dir_all(
        kb.root.join("docs").join(assets_rel_of(&doc.rel_path)),
    );

    let mut manifest = kbmeta::load_manifest(&kb.root);
    manifest.remove(&doc.rel_path);
    kbmeta::save_manifest(&kb.root, &manifest)?;

    state.store.delete_document(doc.id)?;
    state.index.delete(doc.id)?;
    state.index.commit()?;
    Ok(())
}

pub(crate) fn forget_path_artifacts(
    state: &AppState,
    kb: &KnowledgeBase,
    rel_path: &str,
) -> anyhow::Result<bool> {
    let Some(doc) = state.store.get_by_path(&kb.id, rel_path)? else {
        return Ok(false);
    };
    forget_doc_artifacts(state, kb, &doc)?;
    Ok(true)
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

fn push_unique_path(paths: &mut Vec<String>, rel: &str) {
    if let Some(rel) = safe_rel_path(rel) {
        if !paths.iter().any(|p| p == &rel) {
            paths.push(rel);
        }
    }
}

/// 读取一篇文档的 Markdown/文本全文。优先读 store 里的 md_path;
/// 若 docs 产物失效、断链、空文件或 Windows 路径异常,回落到同路径 docs 与 src 原文。
pub(crate) fn read_doc_markdown(kb: &KnowledgeBase, doc: &Document) -> anyhow::Result<String> {
    let mut paths = Vec::new();
    if let Some(md) = doc.md_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        push_unique_path(&mut paths, md);
    }
    push_unique_path(&mut paths, &format!("docs/{}", doc.rel_path));
    push_unique_path(&mut paths, &format!("src/{}", doc.rel_path));

    let mut first_error: Option<std::io::Error> = None;
    for rel in paths {
        if rel.starts_with("src/") && !is_textual(&doc.ext) {
            continue;
        }
        match fs::read(kb.root.join(&rel)) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if !text.trim().is_empty() || rel.starts_with("src/") {
                    return Ok(text);
                }
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    if !is_textual(&doc.ext) {
        anyhow::bail!("该文档没有可读取的 Markdown 转换结果,请先同步或全库检查补齐 docs");
    }
    match first_error {
        Some(e) => Err(e.into()),
        None => anyhow::bail!("该文档没有可读文本"),
    }
}

/// 读一篇文档的「可索引正文」:优先 docs/ 下的产物,回落到 src 原文,
/// 去掉 frontmatter,HTML 抽纯文本。读不到(图片等二进制)返回空串。
pub(crate) fn read_doc_body(kb: &KnowledgeBase, doc: &Document) -> String {
    let raw = read_doc_markdown(kb, doc).unwrap_or_default();
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
            if path_has_ignored_component(&doc.rel_path) {
                continue;
            }
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
        if path_has_ignored_component(&doc.rel_path) {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::path_has_ignored_component;

    #[test]
    fn explicit_ignore_prefix_only() {
        assert!(path_has_ignored_component("vcs/svn/##!草稿/方案.md"));
        assert!(path_has_ignored_component("vcs/svn/##！草稿/方案.md"));
        assert!(path_has_ignored_component("vcs/svn/策划/##!方案.md"));
        assert!(!path_has_ignored_component("vcs/svn/归档（AI不看）/方案.md"));
        assert!(!path_has_ignored_component("vcs/svn/策划/方案##!.md"));
    }
}
