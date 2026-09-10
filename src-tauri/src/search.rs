//! 全文检索（FR-06 / T1.4）
//!
//! - ≥3 字符：FTS5 trigram + bm25() + snippet()
//! - 1–2 字符：退化 LIKE '%kw%'
//! - 检索范围：正文、标题、目录路径、附件文件名（133 个全量）
//! - 结果按 body_fingerprint 分组去重（P8）
//! - 历史最近 20 条

use std::path::Path;

use rusqlite::Connection;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub guid: String,
    pub title: String,
    pub location: String,
    pub data_modified: String,
    pub fingerprint: String,
    /// 命中片段（已含 <mark> 标记）
    pub snippet: String,
    /// 标题命中标记（无命中则空）
    pub title_hl: String,
    /// 该指纹下的副本总数（含自身），去重折叠用
    pub dup_count: i64,
    /// 命中来源：body / title
    pub hit_in: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AttachmentHit {
    pub file_path: String,
    pub display_name: String,
    pub document_guid: Option<String>,
    pub tier: i64,
    pub source: String,
    pub size: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResponse {
    pub kw: String,
    pub notes: Vec<SearchResult>,
    pub attachments: Vec<AttachmentHit>,
    pub elapsed_ms: u128,
}

fn quote_fts(kw: &str) -> String {
    format!("\"{}\"", kw.replace('"', "\"\""))
}

pub fn search(index_db: &Path, kw: &str, folder: Option<&str>) -> Result<SearchResponse, String> {
    let t0 = std::time::Instant::now();
    let conn = Connection::open_with_flags(
        index_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| e.to_string())?;
    let kw_trim = kw.trim();
    if kw_trim.is_empty() {
        return Ok(SearchResponse {
            kw: kw_trim.into(),
            notes: vec![],
            attachments: vec![],
            elapsed_ms: t0.elapsed().as_millis(),
        });
    }

    let folder_filter = |loc: &str| -> bool {
        match folder {
            Some(f) if !f.is_empty() => loc.starts_with(f),
            _ => true,
        }
    };

    let mut notes: Vec<SearchResult> = Vec::new();
    if kw_trim.chars().count() >= 3 {
        // FTS5 trigram + bm25
        let sql = format!(
            "SELECT n.guid, n.title, n.location, n.data_modified, n.body_fingerprint,
                    snippet(note_fts, 2, '<mark>', '</mark>', '…', 20) AS snip,
                    highlight(note_fts, 1, '<mark>', '</mark>') AS thl,
                    bm25(note_fts) AS rank
             FROM note_fts
             JOIN note n ON n.guid = note_fts.guid
             WHERE note_fts MATCH ?1
             ORDER BY rank
             LIMIT 200"
        );
        let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map([quote_fts(kw_trim)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows.flatten() {
            if !folder_filter(&row.2) {
                continue;
            }
            notes.push(SearchResult {
                guid: row.0,
                title: row.1.clone(),
                location: row.2,
                data_modified: row.3,
                fingerprint: row.4.unwrap_or_default(),
                snippet: row.5,
                title_hl: row.6.clone(),
                dup_count: 1,
                hit_in: if row.6.contains("<mark>") { "title" } else { "body" }.into(),
            });
        }
    } else {
        // 退化 LIKE（1–2 字符）
        let pat = format!("%{}%", kw_trim.replace('%', ""));
        let sql = "SELECT n.guid, n.title, n.location, n.data_modified, n.body_fingerprint,
                   CASE WHEN instr(f.body, ?1) > 0
                        THEN substr(f.body, max(1, instr(f.body, ?1) - 40), 120)
                        ELSE '' END
                   FROM note_fts f JOIN note n ON n.guid = f.guid
                   WHERE f.body LIKE ?2 OR f.title LIKE ?2 OR f.folder LIKE ?2
                   LIMIT 200";
        let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map(rusqlite::params![kw_trim, pat], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows.flatten() {
            if !folder_filter(&row.2) {
                continue;
            }
            let hit_in = if row.1.contains(kw_trim) { "title" } else { "body" };
            let body_snip = if row.5.is_empty() {
                String::new()
            } else {
                row.5.replace(kw_trim, &format!("<mark>{}</mark>", kw_trim))
            };
            notes.push(SearchResult {
                guid: row.0,
                title: row.1.clone(),
                location: row.2,
                data_modified: row.3,
                fingerprint: row.4.unwrap_or_default(),
                snippet: body_snip,
                title_hl: row.1.replace(kw_trim, &format!("<mark>{}</mark>", kw_trim)),
                dup_count: 1,
                hit_in: hit_in.into(),
            });
        }
    }

    // 正文指纹去重（P8）：按 fingerprint 分组，取每组 bm25 最先的一条代表
    {
        let fps: Vec<String> = notes.iter().map(|n| n.fingerprint.clone()).collect();
        let mut counts: HashMap<String, i64> = HashMap::new();
        for fp in &fps {
            *counts.entry(fp.clone()).or_insert(0) += 1;
        }
        // 还需加上未命中但同指纹的副本数（dup 组内其他篇也可能未命中关键词？不会——同正文必同命中）
        let mut seen: HashSet<String> = HashSet::new();
        notes.retain(|n| {
            if n.fingerprint.is_empty() {
                return true;
            }
            seen.insert(n.fingerprint.clone())
        });
        for n in &mut notes {
            n.dup_count = counts.get(&n.fingerprint).copied().unwrap_or(1);
        }
    }

    // ---- 附件文件名检索（133 个全量，含 Tier4 与 DB 缺失）----
    let mut attachments: Vec<AttachmentHit> = Vec::new();
    let pat = format!("%{}%", kw_trim.replace('%', ""));
    let sql = "SELECT file_path, attachment_guid, document_guid, display_name, tier, source, size, db_name
               FROM attachment
               WHERE display_name LIKE ?1 OR db_name LIKE ?1
               ORDER BY tier, display_name";
    let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows = st
        .query_map([&pat], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows.flatten() {
        attachments.push(AttachmentHit {
            file_path: row.0,
            display_name: row.3,
            document_guid: row.2,
            tier: row.4,
            source: row.5,
            size: row.6,
        });
    }

    // 记录检索历史
    let _ = add_history(&conn, kw_trim);

    Ok(SearchResponse {
        kw: kw_trim.into(),
        notes,
        attachments,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

fn add_history(conn: &Connection, kw: &str) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO search_history(keyword, ts) VALUES (?1, ?2)
         ON CONFLICT(keyword) DO UPDATE SET ts = excluded.ts",
        rusqlite::params![kw, now],
    )
    .map_err(|e| e.to_string())?;
    // 只保留最近 20 条
    conn.execute(
        "DELETE FROM search_history WHERE keyword NOT IN (
            SELECT keyword FROM search_history ORDER BY ts DESC LIMIT 20)",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn get_history(index_db: &Path) -> Result<Vec<(String, i64)>, String> {
    let conn = Connection::open_with_flags(index_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    let mut st = conn
        .prepare("SELECT keyword, ts FROM search_history ORDER BY ts DESC LIMIT 20")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

pub fn clear_history(index_db: &Path) -> Result<(), String> {
    let conn = Connection::open(index_db).map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM search_history", [])
        .map_err(|e| e.to_string())?;
    Ok(())
}

use std::collections::{HashMap, HashSet};
