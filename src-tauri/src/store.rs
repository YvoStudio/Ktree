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

/// 记事板里的一条公共记事。`note_type`:text(纯文本)/ url(网络链接)/ kblink(知识库链接)。
/// kblink 时 `kb_id` 为目标知识库,`content` 为相对知识库根的路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub title: String,
    pub note_type: String,
    pub content: String,
    #[serde(default)]
    pub kb_id: String,
    /// 卡片背景色(空 = 默认)。
    #[serde(default)]
    pub color: String,
    /// 是否置顶。
    #[serde(default)]
    pub pinned: bool,
    pub created_at: i64,
}

/// 新建记事入参(id / 时间戳由 Store 填充)。
#[derive(Debug, Clone, Deserialize)]
pub struct NewNote {
    #[serde(default)]
    pub title: String,
    pub note_type: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub kb_id: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub pinned: bool,
}

/// 把小端序列化的 BLOB 还原成 f32 向量。
fn bytes_to_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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
            CREATE TABLE IF NOT EXISTS doc_vectors (
                doc_id  INTEGER PRIMARY KEY,
                dim     INTEGER NOT NULL,
                vec     BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS notes (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                title       TEXT NOT NULL DEFAULT '',
                note_type   TEXT NOT NULL DEFAULT 'text',
                content     TEXT NOT NULL DEFAULT '',
                kb_id       TEXT NOT NULL DEFAULT '',
                color       TEXT NOT NULL DEFAULT '',
                pinned      INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );
            "#,
        )?;
        // 老库补列(已存在则忽略报错)
        let _ = conn.execute(
            "ALTER TABLE notes ADD COLUMN color TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE notes ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            [],
        );
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

    /// 只更新一篇文档的 summary / tags(摘要 / 关键词补算用)。
    pub fn update_meta(&self, id: i64, summary: &str, tags: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE documents SET summary = ?1, tags = ?2, updated_at = ?3 WHERE id = ?4",
            params![summary, tags, now(), id],
        )?;
        Ok(())
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
        conn.execute("DELETE FROM doc_vectors WHERE doc_id = ?1", params![id])?;
        let n = conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// 清空某知识库的全部文档记录(缓存重建前用)。
    pub fn delete_by_kb(&self, kb_id: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM doc_vectors WHERE doc_id IN
                (SELECT id FROM documents WHERE kb_id = ?1)",
            params![kb_id],
        )?;
        let n = conn.execute("DELETE FROM documents WHERE kb_id = ?1", params![kb_id])?;
        Ok(n)
    }

    // ----- 语义向量(doc_vectors)-----

    /// 存 / 更新一篇文档的语义向量(f32 按小端序列化成 BLOB)。
    pub fn set_vector(&self, doc_id: i64, vec: &[f32]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let bytes: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO doc_vectors (doc_id, dim, vec) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_id) DO UPDATE SET dim = ?2, vec = ?3",
            params![doc_id, vec.len() as i64, bytes],
        )?;
        Ok(())
    }

    /// 取某知识库(None = 全部)所有文档的向量,供检索时暴力算相似度。
    pub fn all_vectors(&self, kb_id: Option<&str>) -> anyhow::Result<Vec<(i64, Vec<f32>)>> {
        let conn = self.conn.lock().unwrap();
        let to_pair =
            |row: &rusqlite::Row| -> rusqlite::Result<(i64, Vec<u8>)> {
                Ok((row.get(0)?, row.get(1)?))
            };
        let pairs: Vec<(i64, Vec<u8>)> = match kb_id {
            Some(kb) => {
                let mut stmt = conn.prepare(
                    "SELECT v.doc_id, v.vec FROM doc_vectors v
                     JOIN documents d ON d.id = v.doc_id WHERE d.kb_id = ?1",
                )?;
                let rows = stmt.query_map(params![kb], to_pair)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare("SELECT doc_id, vec FROM doc_vectors")?;
                let rows = stmt.query_map([], to_pair)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(pairs
            .into_iter()
            .map(|(id, bytes)| (id, bytes_to_vec(&bytes)))
            .collect())
    }

    /// 列出 kb_id 已不在配置里的孤儿文档 id —— 知识库改名 / 删除会留下这些。
    pub fn orphan_doc_ids(&self, valid_kb_ids: &[String]) -> anyhow::Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, kb_id FROM documents")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, kb) = r?;
            if !valid_kb_ids.iter().any(|v| v == &kb) {
                out.push(id);
            }
        }
        Ok(out)
    }

    // ----- 记事板(notes)-----

    /// 列出全部公共记事,最新的在前。
    pub fn list_notes(&self) -> anyhow::Result<Vec<Note>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, note_type, content, kb_id, color, pinned, created_at
             FROM notes ORDER BY pinned DESC, created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Note {
                id: r.get(0)?,
                title: r.get(1)?,
                note_type: r.get(2)?,
                content: r.get(3)?,
                kb_id: r.get(4)?,
                color: r.get(5)?,
                pinned: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 新增一条公共记事,返回完整 Note。
    pub fn add_note(&self, n: &NewNote) -> anyhow::Result<Note> {
        let conn = self.conn.lock().unwrap();
        let ts = now();
        conn.execute(
            "INSERT INTO notes (title, note_type, content, kb_id, color, pinned, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![n.title, n.note_type, n.content, n.kb_id, n.color, n.pinned, ts],
        )?;
        Ok(Note {
            id: conn.last_insert_rowid(),
            title: n.title.clone(),
            note_type: n.note_type.clone(),
            content: n.content.clone(),
            kb_id: n.kb_id.clone(),
            color: n.color.clone(),
            pinned: n.pinned,
            created_at: ts,
        })
    }

    /// 更新一条公共记事,返回是否命中。
    pub fn update_note(&self, id: i64, n: &NewNote) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE notes SET title = ?1, note_type = ?2, content = ?3, kb_id = ?4,
                color = ?5, pinned = ?6 WHERE id = ?7",
            params![n.title, n.note_type, n.content, n.kb_id, n.color, n.pinned, id],
        )?;
        Ok(rows > 0)
    }

    /// 删除一条公共记事。
    pub fn delete_note(&self, id: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM notes WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// 列出还没有语义向量的文档(供启动时给存量文档补算)。
    pub fn docs_missing_vector(&self) -> anyhow::Result<Vec<Document>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT d.* FROM documents d
             LEFT JOIN doc_vectors v ON v.doc_id = d.id
             WHERE v.doc_id IS NULL",
        )?;
        let rows = stmt.query_map([], Self::row_to_doc)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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
