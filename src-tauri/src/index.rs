use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED,
    STORED, STRING,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

const JIEBA: &str = "jieba";
const WRITER_HEAP: usize = 50_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub doc_id: i64,
    pub kb_id: String,
    pub title: String,
    pub category: String,
    pub summary: String,
    pub score: f32,
}

struct Fields {
    doc_id: Field,
    kb_id: Field,
    title: Field,
    category: Field,
    body: Field,
    summary: Field,
}

/// tantivy 全文索引(app_data 里的可重建缓存)。多知识库共用一个索引,按 kb_id 字段隔离。
pub struct SearchIndex {
    index: Index,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    fields: Fields,
}

impl SearchIndex {
    pub fn open(index_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(index_dir)?;

        let cn_text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JIEBA)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );

        let mut builder = Schema::builder();
        let doc_id = builder.add_u64_field("doc_id", STORED | INDEXED | FAST);
        let kb_id = builder.add_text_field("kb_id", STRING | STORED | FAST);
        let title = builder.add_text_field("title", cn_text.clone() | STORED);
        let body = builder.add_text_field("body", cn_text);
        let category = builder.add_text_field("category", STRING | STORED);
        let summary = builder.add_text_field("summary", STORED);
        let schema = builder.build();

        let dir = tantivy::directory::MmapDirectory::open(index_dir)?;
        let index = Index::open_or_create(dir, schema)?;
        index
            .tokenizers()
            .register(JIEBA, tantivy_jieba::JiebaTokenizer {});

        let writer: IndexWriter = index.writer(WRITER_HEAP)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        Ok(Self {
            index,
            writer: Mutex::new(writer),
            reader,
            fields: Fields {
                doc_id,
                kb_id,
                title,
                category,
                body,
                summary,
            },
        })
    }

    /// 新增或覆盖一篇文档的索引(先按 doc_id 删旧再加新)。不 commit。
    pub fn add_or_update(
        &self,
        kb_id: &str,
        doc_id: i64,
        title: &str,
        category: &str,
        body: &str,
        summary: &str,
    ) -> anyhow::Result<()> {
        let id = doc_id as u64;
        let writer = self.writer.lock().unwrap();
        writer.delete_term(Term::from_field_u64(self.fields.doc_id, id));

        let mut doc = TantivyDocument::default();
        doc.add_u64(self.fields.doc_id, id);
        doc.add_text(self.fields.kb_id, kb_id);
        doc.add_text(self.fields.title, title);
        doc.add_text(self.fields.category, category);
        doc.add_text(self.fields.body, body);
        doc.add_text(self.fields.summary, summary);
        writer.add_document(doc)?;
        Ok(())
    }

    /// 删除一篇文档的索引。不 commit。
    pub fn delete(&self, doc_id: i64) -> anyhow::Result<()> {
        let writer = self.writer.lock().unwrap();
        writer.delete_term(Term::from_field_u64(self.fields.doc_id, doc_id as u64));
        Ok(())
    }

    /// 删除某知识库的全部索引(缓存重建前用)。不 commit。
    pub fn delete_by_kb(&self, kb_id: &str) -> anyhow::Result<()> {
        let writer = self.writer.lock().unwrap();
        writer.delete_term(Term::from_field_text(self.fields.kb_id, kb_id));
        Ok(())
    }

    /// 提交所有挂起的增删改,使其对搜索可见。
    pub fn commit(&self) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.commit()?;
        Ok(())
    }

    /// BM25 全文搜索 title + body。kb_id 非空时只搜该知识库。
    pub fn search(
        &self,
        kb_id: Option<&str>,
        query_str: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let trimmed = query_str.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        // 中文 jieba 切词后逐词匹配。两个极端都要避开:
        //   ·「整串当短语」→ tantivy 把多词元编成必须相邻的 PhraseQuery(本索引带
        //     positions),"配置表规范"匹配不到"配置表生成规范"(中间隔着"生成")—— 漏召回。
        //   ·「纯 OR」→ 单字 / 高频词(如"表""配置")把一堆只蹭一个词的文档拉进来 —— 噪声。
        // 折中三件事:
        //   ① OR 召回 + minimum-should-match:至少命中查询词的多数才算候选,压掉
        //      「只匹配一个词」的噪声。tantivy 0.22 无原生 MSM,用「C(n,k) 组合各自 AND、
        //      组合之间 OR」表达,查询词数少,组合可控。
        //   ② 短语 / 相邻 bigram 的 slop 加权:词元在文档里挨得近、顺序对的额外加分,
        //      把旧 PhraseQuery 的精度当作「排序信号」找回来,但只加分不过滤,不牺牲召回。
        let mut analyzer = self
            .index
            .tokenizers()
            .get(JIEBA)
            .expect("jieba tokenizer registered at open()");
        let mut token_stream = analyzer.token_stream(trimmed);
        let mut toks: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        token_stream.process(&mut |token| {
            // 去重,但保留出现顺序(短语加权要按原序)
            if seen.insert(token.text.clone()) {
                toks.push(token.text.clone());
            }
        });
        if toks.is_empty() {
            return Ok(Vec::new());
        }

        // 单个词元 → 在 title / body 任一命中(OR)。Query 非 Clone,故每次新建。
        let token_query = |t: &str| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(vec![
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.title, t),
                        IndexRecordOption::WithFreqs,
                    )) as Box<dyn Query>,
                ),
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.body, t),
                        IndexRecordOption::WithFreqs,
                    )),
                ),
            ]))
        };

        // ① 召回主体:至少命中 msm 个不同词元(≈60%)。
        let n = toks.len();
        let msm = (((n as f32) * 0.6).ceil() as usize).clamp(1, n);
        let recall: Box<dyn Query> = if msm <= 1 || n > 6 {
            // msm=1,或词太多(组合数会爆炸)→ 退化成纯 OR
            Box::new(BooleanQuery::new(
                toks.iter().map(|t| (Occur::Should, token_query(t))).collect(),
            ))
        } else {
            // 「n 选 msm」的所有组合:组合内部 AND(Must),组合之间 OR(Should)
            let mut groups: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for combo in combinations(n, msm) {
                let ands: Vec<(Occur, Box<dyn Query>)> = combo
                    .iter()
                    .map(|&i| (Occur::Must, token_query(&toks[i])))
                    .collect();
                groups.push((Occur::Should, Box::new(BooleanQuery::new(ands))));
            }
            Box::new(BooleanQuery::new(groups))
        };

        // ② 短语 / 邻近加权(Should,只加分不过滤)。
        let phrase_boost = |field: Field, terms: &[&str], slop: u32, boost: f32| -> Box<dyn Query> {
            let mut pq = PhraseQuery::new(
                terms
                    .iter()
                    .map(|t| Term::from_field_text(field, t))
                    .collect(),
            );
            pq.set_slop(slop);
            Box::new(BoostQuery::new(Box::new(pq), boost))
        };
        let mut scored: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Should, recall)];
        if n >= 2 {
            let all: Vec<&str> = toks.iter().map(|s| s.as_str()).collect();
            for field in [self.fields.title, self.fields.body] {
                // 全词按序、容隔(slop 宽松):真正"在讲这个主题"的文档
                scored.push((Occur::Should, phrase_boost(field, &all, (n as u32) * 3, 2.0)));
                // 相邻 bigram(slop=1):捕捉"配置表"这种被 jieba 拆开、本是一体的词
                for w in all.windows(2) {
                    scored.push((Occur::Should, phrase_boost(field, w, 1, 1.5)));
                }
            }
        }
        let user_query: Box<dyn Query> = Box::new(BooleanQuery::new(scored));

        let final_query: Box<dyn Query> = match kb_id.filter(|k| !k.is_empty()) {
            Some(kb) => {
                let kb_term = Term::from_field_text(self.fields.kb_id, kb);
                let kb_query = TermQuery::new(kb_term, IndexRecordOption::Basic);
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, user_query),
                    (Occur::Must, Box::new(kb_query)),
                ]))
            }
            None => user_query,
        };

        let searcher = self.reader.searcher();
        let top = searcher.search(&final_query, &TopDocs::with_limit(limit.max(1)))?;

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            hits.push(SearchHit {
                doc_id: get_u64(&doc, self.fields.doc_id) as i64,
                kb_id: get_str(&doc, self.fields.kb_id),
                title: get_str(&doc, self.fields.title),
                category: get_str(&doc, self.fields.category),
                summary: get_str(&doc, self.fields.summary),
                score,
            });
        }
        Ok(hits)
    }
}

fn get_u64(doc: &TantivyDocument, field: Field) -> u64 {
    doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0)
}

fn get_str(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// 0..n 里所有大小为 k 的下标组合(字典序)。用于把 minimum-should-match
/// 表达成「C(n,k) 个 AND 组合的 OR」。调用方保证 1 <= k <= n 且 n 不大。
fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    if k == 0 || k > n {
        return out;
    }
    let mut idx: Vec<usize> = (0..k).collect();
    loop {
        out.push(idx.clone());
        // 从右往左找第一个还能自增的位置(其上限是 i + n - k)
        let mut i = k;
        loop {
            if i == 0 {
                return out;
            }
            i -= 1;
            if idx[i] != i + n - k {
                break;
            }
        }
        idx[i] += 1;
        for j in (i + 1)..k {
            idx[j] = idx[j - 1] + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 给每个测试一个独立的临时索引目录(无需 tempfile 依赖)。
    fn temp_index() -> (SearchIndex, std::path::PathBuf) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ktree_idx_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SearchIndex::open(&dir).unwrap();
        (index, dir)
    }

    /// 回归:查询词在标题里"不连续"(中间隔着别的词)时也应命中。
    /// 历史 bug:整串交给 QueryParser 被编译成相邻短语,"配置表规范"搜不到
    /// "配置表生成规范"。
    #[test]
    fn non_contiguous_query_matches() {
        let (index, dir) = temp_index();
        index
            .add_or_update("吃播", 1, "配置表生成规范", "未分类", "配置表定义规则", "")
            .unwrap();
        index.commit().unwrap();
        index.reader.reload().unwrap(); // OnCommitWithDelay:单测里需手动刷新可见性

        let hits = index.search(Some("吃播"), "配置表规范", 10).unwrap();
        assert!(
            hits.iter().any(|h| h.doc_id == 1),
            "「配置表规范」应命中「配置表生成规范」,实际: {:?}",
            hits.iter().map(|h| &h.title).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 完全不相关的查询不应误命中。
    #[test]
    fn unrelated_query_misses() {
        let (index, dir) = temp_index();
        index
            .add_or_update("吃播", 1, "配置表生成规范", "未分类", "配置表定义规则", "")
            .unwrap();
        index.commit().unwrap();
        index.reader.reload().unwrap(); // OnCommitWithDelay:单测里需手动刷新可见性

        let hits = index.search(Some("吃播"), "战斗数值公式", 10).unwrap();
        assert!(
            !hits.iter().any(|h| h.doc_id == 1),
            "无关查询不应命中"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn combinations_works() {
        assert_eq!(combinations(3, 2), vec![vec![0, 1], vec![0, 2], vec![1, 2]]);
        assert_eq!(combinations(2, 2), vec![vec![0, 1]]);
        assert_eq!(combinations(4, 1), vec![vec![0], vec![1], vec![2], vec![3]]);
        assert_eq!(combinations(3, 0), Vec::<Vec<usize>>::new());
    }

    /// 精度:MSM 压噪 + 短语加权排序。语料模拟线上「配置表规范」的实际结果集。
    /// 期望:近义标题命中稳居第一;只蹭单个高频词「配置」的噪声被 MSM 过滤掉。
    #[test]
    fn precision_msm_and_phrase_ranking() {
        let (index, dir) = temp_index();
        // doc1: 近义标题,含 配置/表/规范 —— 应排第一
        index
            .add_or_update("吃播", 1, "配置表生成规范", "未分类", "配置表定义规则 表名 sheet 名 全部使用小写字母", "")
            .unwrap();
        // doc2: 同享「配置表」概念,缺「规范」—— 应保留且靠前
        index
            .add_or_update("吃播", 2, "配置表索引", "svn", "本文件由脚本自动生成 配置表 config 源目录", "")
            .unwrap();
        // doc3: 只命中单个高频词「配置」(无 表 / 规范)—— 噪声,应被 MSM 过滤
        index
            .add_or_update("吃播", 3, "order_配置修改说明", "svn", "对照文档 配置 修改 记录 差异 校正", "")
            .unwrap();
        // doc4: 命中 配置 + 规范 但非「配置表」主题 —— 可保留但应排在 doc1/doc2 之后
        index
            .add_or_update("吃播", 4, "家具分类规范", "svn", "家具 分类 配置说明 来源 美术", "")
            .unwrap();
        // doc5: 完全无关 —— 不应出现
        index
            .add_or_update("吃播", 5, "战斗数值公式", "svn", "伤害 暴击 防御", "")
            .unwrap();
        index.commit().unwrap();
        index.reader.reload().unwrap();

        let hits = index.search(Some("吃播"), "配置表规范", 10).unwrap();
        let order: Vec<i64> = hits.iter().map(|h| h.doc_id).collect();
        eprintln!(
            "排名: {:?}",
            hits.iter()
                .map(|h| format!("{}#{:.2}", h.title, h.score))
                .collect::<Vec<_>>()
        );

        assert_eq!(order.first(), Some(&1), "近义标题「配置表生成规范」应排第一");
        assert!(!order.contains(&3), "只蹭单个「配置」的噪声应被 MSM 过滤");
        assert!(!order.contains(&5), "完全无关文档不应出现");
        let p = |id: i64| order.iter().position(|&x| x == id);
        assert!(p(1) < p(4), "「配置表生成规范」应排在「家具分类规范」之前");
        assert!(
            p(2).map_or(false, |x| x < p(4).unwrap_or(usize::MAX)),
            "同享「配置表」的 doc2 应排在弱相关 doc4 之前"
        );
    }
}
