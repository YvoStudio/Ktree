//! 混合检索:BM25(tantivy 字面匹配)+ 语义向量,用 RRF 融合排序。
//!
//! 向量这一路失败(embed sidecar 不可用、还没补算向量等)时自动退化为纯 BM25,
//! 语义只是增强项,不是硬依赖。

use std::collections::{HashMap, HashSet};

use crate::embed::QUERY_INSTRUCTION;
use crate::index::SearchHit;
use crate::ingest;
use crate::kbmeta;
use crate::query_parser;
use crate::state::AppState;
use crate::store::Document;

/// Reciprocal Rank Fusion 常数(业界惯用 60):靠名次而非绝对分融合,
/// 免去 BM25 分与 cosine 分量纲不一致的归一化麻烦。
const RRF_K: f32 = 60.0;

/// 混合检索:BM25 与语义向量各取候选,RRF 融合后返回前 `limit` 条。
///
/// query 支持结构化算子(tag:/path:/type:/kb:/title: + 取反 / 引号),解析见 query_parser。
/// 算子部分走 SQLite 过滤得到 allowed 集合,对两路结果取交集;不进 tantivy、不重建索引。
/// 只给算子不给自由文本时,退化为「按条件列举」(过滤-only,按更新时间倒序)。
pub fn hybrid(
    state: &AppState,
    kb: Option<&str>,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<SearchHit>> {
    let (free_text, constraints) = query_parser::parse(query);
    let free_text = free_text.trim();
    let limit = limit.max(1);

    // 算子约束 → 允许的 doc_id 集合(None = 无约束,不过滤)。
    let allowed: Option<HashSet<i64>> = if constraints.is_empty() {
        None
    } else {
        Some(state.store.doc_ids_matching(kb, &constraints)?)
    };

    // 纯算子无自由文本:没有相关度可言,直接按约束列举(更新时间倒序)。
    if free_text.is_empty() {
        return Ok(match &allowed {
            Some(ids) => filter_only_hits(state, ids, limit),
            None => Vec::new(),
        });
    }

    // 有约束时放大候选池:过滤后仍要凑够 limit。向量路对 allowed 是完整覆盖
    //(过滤在截断前),BM25 路放大兜底即可。
    let candidates = if allowed.is_some() {
        (limit * 20).max(200)
    } else {
        (limit * 3).max(20)
    };

    let bm25: Vec<SearchHit> = state
        .index
        .search(kb, free_text, candidates)?
        .into_iter()
        .filter(|h| !is_ignored_doc_id(state, h.doc_id))
        .filter(|h| allowed.as_ref().map_or(true, |s| s.contains(&h.doc_id)))
        .collect();
    // 向量路失败不致命:退化成纯 BM25。
    let vector: Vec<(i64, f32)> = vector_search(state, kb, free_text, candidates, allowed.as_ref())
        .unwrap_or_default()
        .into_iter()
        .filter(|(doc_id, _)| !is_ignored_doc_id(state, *doc_id))
        .collect();

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
            if ingest::path_has_ignored_component(&doc.rel_path) {
                *score = 0.0;
                continue;
            }
            if doc.summary.trim().is_empty() && is_media_ext(&doc.ext) {
                *score *= 0.3;
            }
        }
    }
    ranked.retain(|(doc_id, _)| !is_ignored_doc_id(state, *doc_id));
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
/// `allowed` 非 None 时,在截断前先按约束过滤,保证被约束命中的文档不会因排在候选外而漏掉。
fn vector_search(
    state: &AppState,
    kb: Option<&str>,
    query: &str,
    limit: usize,
    allowed: Option<&HashSet<i64>>,
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
        .filter(|(id, v)| {
            v.len() == qv.len() && allowed.map_or(true, |s| s.contains(id))
        })
        .map(|(id, v)| (id, dot(&qv, &v)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    Ok(scored)
}

/// 纯算子(无自由文本)时的「过滤-only 浏览」:把 allowed 集合按更新时间倒序列出。
/// 无相关度评分,统一给满分(每条都精确满足过滤条件),顺序由本函数排定、下游不再重排。
fn filter_only_hits(state: &AppState, ids: &HashSet<i64>, limit: usize) -> Vec<SearchHit> {
    let mut docs: Vec<Document> = ids
        .iter()
        .filter_map(|id| state.store.get_document(*id).ok().flatten())
        .filter(|d| !ingest::path_has_ignored_component(&d.rel_path))
        .collect();
    // 更新时间倒序,同刻按 id 倒序(新的在前),稳定可预期。
    docs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
    docs.truncate(limit);
    docs.into_iter()
        .map(|doc| SearchHit {
            category: kbmeta::category_of(&doc.rel_path),
            doc_id: doc.id,
            kb_id: doc.kb_id,
            title: doc.title,
            summary: doc.summary,
            score: 100.0,
        })
        .collect()
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

fn is_ignored_doc_id(state: &AppState, doc_id: i64) -> bool {
    state
        .store
        .get_document(doc_id)
        .ok()
        .flatten()
        .map(|doc| ingest::path_has_ignored_component(&doc.rel_path))
        .unwrap_or(false)
}

/// 仅被语义命中的文档,从 SQLite 补出标题 / 摘要 / 分类。
fn hit_from_store(state: &AppState, doc_id: i64, score: f32) -> Option<SearchHit> {
    let doc = state.store.get_document(doc_id).ok().flatten()?;
    if ingest::path_has_ignored_component(&doc.rel_path) {
        return None;
    }
    Some(SearchHit {
        category: kbmeta::category_of(&doc.rel_path),
        doc_id: doc.id,
        kb_id: doc.kb_id,
        title: doc.title,
        summary: doc.summary,
        score,
    })
}
