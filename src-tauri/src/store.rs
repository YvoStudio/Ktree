use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// 知识库中的一篇文档。这是 app_data 里的可重建缓存,真相源是各知识库目录的文件。
/// rel_path 是 src/ 下的相对路径(含子目录),用正斜杠。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: i64,
    pub kb_id: String,
    /// src/ 下的相对路径,如 "策划文档/需求.docx"
    pub rel_path: String,
    pub title: String,
    pub ext: String,
    pub size: i64,
    pub md5: String,
    pub summary: String,
    /// 逗号分隔的标签(来自 frontmatter tags)
    pub tags: String,
    /// 转换后的 Markdown 相对知识库根的路径(docs/<rel_path>.md),未转换则 None
    pub md_path: Option<String>,
    /// 来源:upload / feishu / local
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 新建/更新文档时的入参(id 与时间戳由 Store 填充)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewDocument {
    pub kb_id: String,
    pub rel_path: String,
    pub title: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub md_path: Option<String>,
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    "upload".to_string()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS documents (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                kb_id       TEXT NOT NULL,
                rel_path    TEXT NOT NULL,
                title       TEXT NOT NULL,
                ext         TEXT NOT NULL DEFAULT '',
                size        INTEGER NOT NULL DEFAULT 0,
                md5         TEXT NOT NULL DEFAULT '',
                summary     TEXT NOT NULL DEFAULT '',
                tags        TEXT NOT NULL DEFAULT '',
                md_path     TEXT,
                source      TEXT NOT NULL DEFAULT 'upload',
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                UNIQUE(kb_id, rel_path)
            );
            CREATE INDEX IF NOT EXISTS idx_documents_kb ON documents(kb_id);
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn row_to_doc(row: &rusqlite::Row) -> rusqlite::Result<Document> {
        Ok(Document {
            id: row.get("id")?,
            kb_id: row.get("kb_id")?,
            rel_path: row.get("rel_path")?,
            title: row.get("title")?,
            ext: row.get("ext")?,
            size: row.get("size")?,
            md5: row.get("md5")?,
            summary: row.get("summary")?,
            tags: row.get("tags")?,
            md_path: row.get("md_path")?,
            source: row.get("source")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    /// 按 (kb_id, rel_path) 插入或更新,返回文档 id。
    pub fn upsert_document(&self, doc: &NewDocument) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let ts = now();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM documents WHERE kb_id = ?1 AND rel_path = ?2",
                params![doc.kb_id, doc.rel_path],
                |r| r.get(0),
            )
            .optional()?;

        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE documents SET title=?1, ext=?2, size=?3, md5=?4, summary=?5,
                        tags=?6, md_path=?7, source=?8, updated_at=?9 WHERE id=?10",
                    params![
                        doc.title, doc.ext, doc.size, doc.md5, doc.summary, doc.tags,
                        doc.md_path, doc.source, ts, id
                    ],
                )?;
                Ok(id)
            }
            None => {
                conn.execute(
                    "INSERT INTO documents
                        (kb_id, rel_path, title, ext, size, md5, summary, tags, md_path,
                         source, created_at, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)",
                    params![
                        doc.kb_id, doc.rel_path, doc.title, doc.ext, doc.size, doc.md5,
                        doc.summary, doc.tags, doc.md_path, doc.source, ts
                    ],
                )?;
                Ok(conn.last_insert_rowid())
            }
        }
    }

    pub fn get_document(&self, id: i64) -> anyhow::Result<Option<Document>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM documents WHERE id = ?1",
                params![id],
                Self::row_to_doc,
            )
            .optional()?)
    }

    pub fn get_by_path(&self, kb_id: &str, rel_path: &str) -> anyhow::Result<Option<Document>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM documents WHERE kb_id = ?1 AND rel_path = ?2",
                params![kb_id, rel_path],
                Self::row_to_doc,
            )
            .optional()?)
    }

    /// 列某知识库的文档;path_prefix 非空时只返回该目录前缀下的(递归)。按 rel_path 排序。
    pub fn list_documents(
        &self,
        kb_id: &str,
        path_prefix: Option<&str>,
    ) -> anyhow::Result<Vec<Document>> {
        let conn = self.conn.lock().unwrap();
        let mut docs = Vec::new();
        match path_prefix.filter(|p| !p.is_empty()) {
            Some(prefix) => {
                let like = format!("{}/%", prefix.trim_end_matches('/'));
                let mut stmt = conn.prepare(
                    "SELECT * FROM documents WHERE kb_id = ?1 AND rel_path LIKE ?2
                     ORDER BY rel_path",
                )?;
                let rows = stmt.query_map(params![kb_id, like], Self::row_to_doc)?;
                for r in rows {
                    docs.push(r?);
                }
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT * FROM documents WHERE kb_id = ?1 ORDER BY rel_path",
                )?;
                let rows = stmt.query_map(params![kb_id], Self::row_to_doc)?;
                for r in rows {
                    docs.push(r?);
                }
            }
        }
        Ok(docs)
    }

    pub fn delete_document(&self, id: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// 清空某知识库的全部文档记录(缓存重建前用)。
    pub fn delete_by_kb(&self, kb_id: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM documents WHERE kb_id = ?1", params![kb_id])?;
        Ok(n)
    }

    /// 统计文档数;kb_id 为 None 时统计全部。
    pub fn count_documents(&self, kb_id: Option<&str>) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = match kb_id {
            Some(kb) => conn.query_row(
                "SELECT COUNT(*) FROM documents WHERE kb_id = ?1",
                params![kb],
                |r| r.get(0),
            )?,
            None => conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?,
        };
        Ok(n)
    }
}
