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
/// 1. **软链**(symlink):Linux / macOS 用相对目标;Windows 用绝对目标(见下)。
/// 2. **硬链接**(hard_link):无需特权,但要求同卷且只能链接文件 —— 知识库内部完全够用。
/// 3. **文件复制**(copy):前两种都失败时的最后兜底,代价是 src 改了 docs 不会自动跟。
///
/// 这样 Windows 普通用户(没开发者模式)也能跑通整个 ingest 流程。
///
/// Windows 上符号链接的目标用**绝对路径**:相对目标(里含 `/`)在 Windows 上
/// 建出来的链接打不开 —— `is_file()` 恒为 false,阅读视图一路回落 src,
/// 短路判定与逐文件核对也把产物当成缺失(见 `docs_artifact_present`)。
fn mirror_into_docs(kb_root: &Path, rel_path: &str) -> std::io::Result<()> {
    let docs_abs = kb_root.join("docs").join(rel_path);
    if let Some(parent) = docs_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&docs_abs); // 删旧(普通文件或旧链)

    #[cfg(unix)]
    {
        let depth = rel_path.matches('/').count();
        let up = "../".repeat(depth + 1);
        let link_target = format!("{up}src/{rel_path}");
        std::os::unix::fs::symlink(&link_target, &docs_abs)
    }
    #[cfg(windows)]
    {
        let src_abs = kb_root.join("src").join(rel_path);
        if std::os::windows::fs::symlink_file(&src_abs, &docs_abs).is_ok() {
            return Ok(());
        }
        if fs::hard_link(&src_abs, &docs_abs).is_ok() {
            return Ok(());
        }
        fs::copy(&src_abs, &docs_abs).map(|_| ())
    }
}

/// docs 产物是否"在"。
///
/// **不能**用 `is_file()`:Windows 上镜像类产物是符号链接,`is_file()` 跟随链接,
/// 链接不可解析时恒为 false。实测(2026-09-22 / v0.1.22,萌喵+吃播库):盘上这批
/// 链接打不开,于是每个"原样镜像"的文件(html/md/py/png/mp4/csv…)每轮同步都被
/// 判成产物缺失 → 整库重新转换 + 重新向量化,一轮二十多分钟,且与 SVN 端
/// 是否真有提交无关 —— 转换类产物(xlsx→.md 是真实文件)则一切正常。
///
/// 这里改成"目录项存在即算在":普通文件、有效链接、断链链接都算有产物
/// (断链内容由 `serve_kb_file` 回落 src 兜底,阅读视图不受影响);
/// 只有产物真被删掉才返回 false,让它重建。
pub(crate) fn docs_artifact_present(kb_root: &Path, md_path: &str) -> bool {
    if md_path.is_empty() {
        return false;
    }
    match fs::symlink_metadata(kb_root.join(md_path)) {
        Ok(m) => m.is_file() || m.file_type().is_symlink(),
        Err(_) => false,
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
///
/// 单文件入口:自带 manifest 读写。批量同步(VCS)必须用 `ingest_file_with_manifest`,
/// 否则每个文件都要解析+重写整个 manifest.json(大库为 MB 级,16k 文件就是几十 GB 的
/// JSON 序列化),一轮同步会被拖到小时级。
pub fn ingest_file(
    state: &AppState,
    kb: &KnowledgeBase,
    rel_path: &str,
    source: &str,
    convert_md: bool,
    force: bool,
) -> anyhow::Result<Document> {
    let mut manifest = kbmeta::load_manifest(&kb.root);
    let mut dirty = false;
    let doc = ingest_file_with_manifest(
        state, kb, rel_path, source, convert_md, force, &mut manifest, &mut dirty,
    )?;
    if dirty {
        kbmeta::save_manifest(&kb.root, &manifest)?;
    }
    Ok(doc)
}

/// `ingest_file` 的批量版本:manifest 由调用方加载、传入并负责落盘。
/// 修改过 manifest 时置位 `manifest_dirty`(跳过路径不落盘,调用方据此省掉无谓写)。
#[allow(clippy::too_many_arguments)]
pub(crate) fn ingest_file_with_manifest(
    state: &AppState,
    kb: &KnowledgeBase,
    rel_path: &str,
    source: &str,
    convert_md: bool,
    force: bool,
    manifest: &mut kbmeta::Manifest,
    manifest_dirty: &mut bool,
) -> anyhow::Result<Document> {
    if path_has_ignored_component(rel_path) {
        let _ = forget_path_artifacts_with_manifest(state, kb, rel_path, manifest, manifest_dirty);
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

    // 增量:manifest / SQLite md5 均未变、二者记录的产物路径一致,
    // 且 docs 产物文件仍在 → 跳过。
    // 多加一道"docs 产物存在性"检查:换 URL / 改结构 / 软链断裂等导致 docs 失步时,
    // 不能因 md5 匹配就短路 —— 那样缺失的 docs 产物永远补不回来。docs 不在则重新生成。
    if !force {
        if let Some(entry) = manifest.get(rel_path) {
            if entry.md5 == md5 {
                if let Some(doc) = state.store.get_by_path(&kb.id, rel_path)? {
                    let store_ok = doc.md5 == md5
                        && entry.output == doc.md_path.as_deref().unwrap_or_default();
                    let docs_ok = doc
                        .md_path
                        .as_deref()
                        .map(|md| docs_artifact_present(&kb.root, md))
                        .unwrap_or(false);
                    if store_ok && docs_ok {
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

    // 更新 manifest(落盘时机由调用方决定)
    manifest.insert(
        rel_path.to_string(),
        kbmeta::ManifestEntry {
            md5: md5.clone(),
            output: md_path.clone().unwrap_or_default(),
            converted_at: kbmeta::timestamp_str(),
        },
    );
    *manifest_dirty = true;

    // frontmatter 属性(② 属性视图 / prop: 算子)与文档内链接(③ 反链 / 图谱),均从 md 正文解析。
    let props = parse_frontmatter_props(&body);
    let links = extract_links(&body);

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
        props,
        md_path,
        source: source.to_string(),
    })?;
    // 出链入库(失败不阻断:反链 / 图谱是增强项)
    let _ = state.store.set_doc_links(doc_id, &kb.id, &links);
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

/// 严格镜像收尾:清理 docs/<prefix> 区下不再同时对应 src 原件与 store 文档的
/// 孤儿文件、断链软链接和空目录。src 是来源真相,store 只作为产物路径映射。
/// `prefix` 是相对 src/ 的区前缀(如 "vcs/svn"、"cloud/feishu/xxx")。返回清理的文件数。
pub fn prune_docs_orphans(
    state: &AppState,
    kb: &KnowledgeBase,
    prefix: &str,
) -> anyhow::Result<usize> {
    prune_docs_orphans_with_store(&state.store, kb, prefix)
}

fn prune_docs_orphans_with_store(
    store: &crate::store::Store,
    kb: &KnowledgeBase,
    prefix: &str,
) -> anyhow::Result<usize> {
    // 当前 store 里该区且 src 原件仍存在的文档 → 合法 docs 产物路径 + .assets 目录。
    // store 可能因一次漏掉的 SVN 删除而残留旧记录,不能仅凭 store 就保留 docs,
    // 否则源文件已删除的断链软链接会永久留在阅读视图。
    let docs = store.list_documents(&kb.id, Some(prefix))?;
    let mut keep_files: HashSet<String> = HashSet::new();
    let mut keep_assets: HashSet<String> = HashSet::new();
    for d in &docs {
        if path_has_ignored_component(&d.rel_path)
            || !kb.root.join("src").join(&d.rel_path).is_file()
        {
            continue;
        }
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
    fn walk(base: &Path, rel: &str, out: &mut Vec<String>) -> std::io::Result<()> {
        for entry in fs::read_dir(base.join(rel))? {
            let e = entry?;
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if e.file_type()?.is_dir() {
                walk(base, &child, out)?;
            } else {
                // 包含普通文件和软链接(包括源文件已删除后的断链软链接)。
                out.push(child);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(&docs_base, prefix, &mut files)?;

    let mut removed = 0;
    for f in files {
        if keep_files.contains(&f) {
            continue;
        }
        // 在某个合法 .assets 目录下的资源文件 → 保留
        if keep_assets.iter().any(|a| f.starts_with(&format!("{a}/"))) {
            continue;
        }
        let stale = docs_base.join(&f);
        fs::remove_file(&stale)
            .map_err(|e| anyhow::anyhow!("清理 docs 孤儿文件失败({}): {e}", stale.display()))?;
        removed += 1;
    }

    // 自底向上删空目录(scan_root 本身若空也删,下次同步会重建)
    fn rm_empty(dir: &Path) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let e = entry?;
            if e.file_type()?.is_dir() {
                rm_empty(&e.path())?;
            }
        }
        // 目录仍有合法产物时保留;已经为空则删除,此时权限等失败需要向上报告。
        if fs::read_dir(dir)?.next().transpose()?.is_some() {
            Ok(())
        } else {
            fs::remove_dir(dir)
        }
    }
    rm_empty(&scan_root).map_err(|e| {
        anyhow::anyhow!("清理 docs 空目录失败({}): {e}", scan_root.display())
    })?;

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
/// manifest 条目、SQLite + tantivy 缓存。单文件入口,自带 manifest 读写;
/// 批量删除(VCS 严格镜像)用 `delete_doc_with_manifest`。
pub fn delete_doc(state: &AppState, kb: &KnowledgeBase, doc: &Document) -> anyhow::Result<()> {
    let mut manifest = kbmeta::load_manifest(&kb.root);
    let mut dirty = false;
    delete_doc_with_manifest(state, kb, doc, &mut manifest, &mut dirty)?;
    if dirty {
        kbmeta::save_manifest(&kb.root, &manifest)?;
    }
    Ok(())
}

pub(crate) fn delete_doc_with_manifest(
    state: &AppState,
    kb: &KnowledgeBase,
    doc: &Document,
    manifest: &mut kbmeta::Manifest,
    manifest_dirty: &mut bool,
) -> anyhow::Result<()> {
    let _ = fs::remove_file(kb.root.join("src").join(&doc.rel_path));
    forget_doc_artifacts_with_manifest(state, kb, doc, manifest, manifest_dirty)
}

/// 只清理某文档的 docs 产物 / manifest / SQLite / tantivy,保留 src 原件。
/// 用于显式忽略规则:文件仍可留在 VCS 工作副本里,但不再属于知识库索引。
pub(crate) fn forget_doc_artifacts(
    state: &AppState,
    kb: &KnowledgeBase,
    doc: &Document,
) -> anyhow::Result<()> {
    let mut manifest = kbmeta::load_manifest(&kb.root);
    let mut dirty = false;
    forget_doc_artifacts_with_manifest(state, kb, doc, &mut manifest, &mut dirty)?;
    if dirty {
        kbmeta::save_manifest(&kb.root, &manifest)?;
    }
    Ok(())
}

pub(crate) fn forget_doc_artifacts_with_manifest(
    state: &AppState,
    kb: &KnowledgeBase,
    doc: &Document,
    manifest: &mut kbmeta::Manifest,
    manifest_dirty: &mut bool,
) -> anyhow::Result<()> {
    if let Some(md) = &doc.md_path {
        let _ = fs::remove_file(kb.root.join(md));
    }
    // 伴生资源目录:docs/<父目录>/<stem>.assets/
    let _ = fs::remove_dir_all(
        kb.root.join("docs").join(assets_rel_of(&doc.rel_path)),
    );

    if manifest.remove(&doc.rel_path).is_some() {
        *manifest_dirty = true;
    }

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

pub(crate) fn forget_path_artifacts_with_manifest(
    state: &AppState,
    kb: &KnowledgeBase,
    rel_path: &str,
    manifest: &mut kbmeta::Manifest,
    manifest_dirty: &mut bool,
) -> anyhow::Result<bool> {
    let Some(doc) = state.store.get_by_path(&kb.id, rel_path)? else {
        return Ok(false);
    };
    forget_doc_artifacts_with_manifest(state, kb, &doc, manifest, manifest_dirty)?;
    Ok(true)
}

/// 从去扩展名的文件名得到链接归一化键(小写)。`[[Note]]` 与 `[x](dir/Note.md)` 都按此匹配。
pub(crate) fn link_key_of(rel_path: &str) -> String {
    let last = rel_path.rsplit(['/', '\\']).next().unwrap_or(rel_path);
    let stem = last.rsplit_once('.').map(|(a, _)| a).unwrap_or(last);
    stem.trim().to_lowercase()
}

/// 把链接目标(wikilink 内文 / md 链接 url)归一化成 target_key;外链 / 空 → None。
fn link_key_from_ref(raw: &str) -> Option<String> {
    let s = raw.split('|').next().unwrap_or(raw); // 去 wikilink 别名
    let s = s.split('#').next().unwrap_or(s); // 去锚点
    let s = s.split('?').next().unwrap_or(s); // 去查询串
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let low = s.to_ascii_lowercase();
    if low.starts_with("http://")
        || low.starts_with("https://")
        || low.starts_with("mailto:")
        || low.starts_with("//")
        || low.starts_with("data:")
    {
        return None;
    }
    let key = link_key_of(s);
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// 从 markdown 正文提取出链:`[[wikilink]]` 与 `[text](target)`。去重,返回 (target_key, 显示文本)。
pub(crate) fn extract_links(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |key: String, text: &str, seen: &mut std::collections::HashSet<String>| {
        if seen.insert(key.clone()) {
            out.push((key, text.trim().to_string()));
        }
    };
    // [[wikilink]]
    let mut i = 0;
    while let Some(p) = body[i..].find("[[") {
        let start = i + p + 2;
        match body[start..].find("]]") {
            Some(q) => {
                let inner = &body[start..start + q];
                if let Some(key) = link_key_from_ref(inner) {
                    push(key, inner, &mut seen);
                }
                i = start + q + 2;
            }
            None => break,
        }
    }
    // [text](target)
    let mut j = 0;
    while let Some(p) = body[j..].find("](") {
        let start = j + p + 2;
        match body[start..].find(')') {
            Some(q) => {
                let target = &body[start..start + q];
                if let Some(key) = link_key_from_ref(target) {
                    push(key, target, &mut seen);
                }
                j = start + q + 1;
            }
            None => break,
        }
    }
    out
}

/// 解析 frontmatter 顶层 `key: value` 属性,返回 JSON 对象串(无则空串)。
/// 只取顶层标量键值,跳过缩进的列表项 / 续行;`tags: [a, b]` 这类整串保留为字符串。
/// 跳过 title/category/tags/summary 这四个系统保留键(已在标题 / 标签 / 摘要里单独呈现,
/// 也是转换文件生成 frontmatter 的固定字段),只留用户自定义属性。
pub(crate) fn parse_frontmatter_props(text: &str) -> String {
    const RESERVED: [&str; 4] = ["title", "category", "tags", "summary"];
    let rest = match text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        Some(r) => r,
        None => return String::new(),
    };
    let end = ["\n---\n", "\n---\r\n", "\n---"]
        .iter()
        .filter_map(|m| rest.find(m))
        .min();
    let block = match end {
        Some(e) => &rest[..e],
        None => return String::new(),
    };
    let mut map = serde_json::Map::new();
    for line in block.lines() {
        if line.starts_with(' ') || line.starts_with('\t') || line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            if key.is_empty() || k.starts_with('-') || RESERVED.contains(&key.to_lowercase().as_str()) {
                continue;
            }
            let val = v.trim().trim_matches('"').trim_matches('\'').trim();
            map.insert(
                key.to_string(),
                serde_json::Value::String(val.to_string()),
            );
        }
    }
    if map.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&map).unwrap_or_default()
    }
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
    use super::{
        docs_artifact_present, extract_links, link_key_of, parse_frontmatter_props,
        path_has_ignored_component, prune_docs_orphans_with_store,
    };

    /// 断链软链必须算「产物在」:Windows 上「原样镜像」类文件的 docs 产物就是这个
    /// 形态。若按 is_file() 判定,每个镜像类文件每轮同步都被判缺失 → 整库重灌。
    #[test]
    fn docs_artifact_present_counts_dangling_symlink_as_present() {
        let base = std::env::temp_dir().join(format!("ktree_dap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("docs")).unwrap();
        std::fs::write(base.join("docs/real.md"), "hi").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("nowhere.md", base.join("docs/dangling.md")).unwrap();

        assert!(docs_artifact_present(&base, "docs/real.md"));
        assert!(!docs_artifact_present(&base, "docs/absent.md"));
        assert!(!docs_artifact_present(&base, ""));
        #[cfg(unix)]
        assert!(docs_artifact_present(&base, "docs/dangling.md"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn explicit_ignore_prefix_only() {
        assert!(path_has_ignored_component("vcs/svn/##!草稿/方案.md"));
        assert!(path_has_ignored_component("vcs/svn/##！草稿/方案.md"));
        assert!(path_has_ignored_component("vcs/svn/策划/##!方案.md"));
        assert!(!path_has_ignored_component("vcs/svn/归档（AI不看）/方案.md"));
        assert!(!path_has_ignored_component("vcs/svn/策划/方案##!.md"));
    }

    #[test]
    fn link_key_normalizes_basename() {
        assert_eq!(link_key_of("upload/A 方案.md"), "a 方案");
        assert_eq!(link_key_of("Note"), "note");
        assert_eq!(link_key_of("dir/Sub/X.MARKDOWN"), "x");
    }

    #[test]
    fn extract_links_wiki_and_md() {
        let body = "见 [[设计方案]] 与 [[人物#背景|人物设定]],外链 [站](https://x.com),\
                    本地 [配置](../config/表.md),锚点 [a](#sec) 不算。";
        let keys: Vec<String> = extract_links(body).into_iter().map(|(k, _)| k).collect();
        assert!(keys.contains(&"设计方案".to_string()));
        assert!(keys.contains(&"人物".to_string())); // 去别名 / 去锚点
        assert!(keys.contains(&"表".to_string())); // 相对 md 链接取 basename 去扩展名
        assert!(!keys.iter().any(|k| k.contains("x.com"))); // 外链排除
        assert!(!keys.iter().any(|k| k.is_empty())); // 纯锚点排除
    }

    #[test]
    fn frontmatter_props_skips_reserved_and_indent() {
        let md = "---\ntitle: 标题\ntags: [a, b]\nstatus: Done\nowner: yvo\n  nested: x\n---\n正文";
        let props = parse_frontmatter_props(md);
        // 保留用户键
        assert!(props.contains("\"status\""));
        assert!(props.contains("Done"));
        assert!(props.contains("\"owner\""));
        // 跳过保留键与缩进续行
        assert!(!props.contains("\"title\""));
        assert!(!props.contains("\"tags\""));
        assert!(!props.contains("nested"));
        // 无 frontmatter → 空
        assert_eq!(parse_frontmatter_props("正文无 frontmatter"), "");
    }

    #[test]
    fn prune_removes_outputs_whose_sources_were_deleted() {
        use crate::config::KnowledgeBase;
        use crate::store::{NewDocument, Store};
        use std::sync::atomic::{AtomicU64, Ordering};

        static C: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ktree_prune_{}_{}",
            std::process::id(),
            C.fetch_add(1, Ordering::SeqCst)
        ));
        let prefix = "vcs/svn";
        let stale_dir = root.join("docs/vcs/svn/docs/旧目录");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("旧转换.md"), "stale").unwrap();

        let store = Store::open(&root.join("test.db")).unwrap();
        let add_doc = |rel_path: &str, md_path: &str| {
            store
                .upsert_document(&NewDocument {
                    kb_id: "kb".to_string(),
                    rel_path: rel_path.to_string(),
                    title: "stale".to_string(),
                    ext: "md".to_string(),
                    size: 0,
                    md5: String::new(),
                    summary: String::new(),
                    tags: String::new(),
                    props: String::new(),
                    md_path: Some(md_path.to_string()),
                    source: "vcs".to_string(),
                })
                .unwrap();
        };
        add_doc(
            "vcs/svn/docs/旧目录/旧转换.docx",
            "docs/vcs/svn/docs/旧目录/旧转换.md",
        );

        let mut expected_removed = 1;
        #[cfg(unix)]
        {
            // Markdown / 文本源文件使用软链接镜像;源删除后会变成截图中的断链项。
            std::os::unix::fs::symlink(
                "../../../../../../src/vcs/svn/docs/旧目录/断链.md",
                stale_dir.join("断链.md"),
            )
            .unwrap();
            add_doc(
                "vcs/svn/docs/旧目录/断链.md",
                "docs/vcs/svn/docs/旧目录/断链.md",
            );
            expected_removed += 1;
        }

        let kb = KnowledgeBase {
            id: "kb".to_string(),
            name: "kb".to_string(),
            root: root.clone(),
            vcs_bindings: Vec::new(),
            cloud_bindings: Vec::new(),
        };
        let removed = prune_docs_orphans_with_store(&store, &kb, prefix).unwrap();
        assert_eq!(removed, expected_removed);
        assert!(!stale_dir.exists(), "清完断链文件后应递归删除空目录");

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }
}
