//! 混合检索:BM25(tantivy 字面匹配)+ 语义向量,用 RRF 融合排序。
//!
//! 向量这一路失败(embed sidecar 不可用、还没补算向量等)时自动退化为纯 BM25,
//! 语义只是增强项,不是硬依赖。

use std::collections::HashMap;

use crate::embed::QUERY_INSTRUCTION;
use crate::index::SearchHit;
use crate::kbmeta;
use crate::state::AppState;

/// Reciprocal Rank Fusion 常数(业界惯用 60):靠名次而非绝对分融合,
/// 免去 BM25 分与 cosine 分量纲不一致的归一化麻烦。
const RRF_K: f32 = 60.0;

/// 混合检索:BM25 与语义向量各取候选,RRF 融合后返回前 `limit` 条。
pub fn hybrid(
    state: &AppState,
    kb: Option<&str>,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<SearchHit>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.max(1);
    // 各路多取一些候选,给融合留出空间。
    let candidates = (limit * 3).max(20);

    let bm25 = state.index.search(kb, query, candidates)?;
    // 向量路失败不致命:退化成纯 BM25。
    let vector = vector_search(state, kb, query, candidates).unwrap_or_default();

    // RRF:每条结果在某一路里名次为 r(从 0 起),贡献 1/(K + r + 1)。
    let mut fused: HashMap<i64, f32> = HashMap::new();
    for (rank, h) in bm25.iter().enumerate() {
        *fused.entry(h.doc_id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    for (rank, (doc_id, _sim)) in vector.iter().enumerate() {
        *fused.entry(*doc_id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
    }

    // 内容惩罚:图片/视频等无正文的媒体文件只能靠文件名命中,融合分打 3 折 ——
    // 避免一张恰好同名的 PNG 排在真正讨论该主题的文本文档前面(检索噪音)。
    // 它们仍会出现在结果里(用户确实可能在找图),只是排序靠后。
    let mut ranked: Vec<(i64, f32)> = fused.into_iter().collect();
    for (doc_id, score) in ranked.iter_mut() {
        if let Ok(Some(doc)) = state.store.get_document(*doc_id) {
            if doc.summary.trim().is_empty() && is_media_ext(&doc.ext) {
                *score *= 0.3;
            }
        }
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.truncate(limit);

    // 组装:BM25 命中直接复用其 SearchHit(标题/摘要来自 tantivy 存储字段);
    // 仅被向量命中的,从 store 补元信息。
    let by_id: HashMap<i64, &SearchHit> = bm25.iter().map(|h| (h.doc_id, h)).collect();
    let mut out = Vec::with_capacity(ranked.len());
    for (doc_id, score) in ranked {
        if let Some(h) = by_id.get(&doc_id) {
            let mut hit = (*h).clone();
            hit.score = score;
            out.push(hit);
        } else if let Some(hit) = hit_from_store(state, doc_id, score) {
            out.push(hit);
        }
    }

    // 把 RRF 融合分归一化成 0-100 可读分:RRF 原始分很小且挤在一起,
    // 这里以「两路检索都排第一」的理论最高分为满分映射,便于按相关度筛选。
    let max_possible = 2.0 / (RRF_K + 1.0);
    for hit in &mut out {
        hit.score = (hit.score / max_possible * 100.0).clamp(0.0, 100.0);
    }
    Ok(out)
}

/// 把查询编码成向量,对 store 里所有文档向量暴力算 cosine(已归一化 → 即点积),取前 `limit`。
fn vector_search(
    state: &AppState,
    kb: Option<&str>,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<(i64, f32)>> {
    let qvecs = state
        .embedder
        .embed(&[query.to_string()], QUERY_INSTRUCTION)?;
    let qv = qvecs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("查询向量为空"))?;
    let mut scored: Vec<(i64, f32)> = state
        .store
        .all_vectors(kb)?
        .into_iter()
        .filter(|(_, v)| v.len() == qv.len())
        .map(|(id, v)| (id, dot(&qv, &v)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    Ok(scored)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// 图片 / 音视频 / 压缩包等不可能有可检索正文的扩展名。
fn is_media_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico"
            | "mp4" | "mov" | "webm" | "mkv" | "avi"
            | "mp3" | "wav" | "flac" | "ogg" | "aac"
            | "zip" | "rar" | "gz" | "tar" | "7z" | "bz2"
    )
}

/// 仅被语义命中的文档,从 SQLite 补出标题 / 摘要 / 分类。
fn hit_from_store(state: &AppState, doc_id: i64, score: f32) -> Option<SearchHit> {
    let doc = state.store.get_document(doc_id).ok().flatten()?;
    Some(SearchHit {
        category: kbmeta::category_of(&doc.rel_path),
        doc_id: doc.id,
        kb_id: doc.kb_id,
        title: doc.title,
        summary: doc.summary,
        score,
    })
}
