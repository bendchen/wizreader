//! 派生索引构建（M1 / FR-01）
//!
//! - 源 index.db 以 `mode=ro` 打开（G1）
//! - 扫描白名单：仅 `notes/` 与 `attachments/`（G2）
//! - 附件四层归属（§2.6.3）：Tier1=79 / Tier2=43 / Tier3=5 / Tier4=6 / DB缺失=2
//! - 目录树含 WIZ_META FOLDERS_POS 手工排序权重（FR-03.3）
//! - note_fts：FTS5 trigram（FR-06.2）

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::extract::{extract_text, fingerprint};
use crate::zipserve::ZipService;

pub const EXPECTED_NOTE_COUNT: usize = 1780;
pub const EXPECTED_TIERS: (usize, usize, usize, usize, usize) = (79, 43, 5, 6, 2);

#[derive(Debug, Clone, serde::Serialize)]
pub struct BuildReport {
    pub note_count: usize,
    pub source_note_count: usize,
    pub package_count: usize,
    pub tier1: usize,
    pub tier2: usize,
    pub tier3: usize,
    pub tier4: usize,
    pub db_missing: usize,
    pub elapsed_ms: u128,
    pub warnings: Vec<String>,
    pub ok: bool,
}

fn guid_regex() -> regex::Regex {
    regex::Regex::new(r"^\{([0-9a-fA-F-]{36})\}(.*)$").unwrap()
}

/// 构建派生索引。`data_dir` 为源数据目录（含 index.db / notes/ / attachments/）。
/// `progress` 每处理 50 篇回调一次（0..=total）。
pub fn build_index(
    data_dir: &Path,
    index_dir: &Path,
    progress: &dyn Fn(usize, usize),
) -> Result<BuildReport, String> {
    let t0 = std::time::Instant::now();
    let mut warnings = Vec::new();

    let src_db = data_dir.join("index.db");
    let notes_dir = data_dir.join("notes");
    let attach_dir = data_dir.join("attachments");
    for p in [&src_db, &notes_dir] {
        if !p.exists() {
            return Err(format!("源数据缺失: {}", p.display()));
        }
    }

    // 派生索引目录
    std::fs::create_dir_all(index_dir).map_err(|e| e.to_string())?;
    let index_db = index_dir.join("index.db");

    // 保留用户数据（检索历史、折叠状态）
    let mut saved_history: Vec<(String, i64)> = Vec::new();
    let mut saved_states: Vec<(String, bool)> = Vec::new();
    if index_db.exists() {
        if let Ok(old) = Connection::open_with_flags(
            &index_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            if let Ok(mut st) = old.prepare("SELECT keyword, ts FROM search_history") {
                if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                    for r in rows.flatten() {
                        saved_history.push(r);
                    }
                }
            }
            if let Ok(mut st) = old.prepare("SELECT path, expanded FROM folder_state") {
                if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))) {
                    for r in rows.flatten() {
                        saved_states.push(r);
                    }
                }
            }
        }
        std::fs::remove_file(&index_db).map_err(|e| e.to_string())?;
    }

    // ---- 打开源库（只读，G1）----
    let src_uri = format!("file:{}?mode=ro", url_encode_path(&src_db));
    let src = Connection::open_with_flags(
        &src_uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("打开源库失败(mode=ro): {e}"))?;

    // ---- 读 WIZ_DOCUMENT ----
    #[derive(Debug)]
    struct Doc {
        guid: String,
        title: String,
        location: String,
        url: String,
        doc_type: String,
        created: String,
        data_modified: String,
        attach_count: i64,
    }
        let mut stmt = src
        .prepare(
            "SELECT DOCUMENT_GUID, DOCUMENT_TITLE, ifnull(DOCUMENT_LOCATION,''), ifnull(DOCUMENT_URL,''), ifnull(DOCUMENT_TYPE,''),
                    ifnull(DT_CREATED,''), ifnull(DT_DATA_MODIFIED,''), DOCUMENT_ATTACHEMENT_COUNT
             FROM WIZ_DOCUMENT",
        )
        .map_err(|e| e.to_string())?;
    let docs: Vec<Doc> = stmt
        .query_map([], |r| {
            Ok(Doc {
                guid: r.get(0)?,
                title: r.get(1)?,
                location: r.get(2)?,
                url: r.get(3)?,
                doc_type: r.get(4)?,
                created: r.get(5)?,
                data_modified: r.get(6)?,
                attach_count: r.get(7)?,
            })
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let source_note_count = docs.len();

    // notes/ 包文件数（白名单扫描 G2：只认 {GUID} 形态文件）
    let package_files: Vec<String> = std::fs::read_dir(&notes_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.path().is_file())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '{' || c == '}')
                && e.file_name().to_string_lossy().starts_with('{')
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    let package_count = package_files.len();

    // ---- 打开派生库 ----
    let mut dst = Connection::open(&index_db).map_err(|e| e.to_string())?;
    // 派生库可随时重建（源库只读，G1 不受影响），关闭同步/日志换取构建速度（NFR-1 ≤30s）
    let _ = dst.pragma_update(None, "synchronous", "OFF");
    let _ = dst.pragma_update(None, "journal_mode", "MEMORY");
    let _ = dst.pragma_update(None, "cache_size", -64000i64); // 64MB
    create_schema(&dst)?;

    // ---- 目录树（FR-03.3）----
    let pos_map = read_folder_pos(&src)?;
    let mut locations: HashSet<String> = HashSet::new();
    for d in &docs {
        locations.insert(d.location.clone());
    }
    // FOLDERS_POS 中的目录（含 8 个无笔记空目录）也纳入树
    for k in pos_map.keys() {
        locations.insert(k.clone());
    }
    let mut all_paths: HashSet<String> = HashSet::new();
    for loc in &locations {
        let mut acc = String::from("/"); // 根前缀，保证生成 /a/、/a/b/ 形式与 DOCUMENT_LOCATION 一致
        for seg in loc.trim_matches('/').split('/').filter(|s| !s.is_empty()) {
            acc = format!("{}{}/", acc, seg);
            all_paths.insert(acc.clone());
        }
    }
    {
        let mut st = dst
            .prepare("INSERT OR REPLACE INTO folder(path, name, parent, pos) VALUES (?1, ?2, ?3, ?4)")
            .map_err(|e| e.to_string())?;
        for p in &all_paths {
            let name = p.trim_matches('/').rsplit('/').next().unwrap_or("").to_string();
            let parent = {
                let trimmed = p.trim_matches('/');
                match trimmed.rfind('/') {
                    Some(i) => format!("/{}/", &trimmed[..i]),
                    None => String::new(),
                }
            };
            let pos = pos_map.get(p).copied().unwrap_or(i64::MAX);
            st.execute(rusqlite::params![p, name, parent, pos])
                .map_err(|e| e.to_string())?;
        }
    }

    // ---- 附件文件清单（白名单：仅 attachments/ 顶层）----
    let re_guid = guid_regex();
    struct AttFile {
        file_path: PathBuf,
        guid_prefix: Option<String>,
        display_name: String,
        size: i64,
    }
    let mut att_files: Vec<AttFile> = Vec::new();
    if attach_dir.is_dir() {
        for e in std::fs::read_dir(&attach_dir).map_err(|e| e.to_string())?.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue; // 白名单外跳过
            }
            let fname = e.file_name().to_string_lossy().to_string();
            if fname == ".DS_Store" {
                continue;
            }
            let (guid_prefix, display_name) = match re_guid.captures(&fname) {
                Some(c) => (Some(c[1].to_string()), c[2].to_string()),
                None => (None, fname.clone()),
            };
            let size = p.metadata().map(|m| m.len() as i64).unwrap_or(0);
            att_files.push(AttFile {
                file_path: p,
                guid_prefix,
                display_name,
                size,
            });
        }
    }

    // ---- 读 WIZ_DOCUMENT_ATTACHMENT（81 条 DB 记录）----
    struct DbAtt {
        guid: String,
        document_guid: String,
        name: String,
    }
    let mut stmt = src
        .prepare("SELECT ATTACHMENT_GUID, DOCUMENT_GUID, ifnull(ATTACHMENT_NAME,'') FROM WIZ_DOCUMENT_ATTACHMENT")
        .map_err(|e| e.to_string())?;
    let db_atts: Vec<DbAtt> = stmt
        .query_map([], |r| {
            Ok(DbAtt {
                guid: r.get(0)?,
                document_guid: r.get(1)?,
                name: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let db_att_by_guid: HashMap<String, &DbAtt> =
        db_atts.iter().map(|a| (a.guid.clone(), a)).collect();

    // ---- 逐篇构建 note + fts（同时缓存正文用于附件归属）----
    let zip = ZipService::new(notes_dir.clone());
    let mut body_cache: Vec<(String, String)> = Vec::with_capacity(docs.len()); // (guid, body_text)
    let mut failed: Vec<String> = Vec::new();

    dst.execute("BEGIN", []).map_err(|e| e.to_string())?;
    let total = docs.len();
    for (i, d) in docs.iter().enumerate() {
        let package_size = std::fs::metadata(notes_dir.join(&d.guid))
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let (body_len, fp) = match zip.read_index_html(&d.guid) {
            Ok(html) => {
                let text = extract_text(&html);
                let fp = fingerprint(&text);
                body_cache.push((d.guid.clone(), text.clone()));
                (text.chars().count() as i64, fp)
            }
            Err(e) => {
                failed.push(format!("{}: {}", d.guid, e.message()));
                (0, String::new())
            }
        };
        dst.execute(
            "INSERT INTO note(guid,title,location,url,type,created,data_modified,has_attachment,body_text_length,body_fingerprint,package_size)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                d.guid,
                d.title,
                d.location,
                d.url,
                d.doc_type,
                d.created,
                d.data_modified,
                d.attach_count > 0,
                body_len,
                fp,
                package_size
            ],
        )
        .map_err(|e| e.to_string())?;
        let body_text = body_cache.last().map(|(_, t)| t.clone()).unwrap_or_default();
        dst.execute(
            "INSERT INTO note_fts(guid,title,body,folder) VALUES (?1,?2,?3,?4)",
            rusqlite::params![d.guid, d.title, body_text, d.location],
        )
        .map_err(|e| e.to_string())?;
        if (i + 1) % 10 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }
    dst.execute("COMMIT", []).map_err(|e| e.to_string())?;
    if !failed.is_empty() {
        warnings.push(format!("{} 篇解析失败: {}", failed.len(), failed.join("; ")));
    }

    // ---- 附件四层归属（T1.2 / §2.6.3）----
    let mut tier1 = 0usize;
    let mut tier2 = 0usize;
    let mut tier3 = 0usize;
    let mut tier4 = 0usize;
    let mut db_missing = 0usize;

    let mut tx = dst.transaction().map_err(|e| e.to_string())?;
    let _ = &mut tx;

    // Tier 1：{GUID} 前缀精确匹配
    for f in att_files.iter().filter(|f| f.guid_prefix.is_some()) {
        let g = f.guid_prefix.as_ref().unwrap();
        if let Some(db) = db_att_by_guid.get(g) {
            tier1 += 1;
            tx.execute(
                "INSERT INTO attachment(file_path,attachment_guid,document_guid,display_name,size,tier,source)
                 VALUES (?1,?2,?3,?4,?5,1,'db-record')",
                rusqlite::params![f.file_path.to_string_lossy(), g, db.document_guid, f.display_name, f.size],
            )
            .map_err(|e| e.to_string())?;
        } else {
            // 带前缀但 DB 无记录 → 按孤儿逻辑处理
            classify_orphan(
                &tx,
                &body_cache,
                &f.file_path.to_string_lossy(),
                &f.display_name,
                f.size,
                &mut tier2,
                &mut tier3,
                &mut tier4,
                &mut warnings,
            )?;
        }
    }

    // DB 记录对应文件缺失（DB 缺失 = 2）
    let file_guids: HashSet<String> = att_files.iter().filter_map(|f| f.guid_prefix.clone()).collect();
    for db in &db_atts {
        if !file_guids.contains(&db.guid) {
            db_missing += 1;
            tx.execute(
                "INSERT INTO attachment(file_path,attachment_guid,document_guid,display_name,size,tier,source,db_name)
                 VALUES (?1,?2,?3,?4,0,0,'db-missing',?5)",
                rusqlite::params![
                    format!("db-missing:{}", db.guid),
                    db.guid,
                    db.document_guid,
                    db.name,
                    db.name
                ],
            )
            .map_err(|e| e.to_string())?;
        }
    }

    // 孤儿（无前缀）：正文检索归属
    for f in att_files.iter().filter(|f| f.guid_prefix.is_none()) {
        classify_orphan(
            &tx,
            &body_cache,
            &f.file_path.to_string_lossy(),
            &f.display_name,
            f.size,
            &mut tier2,
            &mut tier3,
            &mut tier4,
            &mut warnings,
        )?;
    }

    tx.commit().map_err(|e| e.to_string())?;

    // ---- 用户数据还原 ----
    for (kw, ts) in &saved_history {
        let _ = dst.execute(
            "INSERT INTO search_history(keyword, ts) VALUES (?1, ?2)",
            rusqlite::params![kw, ts],
        );
    }
    for (p, ex) in &saved_states {
        let _ = dst.execute(
            "INSERT OR REPLACE INTO folder_state(path, expanded) VALUES (?1, ?2)",
            rusqlite::params![p, *ex as i64],
        );
    }

    let report = BuildReport {
        note_count: source_note_count,
        source_note_count,
        package_count,
        tier1,
        tier2,
        tier3,
        tier4,
        db_missing,
        elapsed_ms: t0.elapsed().as_millis(),
        warnings: warnings.clone(),
        ok: false,
    };

    // ---- 校验报告（FR-01.6）----
    let mut ok = true;
    if source_note_count != package_count {
        ok = false;
    }
    if source_note_count != EXPECTED_NOTE_COUNT {
        ok = false;
        warnings.push(format!(
            "笔记数偏离基线：实测 {}，基线 {}",
            source_note_count, EXPECTED_NOTE_COUNT
        ));
    }
    if (tier1, tier2, tier3, tier4, db_missing) != EXPECTED_TIERS {
        ok = false;
        warnings.push(format!(
            "附件 Tier 分布偏离：实测 T1={} T2={} T3={} T4={} DB缺失={}，基线 79/43/5/6/2",
            tier1, tier2, tier3, tier4, db_missing
        ));
    }
    Ok(BuildReport { ok, ..report })
}

fn classify_orphan(
    tx: &rusqlite::Transaction,
    body_cache: &[(String, String)],
    file_path: &str,
    name: &str,
    size: i64,
    tier2: &mut usize,
    tier3: &mut usize,
    tier4: &mut usize,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let fp = file_path.to_string();
    let hits: Vec<&String> = body_cache
        .iter()
        .filter(|(_, text)| text.contains(name))
        .map(|(g, _)| g)
        .collect();
    if hits.len() == 1 {
        *tier2 += 1;
        tx.execute(
            "INSERT INTO attachment(file_path,attachment_guid,document_guid,display_name,size,tier,source)
             VALUES (?1, NULL, ?2, ?3, ?4, 2, 'filename-in-body')",
            rusqlite::params![fp, hits[0], name, size],
        )
        .map_err(|e| e.to_string())?;
    } else if hits.len() == 2 {
        *tier3 += 1;
        tx.execute(
            "INSERT INTO attachment(file_path,attachment_guid,document_guid,display_name,size,tier,source)
             VALUES (?1, NULL, NULL, ?2, ?3, 3, 'filename-in-body')",
            rusqlite::params![fp, name, size],
        )
        .map_err(|e| e.to_string())?;
        for g in &hits {
            tx.execute(
                "INSERT INTO attachment_doc(file_path, document_guid) VALUES (?1, ?2)",
                rusqlite::params![fp, g],
            )
            .map_err(|e| e.to_string())?;
        }
    } else if hits.is_empty() {
        *tier4 += 1;
        tx.execute(
            "INSERT INTO attachment(file_path,attachment_guid,document_guid,display_name,size,tier,source)
             VALUES (?1, NULL, NULL, ?2, ?3, 4, 'unlinked')",
            rusqlite::params![fp, name, size],
        )
        .map_err(|e| e.to_string())?;
    } else {
        warnings.push(format!("附件 {} 命中 {} 篇正文（>2），按 Tier3 处理", name, hits.len()));
        *tier3 += 1;
        tx.execute(
            "INSERT INTO attachment(file_path,attachment_guid,document_guid,display_name,size,tier,source)
             VALUES (?1, NULL, NULL, ?2, ?3, 3, 'filename-in-body')",
            rusqlite::params![fp, name, size],
        )
        .map_err(|e| e.to_string())?;
        for g in &hits {
            tx.execute(
                "INSERT INTO attachment_doc(file_path, document_guid) VALUES (?1, ?2)",
                rusqlite::params![fp, g],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// 读取 WIZ_META FOLDERS_POS（JSON: {"/path/": pos}）
fn read_folder_pos(src: &Connection) -> Result<HashMap<String, i64>, String> {
    let mut map = HashMap::new();
    let val: Option<String> = src
        .query_row(
            "SELECT META_VALUE FROM WIZ_META WHERE META_NAME='SYNC_INFO' AND META_KEY='FOLDERS_POS'",
            [],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e.to_string()),
        })
        .map_err(|e| e)?;
    if let Some(v) = val {
        let parsed: HashMap<String, i64> =
            serde_json::from_str(&v).map_err(|e| format!("FOLDERS_POS 解析失败: {e}"))?;
        map = parsed;
    } else {
        // 权重缺失：回退按名称（调用方应记日志）
    }
    Ok(map)
}

fn create_schema(dst: &Connection) -> Result<(), String> {
    dst.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS note(
            guid TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            location TEXT NOT NULL,
            url TEXT,
            type TEXT,
            created TEXT,
            data_modified TEXT,
            has_attachment INTEGER NOT NULL DEFAULT 0,
            body_text_length INTEGER NOT NULL DEFAULT 0,
            body_fingerprint TEXT,
            package_size INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_note_location ON note(location);
        CREATE INDEX IF NOT EXISTS idx_note_data_modified ON note(data_modified);
        CREATE TABLE IF NOT EXISTS folder(
            path TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            parent TEXT NOT NULL DEFAULT '',
            pos INTEGER
        );
        CREATE TABLE IF NOT EXISTS attachment(
            file_path TEXT PRIMARY KEY,          -- P17: 禁止用显示名作键
            attachment_guid TEXT,                -- Tier2/3/4 无 DB 记录时为 NULL
            document_guid TEXT,                  -- Tier4 为 NULL；Tier3 为多值（见 attachment_doc）
            display_name TEXT NOT NULL,
            size INTEGER NOT NULL DEFAULT 0,
            tier INTEGER NOT NULL,               -- 1/2/3/4；0=DB记录但文件缺失
            source TEXT NOT NULL,                -- db-record/filename-in-body/unlinked/db-missing
            db_name TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_att_doc ON attachment(document_guid);
        CREATE TABLE IF NOT EXISTS attachment_doc(
            file_path TEXT NOT NULL,
            document_guid TEXT NOT NULL,
            PRIMARY KEY(file_path, document_guid)
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS note_fts USING fts5(
            guid UNINDEXED, title, body, folder,
            tokenize='trigram'
        );
        CREATE TABLE IF NOT EXISTS search_history(
            keyword TEXT PRIMARY KEY,
            ts INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS folder_state(
            path TEXT PRIMARY KEY,
            expanded INTEGER NOT NULL DEFAULT 1
        );
        "#,
    )
    .map_err(|e| e.to_string())
}

fn url_encode_path(p: &Path) -> String {
    p.to_string_lossy()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/_-:.@".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect()
}
