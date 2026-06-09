use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
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

        // 中文 jieba 分词后逐词做 OR(任一词命中即可,相关度交给 BM25 打分)。
        //
        // 不能把整串直接交给 QueryParser:tantivy 会把"分词出的多个词元"编译成
        // 必须相邻的 PhraseQuery(本索引带 positions),于是"配置表规范"匹配不到
        // "配置表生成规范"(中间隔着"生成")。这里手动把每个词元拆成 Should 词项查询,
        // 绕开短语相邻约束,回到真正的 OR 语义。
        let mut analyzer = self
            .index
            .tokenizers()
            .get(JIEBA)
            .expect("jieba tokenizer registered at open()");
        let mut token_stream = analyzer.token_stream(trimmed);
        let mut seen = std::collections::HashSet::new();
        let mut term_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        token_stream.process(&mut |token| {
            if !seen.insert(token.text.clone()) {
                return; // 同一词元只加一次,避免重复计分
            }
            for field in [self.fields.title, self.fields.body] {
                let term = Term::from_field_text(field, &token.text);
                term_clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)),
                ));
            }
        });

        if term_clauses.is_empty() {
            return Ok(Vec::new());
        }
        let user_query: Box<dyn Query> = Box::new(BooleanQuery::new(term_clauses));

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
}
