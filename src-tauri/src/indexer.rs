//! 派生索引构建（M1 / FR-01）
//!
//! - 源 index.db 以 `mode=ro` 打开（G1）
//! - 扫描白名单：仅 `notes/` 与 `attachments/`（G2）
//! - 附件四层归属（§2.6.3）：Tier1=79 / Tier2=43 / Tier3=5 / Tier4=6 / DB缺失=2
//! - 目录树含 WIZ_META FOLDERS_POS 手工排序权重（FR-03.3）
//! - note_fts：FTS5 trigram（FR-06.2）

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use crate::extract::{extract_text, fingerprint};
use crate::library::LibraryResolver;
use crate::zipserve::{NotePathResolver, ZipService};

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

fn guid_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\{([0-9a-fA-F-]{36})\}(.*)$").unwrap())
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
        let package_size = std::fs::metadata(notes_dir.join(package_name(&d.guid)))
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

/// notes/ 下的包文件名：磁盘上叫 `{guid}`，而 DOCUMENT_GUID 不带花括号。
/// 早期直接 join(guid) 使 metadata 恒失败、package_size 静默写 0（M4 实跑发现）。
fn package_name(guid: &str) -> String {
    if guid.starts_with('{') {
        guid.to_string()
    } else {
        format!("{{{guid}}}")
    }
}

/// 路径 → SQLite URI 的安全片段（中文/空格/特殊字符需百分号编码）
pub fn url_encode_path(p: &Path) -> String {
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

/// 读取旧派生索引里需保留的用户数据（检索历史 / 折叠状态），随后旧库可安全删除
fn read_saved_user_data(index_db: &Path) -> (Vec<(String, i64)>, Vec<(String, bool)>) {
    let mut history: Vec<(String, i64)> = Vec::new();
    let mut states: Vec<(String, bool)> = Vec::new();
    if index_db.exists() {
        if let Ok(old) = Connection::open_with_flags(
            index_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            if let Ok(mut st) = old.prepare("SELECT keyword, ts FROM search_history") {
                if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                    history.extend(rows.flatten());
                }
            }
            if let Ok(mut st) = old.prepare("SELECT path, expanded FROM folder_state") {
                if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))) {
                    states.extend(rows.flatten());
                }
            }
        }
    }
    (history, states)
}

/// 库模式派生索引（FR-11 §4.1 / 计划第四节）：只读 `export.db` + resolver 定位 zip 正文，
/// 产出与 [`build_index`] **表结构完全一致**的 index.db。与源模式的差异：
/// - note 元数据直取清单（不读源 WIZ_DOCUMENT）；`package_size` 用 `exported_size`；
/// - 正文经 `resolver`（清单 `exported_path`）定位 zip，**绝不拼路径**（§13.1）；
/// - folder 由 location 去重推导，无 FOLDERS_POS（`pos=i64::MAX`，前端按名称序）；
/// - attachment/attachment_doc 从清单直转，`file_path` 一律落**绝对路径**
///   （`library_dir.join(相对路径)`，与源模式消费口径 `Path::new(fp).is_file()` 一致）；
///   `db-missing:` 行原样保留（无实体文件）；tier/source 原样带过；
/// - 校验口径：清单行数 == 磁盘 zip 命中数 == 索引 note 行数 → ok；**不做**
///   EXPECTED_NOTE_COUNT/TIERS 基线断言（那是源库专属）。
pub fn build_library_index(
    library_dir: &Path,
    resolver: Arc<LibraryResolver>,
    index_db_path: &Path,
    progress: &dyn Fn(usize, usize),
) -> Result<BuildReport, String> {
    let t0 = std::time::Instant::now();
    let mut warnings = Vec::new();

    // ---- 只读打开库清单（G1：绝不写库、绝不触发 migrate）----
    let manifest_db = crate::manifest::manifest_path(library_dir);
    if !manifest_db.is_file() {
        return Err(format!("库清单缺失: {}", manifest_db.display()));
    }
    let m_uri = format!("file:{}?mode=ro", url_encode_path(&manifest_db));
    let man = Connection::open_with_flags(
        &m_uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("打开库清单失败(mode=ro): {e}"))?;

    // ---- 保留用户数据 + 删除旧索引重建 ----
    if let Some(parent) = index_db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let (saved_history, saved_states) = read_saved_user_data(index_db_path);
    if index_db_path.exists() {
        std::fs::remove_file(index_db_path).map_err(|e| e.to_string())?;
    }

    // ---- 读清单 note 行 ----
    struct MNote {
        guid: String,
        title: String,
        location: String,
        url: String,
        doc_type: String,
        created: String,
        data_modified: String,
        has_attachment: bool,
        exported_size: i64,
    }
    let mut st = man
        .prepare(
            "SELECT guid, title, location, ifnull(url,''), ifnull(doc_type,''),
                    created, data_modified, has_attachment, exported_size
             FROM note ORDER BY guid",
        )
        .map_err(|e| e.to_string())?;
    let notes: Vec<MNote> = st
        .query_map([], |r| {
            Ok(MNote {
                guid: r.get(0)?,
                title: r.get(1)?,
                location: r.get(2)?,
                url: r.get(3)?,
                doc_type: r.get(4)?,
                created: r.get(5)?,
                data_modified: r.get(6)?,
                has_attachment: r.get::<_, i64>(7)? != 0,
                exported_size: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let manifest_count = notes.len();

    // ---- 打开派生库（结构复用 create_schema）----
    let mut dst = Connection::open(index_db_path).map_err(|e| e.to_string())?;
    let _ = dst.pragma_update(None, "synchronous", "OFF");
    let _ = dst.pragma_update(None, "journal_mode", "MEMORY");
    let _ = dst.pragma_update(None, "cache_size", -64000i64);
    create_schema(&dst)?;

    // ---- 目录树（location 去重推导，无 FOLDERS_POS）----
    let mut locations: HashSet<String> = HashSet::new();
    for n in &notes {
        locations.insert(n.location.clone());
    }
    for loc in &locations {
        ensure_folders(&dst, loc)?;
    }
    // 磁盘实况目录也进树（空目录可见、库内「新建目录」跨重建保留）：
    // 跳过保留区（`_` 前缀：_trash/_attachments/_conflicts/_unlinked_attachments）
    // 与隐藏目录（`.` 前缀），只认目录。与清单推导行 INSERT OR IGNORE 共存。
    let mut disk_locs: Vec<String> = Vec::new();
    collect_disk_dirs(library_dir, library_dir, 0, &mut disk_locs)?;
    for loc in &disk_locs {
        ensure_folders(&dst, loc)?;
    }

    // ---- 逐篇 note + fts（正文经 resolver 定位 zip）----
    let zip = ZipService::with_resolver(resolver.clone() as Arc<dyn NotePathResolver>);
    let mut disk_hits = 0usize;
    let mut failed: Vec<String> = Vec::new();
    dst.execute("BEGIN", []).map_err(|e| e.to_string())?;
    let total = notes.len();
    for (i, n) in notes.iter().enumerate() {
        if resolver
            .resolve(&n.guid)
            .map(|p| p.is_file())
            .unwrap_or(false)
        {
            disk_hits += 1;
        }
        let (body_len, fp, body_text) = match note_body_text(&zip, &n.guid) {
            Ok(v) => v,
            Err(msg) => {
                failed.push(format!("{}: {}", n.guid, msg));
                (0, String::new(), String::new())
            }
        };
        put_note_row(
            &dst,
            &IndexNoteRow {
                guid: &n.guid,
                title: &n.title,
                location: &n.location,
                url: &n.url,
                doc_type: &n.doc_type,
                created: &n.created,
                data_modified: &n.data_modified,
                has_attachment: n.has_attachment,
                package_size: n.exported_size,
                body_len,
                fingerprint: &fp,
                body_text: &body_text,
            },
        )?;
        if (i + 1) % 10 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }
    dst.execute("COMMIT", []).map_err(|e| e.to_string())?;
    if !failed.is_empty() {
        warnings.push(format!("{} 篇正文解析失败: {}", failed.len(), failed.join("; ")));
    }

    // ---- 附件：清单 attachment/attachment_doc 直转（file_path 落绝对路径）----
    let mut tier1 = 0usize;
    let mut tier2 = 0usize;
    let mut tier3 = 0usize;
    let mut tier4 = 0usize;
    let mut db_missing = 0usize;
    // 清单相对路径 → 派生索引消费口径的**绝对路径**（db-missing: 前缀行原样保留）。
    // A1'：附件在库内落系统保留区，故与落盘侧同用 `manifest::disk_rel_path`
    // （Tier1–3 `attachments/…` → `_attachments/…`；Tier4 的 `_unlinked_attachments/…` 原样）。
    let abs = |rel: &str| -> String {
        if rel.starts_with("db-missing:") {
            rel.to_string()
        } else {
            library_dir
                .join(crate::manifest::disk_rel_path(rel))
                .to_string_lossy()
                .into_owned()
        }
    };
    let tx = dst.transaction().map_err(|e| e.to_string())?;
    {
        // 从清单（man）读附件行，写入派生索引（tx）——两者是不同连接，勿混淆
        let mut q = man
            .prepare(
                "SELECT file_path, display_name, size, tier, document_guid, source
                 FROM attachment ORDER BY file_path",
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<(String, String, i64, i64, Option<String>, String)> = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        drop(q);
        for (rel, display_name, size, tier, document_guid, source) in rows {
            match tier {
                1 => tier1 += 1,
                2 => tier2 += 1,
                3 => tier3 += 1,
                4 => tier4 += 1,
                _ => db_missing += 1,
            }
            tx.execute(
                "INSERT OR REPLACE INTO attachment(file_path,attachment_guid,document_guid,display_name,size,tier,source,db_name)
                 VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, NULL)",
                rusqlite::params![abs(&rel), document_guid, display_name, size, tier, source],
            )
            .map_err(|e| e.to_string())?;
        }
        // attachment_doc（Tier3 多归属展开），file_path 同口径转绝对以匹配 join 键
        let mut qd = man
            .prepare("SELECT file_path, document_guid FROM attachment_doc")
            .map_err(|e| e.to_string())?;
        let docs: Vec<(String, String)> = qd
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        drop(qd);
        for (rel, g) in docs {
            tx.execute(
                "INSERT OR REPLACE INTO attachment_doc(file_path, document_guid) VALUES (?1, ?2)",
                rusqlite::params![abs(&rel), g],
            )
            .map_err(|e| e.to_string())?;
        }
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

    // ---- 校验报告（库口径：清单行数 == 磁盘命中 == 索引 note 行数）----
    let ok = manifest_count == disk_hits && failed.is_empty();
    if !ok {
        warnings.push(format!(
            "库索引一致性：清单 {manifest_count} 篇，磁盘命中 {disk_hits} 篇",
        ));
    }
    Ok(BuildReport {
        note_count: manifest_count,
        source_note_count: manifest_count,
        package_count: disk_hits,
        tier1,
        tier2,
        tier3,
        tier4,
        db_missing,
        elapsed_ms: t0.elapsed().as_millis(),
        warnings,
        ok,
    })
}

/// 库模式正文抽取（build 全量与单篇增量共用同一口径）：zip 内正文 → 文本 → 指纹。
///
/// **M2 起库内正文是 `note.md`**（§20.3）：有该条目就直接取纯文本（md 本身就是纯文本，
/// 比再走一遍 HTML 抽取更简单也更准）；没有才回退 `index.html`（历史 native 库）。
/// 这条优先级**不是兼容分支**，而是"读侧同时支持两种库形态"的取值顺序 —— 导入流程
/// 第③步必跑索引重建，若索引仍只认 `index.html`，md 库的索引正文会全空（软故障）。
/// 返回 `(字符数, 指纹, 全文)`；失败返回可读错误（调用方决定记为 warning 还是 Err）。
fn note_body_text(zip: &ZipService, guid: &str) -> Result<(i64, String, String), String> {
    let body = if zip.has_entry(guid, crate::md::NOTE_MD) {
        let bytes = zip
            .read_entry(guid, crate::md::NOTE_MD)
            .map_err(|e| e.message())?;
        crate::zipserve::decode_utf8_sig(&bytes)
    } else {
        zip.read_index_html(guid).map_err(|e| e.message())?
    };
    let text = body;
    Ok((text.chars().count() as i64, fingerprint(&text), text))
}

/// 库模式 folder 由 `location` 推导（无 FOLDERS_POS → `pos=i64::MAX`，前端按名称序）。
/// `INSERT OR IGNORE`：已存在的目录行（含将来可能恢复的真实排序权重）不被覆盖。
/// 按 location 补齐 folder 表行（含全部祖先链，INSERT OR IGNORE 幂等）。
/// 全量构建（清单 location + 磁盘实况目录）与库内「新建目录」增量共用。
pub fn ensure_folders(dst: &Connection, location: &str) -> Result<(), String> {
    let mut acc = String::from("/");
    for seg in location.trim_matches('/').split('/').filter(|s| !s.is_empty()) {
        acc = format!("{}{}/", acc, seg);
        let trimmed = acc.trim_matches('/').to_string();
        let name = trimmed.rsplit('/').next().unwrap_or("").to_string();
        let parent = match trimmed.rfind('/') {
            Some(i) => format!("/{}/", &trimmed[..i]),
            None => String::new(),
        };
        dst.execute(
            "INSERT OR IGNORE INTO folder(path, name, parent, pos) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![acc, name, parent, i64::MAX],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 递归收集库内磁盘目录（location 形态 `/a/b/`）：跳过 `_`/`.` 前缀目录
/// （系统保留区/隐藏目录）。深度上限 16 兜底，防异常嵌套。
fn collect_disk_dirs(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    if depth >= 16 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        out.push(format!("/{}/", rel.trim_matches('/')));
        collect_disk_dirs(root, &p, depth + 1, out)?;
    }
    Ok(())
}

/// `note` + `note_fts` 的一行写入（build 全量与单篇增量共用）。
/// **先删后插**：FTS 行内容变了必须整行重建（单篇增量时旧行必须先消失）。
struct IndexNoteRow<'a> {
    guid: &'a str,
    title: &'a str,
    location: &'a str,
    url: &'a str,
    doc_type: &'a str,
    created: &'a str,
    data_modified: &'a str,
    has_attachment: bool,
    package_size: i64,
    body_len: i64,
    fingerprint: &'a str,
    body_text: &'a str,
}

fn put_note_row(dst: &Connection, n: &IndexNoteRow<'_>) -> Result<(), String> {
    dst.execute("DELETE FROM note WHERE guid = ?1", [n.guid])
        .map_err(|e| e.to_string())?;
    dst.execute("DELETE FROM note_fts WHERE guid = ?1", [n.guid])
        .map_err(|e| e.to_string())?;
    dst.execute(
        "INSERT INTO note(guid,title,location,url,type,created,data_modified,has_attachment,body_text_length,body_fingerprint,package_size)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        rusqlite::params![
            n.guid,
            n.title,
            n.location,
            n.url,
            n.doc_type,
            n.created,
            n.data_modified,
            n.has_attachment,
            n.body_len,
            n.fingerprint,
            n.package_size,
        ],
    )
    .map_err(|e| e.to_string())?;
    dst.execute(
        "INSERT INTO note_fts(guid,title,body,folder) VALUES (?1,?2,?3,?4)",
        rusqlite::params![n.guid, n.title, n.body_text, n.location],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// **单篇索引增量**（§5.2 / §4.5 第 4 环）：库内写完一篇后只重建该篇。
/// - 该篇仍在清单 → 重抽正文 + 重写 note/note_fts 行 + 补齐新 location 的 folder 祖先；
/// - 该篇已不在清单（刚删除）→ 从索引删除该篇（folder 保留：目录树由其它篇目决定）；
/// - 索引文件/表结构缺失 → `Err`（调用方降级为 `index_updated=false` + 提示重建，**不影响写结果**）。
/// 调用方须保证 `resolver` 读到的是**写之后**的清单（写路径为此新建解析器）。
pub fn update_library_note_index(
    library_dir: &Path,
    resolver: Arc<LibraryResolver>,
    index_db_path: &Path,
    guid: &str,
) -> Result<Vec<String>, String> {
    if !index_db_path.is_file() {
        return Err(format!("派生索引不存在: {}", index_db_path.display()));
    }
    // 只读清单（G1）：写完后的权威状态
    let man = {
        let db = crate::manifest::manifest_path(library_dir);
        let uri = format!("file:{}?mode=ro", url_encode_path(&db));
        Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| format!("打开库清单失败(mode=ro): {e}"))?
    };
    let row: Option<(String, String, String, String, String, String, String, bool, i64)> = {
        let mut st = man
            .prepare(
                "SELECT guid,title,location,ifnull(url,''),ifnull(doc_type,''),created,
                        data_modified,has_attachment,exported_size
                 FROM note WHERE guid = ?1",
            )
            .map_err(|e| e.to_string())?;
        st.query_row([guid], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get::<_, i64>(7)? != 0,
                r.get(8)?,
            ))
        })
        .ok()
    };

    let dst = Connection::open(index_db_path)
        .map_err(|e| format!("打开派生索引失败: {e}"))?;
    // 表结构自检：库索引缺失/未建 → 明确报错，不建半套结构
    dst.query_row("SELECT count(*) FROM note", [], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("派生索引结构不可用（请重建索引）: {e}"))?;

    let mut warnings = Vec::new();
    dst.execute("BEGIN", []).map_err(|e| e.to_string())?;
    let res = (|| -> Result<(), String> {
        match row {
            None => {
                // 已删除：索引里移除该篇
                dst.execute("DELETE FROM note WHERE guid = ?1", [guid])
                    .map_err(|e| e.to_string())?;
                dst.execute("DELETE FROM note_fts WHERE guid = ?1", [guid])
                    .map_err(|e| e.to_string())?;
            }
            Some((g, title, location, url, doc_type, created, data_modified, has_att, size)) => {
                let zip = ZipService::with_resolver(resolver as Arc<dyn NotePathResolver>);
                let (body_len, fp, body_text) = match note_body_text(&zip, &g) {
                    Ok(v) => v,
                    Err(msg) => {
                        warnings.push(format!("正文重抽失败（{msg}），该篇索引正文置空"));
                        (0, String::new(), String::new())
                    }
                };
                ensure_folders(&dst, &location)?;
                put_note_row(
                    &dst,
                    &IndexNoteRow {
                        guid: &g,
                        title: &title,
                        location: &location,
                        url: &url,
                        doc_type: &doc_type,
                        created: &created,
                        data_modified: &data_modified,
                        has_attachment: has_att,
                        package_size: size,
                        body_len,
                        fingerprint: &fp,
                        body_text: &body_text,
                    },
                )?;
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => {
            dst.execute("COMMIT", []).map_err(|e| e.to_string())?;
            Ok(warnings)
        }
        Err(e) => {
            let _ = dst.execute("ROLLBACK", []);
            Err(e)
        }
    }
}

/// 增量索引更新后清理 folder 残留行：删除「**无笔记 ∧ 磁盘无目录**」的行。
///
/// 全量重建天然无此残留（folder 表 = 笔记 location ∪ 磁盘目录，见
/// `build_library_index`）；单篇增量走删除/改名路径会留下 —— `update_library_note_index`
/// 有意保留 folder 行（那是 UI 删单篇的口径，目录树由其它篇目决定），而下行侧回收站
/// 已把空目录从磁盘清掉（`prune_empty_dirs`），行必须跟着走，否则树里出现
/// 「磁盘上不存在的空目录」。只删行、不建行：新增目录由 ensure_folders/全量重建负责。
/// 路径形态 `/a/b/` ↔ `library_dir/a/b`。子目录在磁盘上 ⇒ 父目录必在，故删行天然叶安全。
pub fn prune_stale_index_folders(index_db_path: &Path, library_dir: &Path) -> Result<(), String> {
    let conn = Connection::open(index_db_path).map_err(|e| format!("打开派生索引失败: {e}"))?;
    let paths: Vec<String> = {
        let mut st = conn
            .prepare("SELECT path FROM folder")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    for p in paths {
        let has_note: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM note WHERE location = ?1)",
                [&p],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .map_err(|e| e.to_string())?;
        if has_note {
            continue;
        }
        let rel = p.trim_matches('/');
        let on_disk = rel.is_empty() || library_dir.join(rel).is_dir();
        if !on_disk {
            conn.execute("DELETE FROM folder WHERE path = ?1", [&p])
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    fn write_zip(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("index.html", opt).unwrap();
        std::io::Write::write_all(&mut zw, body.as_bytes()).unwrap();
        zw.finish().unwrap();
    }

    fn mk_note(guid: &str, title: &str, loc: &str, exported: &str) -> manifest::ManifestNote {
        manifest::ManifestNote {
            guid: guid.into(),
            title: title.into(),
            location: loc.into(),
            created: "2024-01-01".into(),
            data_modified: "2024-01-02".into(),
            url: None,
            doc_type: None,
            has_attachment: false,
            package_size: 10,
            exported_path: exported.into(),
            exported_size: 10,
            exported_md5: String::new(),
            export_mode: "native".into(),
            exported_at: "t".into(),
            origin: crate::manifest::ORIGIN_WIZNOTE.into(),
            content_format: crate::manifest::FORMAT_HTML.into(),
        }
    }

    /// temp 目录造小库（手写 export.db + 2 个真实小 zip）→ build_library_index →
    /// 断言表结构齐全、note/folder/attachment 行数、file_path 绝对、FTS 可检索。
    #[test]
    fn test_build_library_index() {
        let lib = std::env::temp_dir().join(format!("wiz-libindex-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        std::fs::create_dir_all(&lib).unwrap();

        let g1 = "11111111-1111-1111-1111-111111111111";
        let g2 = "22222222-2222-2222-2222-222222222222";
        write_zip(&lib.join("工作/笔记一.zip"), "<html><body>苹果 banana</body></html>");
        write_zip(&lib.join("生活/笔记二.zip"), "<html><body>橙子 orange</body></html>");

        let conn = manifest::open_and_migrate(&lib).unwrap();
        manifest::set_meta(&conn, "export_mode", "native").unwrap();
        manifest::upsert_note(&conn, &mk_note(g1, "笔记一", "/工作/", "工作/笔记一.zip")).unwrap();
        manifest::upsert_note(&conn, &mk_note(g2, "笔记二", "/生活/", "生活/笔记二.zip")).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO attachment(file_path,display_name,size,tier,document_guid,source)
             VALUES ('attachments/a.log','a.log',5,1,?1,'db-record')",
            rusqlite::params![g1],
        )
        .unwrap();
        // A1'：库内附件实体落系统保留区 `_attachments/`（清单 `file_path` 仍是源相对口径）
        std::fs::create_dir_all(lib.join(manifest::ATTACH_DIR)).unwrap();
        std::fs::write(lib.join(manifest::ATTACH_DIR).join("a.log"), b"hello").unwrap();
        drop(conn);

        let resolver = Arc::new(LibraryResolver::new(lib.clone()).unwrap());
        let index_db = lib.join("derived-index.db");
        let rep = build_library_index(&lib, resolver, &index_db, &|_, _| {}).unwrap();

        assert_eq!(rep.note_count, 2);
        assert_eq!(rep.package_count, 2, "两个 zip 都应命中");
        assert!(rep.ok, "warnings={:?}", rep.warnings);
        assert_eq!(rep.tier1, 1);

        let idx = Connection::open(&index_db).unwrap();
        let tables: Vec<String> = {
            let mut st = idx
                .prepare("SELECT name FROM sqlite_master WHERE type='table'")
                .unwrap();
            st.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        for t in ["note", "folder", "attachment", "attachment_doc", "search_history", "folder_state"] {
            assert!(tables.iter().any(|x| x == t), "缺表 {t}: {tables:?}");
        }
        assert!(tables.iter().any(|x| x == "note_fts"), "缺 FTS 表");
        let note_rows: i64 = idx.query_row("SELECT count(*) FROM note", [], |r| r.get(0)).unwrap();
        assert_eq!(note_rows, 2);
        let folder_rows: i64 = idx.query_row("SELECT count(*) FROM folder", [], |r| r.get(0)).unwrap();
        assert!(folder_rows >= 2, "至少 /工作/ 与 /生活/，实测 {folder_rows}");
        // attachment.file_path 落绝对路径且实体存在
        let fp: String = idx
            .query_row("SELECT file_path FROM attachment WHERE tier=1", [], |r| r.get(0))
            .unwrap();
        assert!(Path::new(&fp).is_absolute(), "应绝对: {fp}");
        assert!(Path::new(&fp).is_file(), "实体应存在: {fp}");
        assert!(
            fp.ends_with("_attachments/a.log"),
            "库内附件应落保留区 `_attachments/`（A1'）: {fp}"
        );
        // FTS trigram 可检索正文
        let fts_hits: i64 = idx
            .query_row("SELECT count(*) FROM note_fts WHERE note_fts MATCH '\"banana\"'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_hits, 1);
        // package_size 用 exported_size
        let psz: i64 = idx
            .query_row("SELECT package_size FROM note WHERE guid=?1", rusqlite::params![g1], |r| r.get(0))
            .unwrap();
        assert_eq!(psz, 10);

        std::fs::remove_dir_all(&lib).unwrap();
    }
}
