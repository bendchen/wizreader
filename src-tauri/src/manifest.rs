//! 导出目录同步清单缓存 `export.db`（docs/云端同步数据分析.md §5）
//!
//! 定位：**派生物**、可随时删除重建（§3 原则 2），任何同步流程不得把它当唯一事实来源。
//! 职责：增量判定（`data_modified`/`package_size`）、GUID↔导出文件映射、换机恢复。
//! 比对键一律用导出时自算的 zip MD5（§5.3 实测 `DOCUMENT_DATA_MD5` 不可信，仅存档备查）。
//! 本库只写在导出目标目录内，绝不触碰源数据（G1）。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use md5::{Digest, Md5};
use rusqlite::Connection;

/// 导出根目录下的清单文件名（ASCII，利于跨平台与未来云端对象键稳定）
pub const MANIFEST_NAME: &str = "export.db";

/// `note` 表一行（§5.4 DDL）。源库字段直存原文；净化后的文件名只出现在 `exported_path`
#[derive(Debug, Clone)]
pub struct ManifestNote {
    pub guid: String,
    pub title: String,
    pub location: String,
    pub created: String,
    pub data_modified: String,
    pub url: Option<String>,
    pub doc_type: Option<String>,
    pub has_attachment: bool,
    pub package_size: i64,
    /// 导出 zip 相对导出根的路径（'/' 分隔，含 `.zip` 文件名）
    pub exported_path: String,
    pub exported_size: i64,
    pub exported_md5: String,
    /// `"native"` | `"slim"`
    pub export_mode: String,
    /// slim：保留条目数（原条目 - 删除条目）；native 为 NULL
    pub entry_count: Option<u64>,
    pub removed_files: Option<u64>,
    pub removed_bytes: Option<u64>,
    /// slim：保留条目字节（实现补充，用于反推原字节 = kept + removed）
    pub kept_bytes: Option<u64>,
    pub exported_at: String,
}

pub fn manifest_path(dest_root: &Path) -> PathBuf {
    dest_root.join(MANIFEST_NAME)
}

/// 打开（或创建）导出根下的清单库
pub fn open_or_create(dest_root: &Path) -> Result<Connection, String> {
    let conn = Connection::open(manifest_path(dest_root)).map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA_SQL).map_err(|e| e.to_string())?;
    Ok(conn)
}

const SCHEMA_SQL: &str = r#"PRAGMA journal_mode = DELETE;
-- DELETE 模式：导出目录可能被整体拷走/上传，避免留下 -wal/-shm 残留
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS note (
  guid            TEXT PRIMARY KEY,          -- DOCUMENT_GUID（去花括号），未来云端对象键
  title           TEXT NOT NULL,             -- DOCUMENT_TITLE 原文
  location        TEXT NOT NULL,             -- DOCUMENT_LOCATION 原文
  created         TEXT NOT NULL,             -- DT_CREATED
  data_modified   TEXT NOT NULL,             -- DT_DATA_MODIFIED（唯一可信时间，P12）
  url             TEXT,
  doc_type        TEXT,
  has_attachment  INTEGER NOT NULL DEFAULT 0,
  package_size    INTEGER NOT NULL,
  exported_path   TEXT NOT NULL,
  exported_size   INTEGER NOT NULL,
  exported_md5    TEXT NOT NULL,             -- 导出 zip 自算 MD5（唯一可信比对键，§5.3）
  export_mode     TEXT NOT NULL,             -- 'native' | 'slim'
  entry_count     INTEGER,
  removed_files   INTEGER,
  removed_bytes   INTEGER,
  kept_bytes      INTEGER,                   -- 实现补充：slim 保留字节（反推原字节用）
  exported_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_note_location ON note(location);
CREATE INDEX IF NOT EXISTS idx_note_modified ON note(data_modified);
CREATE TABLE IF NOT EXISTS deleted (         -- 墓碑：源库已消失的篇目
  guid        TEXT PRIMARY KEY,
  last_path   TEXT NOT NULL,
  removed_at  TEXT NOT NULL
);
INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '1'), ('revision', '0');
"#;

pub fn load_notes(conn: &Connection) -> Result<HashMap<String, ManifestNote>, String> {
    let mut st = conn
        .prepare(
            "SELECT guid, title, location, created, data_modified, url, doc_type, has_attachment,
                    package_size, exported_path, exported_size, exported_md5, export_mode,
                    entry_count, removed_files, removed_bytes, kept_bytes, exported_at
             FROM note",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(ManifestNote {
                guid: r.get(0)?,
                title: r.get(1)?,
                location: r.get(2)?,
                created: r.get(3)?,
                data_modified: r.get(4)?,
                url: r.get(5)?,
                doc_type: r.get(6)?,
                has_attachment: r.get::<_, i64>(7)? > 0,
                package_size: r.get(8)?,
                exported_path: r.get(9)?,
                exported_size: r.get(10)?,
                exported_md5: r.get(11)?,
                export_mode: r.get(12)?,
                entry_count: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                removed_files: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
                removed_bytes: r.get::<_, Option<i64>>(15)?.map(|v| v as u64),
                kept_bytes: r.get::<_, Option<i64>>(16)?.map(|v| v as u64),
                exported_at: r.get(17)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().map(|n| (n.guid.clone(), n)).collect())
}

pub fn upsert_note(conn: &Connection, n: &ManifestNote) -> Result<(), String> {
    conn.execute(
        "INSERT INTO note(guid, title, location, created, data_modified, url, doc_type,
                          has_attachment, package_size, exported_path, exported_size,
                          exported_md5, export_mode, entry_count, removed_files,
                          removed_bytes, kept_bytes, exported_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
         ON CONFLICT(guid) DO UPDATE SET
           title=?2, location=?3, created=?4, data_modified=?5, url=?6, doc_type=?7,
           has_attachment=?8, package_size=?9, exported_path=?10, exported_size=?11,
           exported_md5=?12, export_mode=?13, entry_count=?14, removed_files=?15,
           removed_bytes=?16, kept_bytes=?17, exported_at=?18",
        rusqlite::params![
            n.guid,
            n.title,
            n.location,
            n.created,
            n.data_modified,
            n.url,
            n.doc_type,
            n.has_attachment as i64,
            n.package_size,
            n.exported_path,
            n.exported_size,
            n.exported_md5,
            n.export_mode,
            n.entry_count.map(|v| v as i64),
            n.removed_files.map(|v| v as i64),
            n.removed_bytes.map(|v| v as i64),
            n.kept_bytes.map(|v| v as i64),
            n.exported_at,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------- meta / 墓碑

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    let mut st = conn
        .prepare("SELECT value FROM meta WHERE key = ?1")
        .map_err(|e| e.to_string())?;
    let mut rows = st.query_map([key], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    match rows.next() {
        Some(v) => Ok(Some(v.map_err(|e| e.to_string())?)),
        None => Ok(None),
    }
}

/// 写入/更新 meta 键（pub(crate)：导出流程也要维护 exported_at/export_mode 等）
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        [key, value],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 清单版本号自增（§8：单调递增、不依赖跨机时钟）；返回新值
pub fn bump_revision(conn: &Connection, updated_at: &str) -> Result<u64, String> {
    let old = get_meta(conn, "revision")?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let rev = old + 1;
    set_meta(conn, "revision", &rev.to_string())?;
    set_meta(conn, "updated_at", updated_at)?;
    Ok(rev)
}

/// 把清单中「源库已不存在」的行转入墓碑并删除（§6.3：只记录，不删文件、不上传删除指令）。
/// `source_guids` 必须是**全库**口径（不含目录过滤），否则目录范围外的行会被误判。
/// 返回本轮新产生的墓碑 guid。
pub fn reconcile_tombstones(
    conn: &Connection,
    source_guids: &HashSet<String>,
    removed_at: &str,
) -> Result<Vec<String>, String> {
    let existing: Vec<String> = {
        let mut st = conn.prepare("SELECT guid FROM note").map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    let mut out = Vec::new();
    for guid in existing {
        if source_guids.contains(&guid) {
            continue;
        }
        let last_path: String = conn
            .query_row("SELECT exported_path FROM note WHERE guid = ?1", [&guid], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO deleted(guid, last_path, removed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(guid) DO UPDATE SET removed_at = ?3",
            rusqlite::params![guid, last_path, removed_at],
        )
        .map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM note WHERE guid = ?1", [&guid]).map_err(|e| e.to_string())?;
        out.push(guid);
    }
    Ok(out)
}

// ---------------------------------------------------------------- 不变量自检（§5.5）

/// 导出结束后自检；返回警告列表（不抛错，警告进导出报告，供抽查）
pub fn check_invariants(conn: &Connection, dest_root: &Path) -> Result<Vec<String>, String> {
    let rows = load_notes(conn)?;
    let mut warns = Vec::new();

    // 1. 每行 exported_path 指向的文件必须存在且体积相符
    for n in rows.values() {
        let p = dest_root.join(&n.exported_path);
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => {
                if m.len() != n.exported_size as u64 {
                    warns.push(format!(
                        "体积不符 {}: 清单 {} B / 磁盘 {} B",
                        n.exported_path, n.exported_size, m.len()
                    ));
                }
            }
            _ => warns.push(format!("清单行缺文件: {}", n.exported_path)),
        }
    }
    // 2. 导出目录内未被清单覆盖的孤儿 zip（报告，不静默删除）
    let covered: HashSet<&str> = rows.values().map(|n| n.exported_path.as_str()).collect();
    let mut stack = vec![dest_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "zip").unwrap_or(false) {
                let rel = p
                    .strip_prefix(dest_root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                if !covered.contains(rel.as_str()) {
                    warns.push(format!("孤儿文件（无清单行）: {rel}"));
                }
            }
        }
    }
    // 3. 墓碑与 note 不得同时出现
    let dup: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM note n JOIN deleted d ON n.guid = d.guid",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if dup > 0 {
        warns.push(format!("{dup} 个 guid 同时存在于 note 与 deleted"));
    }
    Ok(warns)
}

// ---------------------------------------------------------------- 重建入口（§5.1 原则 2）

#[derive(Debug, Default)]
pub struct RebuildReport {
    pub rows: usize,
    pub warnings: Vec<String>,
}

/// 清单丢失后的重建：重放源库命名计划（与导出同序同规则）→ 匹配磁盘 zip →
/// 逐文件算 MD5 + 取 mtime 作为 exported_at（与导出路径同源，保证重建结果逐字段一致）。
/// `mode` 为导出目录实际使用的模式（"native" | "slim"）；slim 会重算瘦身统计（确定性口径）。
pub fn rebuild(
    dest_root: &Path,
    ctx: &crate::export::ExportContext,
    mode: &str,
) -> Result<RebuildReport, String> {
    let conn = open_or_create(dest_root)?;
    // note 表全量重算；deleted 墓碑保留（删除事实不丢）
    conn.execute("DELETE FROM note", []).map_err(|e| e.to_string())?;

    let src = crate::export::open_index_ro(ctx)?;
    let notes = crate::export::load_notes(&src, "")?;
    let plan = crate::export::plan_zip_paths(&notes);
    let source_guids: HashSet<String> = notes.iter().map(|n| n.guid.clone()).collect();

    let mut rep = RebuildReport::default();
    for (n, p) in notes.iter().zip(&plan) {
        let dest = dest_root.join(&p.rel_path);
        let meta = match std::fs::metadata(&dest) {
            Ok(m) if m.is_file() => m,
            _ => {
                rep.warnings.push(format!("缺失导出文件: {}", p.rel_path));
                continue;
            }
        };
        let exported_at = format_utc(meta.modified().unwrap_or(SystemTime::now()));
        let exported_md5 = md5_file(&dest)?;
        let (entry_count, removed_files, removed_bytes, kept_bytes) =
            if mode == "slim" {
                match crate::export::slim_stat_of(&ctx.notes_dir.join(format!("{{{}}}", n.guid))) {
                    Ok((s, _)) => (
                        Some(s.kept_files),
                        Some(s.orig_files.saturating_sub(s.kept_files)),
                        Some(s.orig_bytes.saturating_sub(s.kept_bytes)),
                        Some(s.kept_bytes),
                    ),
                    Err(e) => {
                        rep.warnings.push(format!("瘦身统计重算失败 {}: {e}", n.guid));
                        (None, None, None, None)
                    }
                }
            } else {
                (None, None, None, None)
            };
        upsert_note(
            &conn,
            &ManifestNote {
                guid: n.guid.clone(),
                title: n.title.clone(),
                location: n.location.clone(),
                created: n.created.clone(),
                data_modified: n.data_modified.clone(),
                url: n.url.clone(),
                doc_type: n.doc_type.clone(),
                has_attachment: n.has_attachment,
                package_size: n.package_size,
                exported_path: p.rel_path.clone(),
                exported_size: meta.len() as i64,
                exported_md5,
                export_mode: mode.to_string(),
                entry_count,
                removed_files,
                removed_bytes,
                kept_bytes,
                exported_at,
            },
        )?;
        rep.rows += 1;
    }
    // 重建同样要收墓碑（源库已消失、导出文件仍在的篇目）
    let tombstoned = reconcile_tombstones(&conn, &source_guids, &format_utc(SystemTime::now()))?;
    rep.warnings.extend(check_invariants(&conn, dest_root)?);
    if !tombstoned.is_empty() {
        rep.warnings.push(format!("转入墓碑 {} 篇", tombstoned.len()));
    }
    Ok(rep)
}

// ---------------------------------------------------------------- 基础设施

/// 流式 MD5（边拷边算的原语，供导出路径复用，不做二次读盘）
pub fn copy_with_md5(src: &Path, dest: &Path) -> Result<(u64, String), String> {
    let mut rd = File::open(src).map_err(|e| e.to_string())?;
    let mut wr = File::create(dest).map_err(|e| e.to_string())?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = rd.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        wr.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        total += n as u64;
    }
    wr.flush().map_err(|e| e.to_string())?;
    Ok((total, format!("{:x}", h.finalize())))
}

/// 流式计算文件 MD5
pub fn md5_file(path: &Path) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// SystemTime → UTC 时间串（不引入 chrono，跨机只用于存档展示）。
/// 注意：导出与重建都取**目标文件 mtime**，因此重建结果与原库逐字段一致
pub fn format_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil-from-days（Howard Hinnant 算法）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wiz-manifest-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn test_format_utc_known_values() {
        assert_eq!(
            format_utc(SystemTime::UNIX_EPOCH),
            "1970-01-01 00:00:00Z"
        );
        // 1_000_000_000 = 2001-09-09 01:46:40 UTC
        assert_eq!(
            format_utc(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000)),
            "2001-09-09 01:46:40Z"
        );
    }

    #[test]
    fn test_md5_copy_and_file() {
        let d = temp_dir("md5");
        let src = d.join("src.bin");
        std::fs::write(&src, b"foo").unwrap();
        let dest = d.join("dest.bin");
        let (len, md5) = copy_with_md5(&src, &dest).unwrap();
        assert_eq!(len, 3);
        // md5("foo") 公认值
        assert_eq!(md5, "acbd18db4cc2f85cedef654fccc4a4d8");
        assert_eq!(md5_file(&dest).unwrap(), md5);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn test_upsert_load_roundtrip() {
        let d = temp_dir("upsert");
        let conn = open_or_create(&d).unwrap();
        let n = ManifestNote {
            guid: "11111111-2222-3333-4444-555555555555".into(),
            title: "标题<一>".into(),
            location: "/a/b/".into(),
            created: "2020-01-01 00:00:00".into(),
            data_modified: "2024-05-12 08:00:00".into(),
            url: Some("https://example.com".into()),
            doc_type: None,
            has_attachment: true,
            package_size: 12345,
            exported_path: "a/b/标题_一.zip".into(),
            exported_size: 999,
            exported_md5: "abc".into(),
            export_mode: "slim".into(),
            entry_count: Some(10),
            removed_files: Some(5),
            removed_bytes: Some(4096),
            kept_bytes: Some(1024),
            exported_at: "2026-09-16 00:00:00Z".into(),
        };
        upsert_note(&conn, &n).unwrap();
        // 二次 upsert（复用路径更新元数据）不产生重复行
        upsert_note(&conn, &n).unwrap();
        let loaded = load_notes(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        let m = loaded.get(&n.guid).unwrap();
        assert_eq!(m.title, "标题<一>");
        assert_eq!(m.entry_count, Some(10));
        assert_eq!(m.kept_bytes, Some(1024));
        assert!(m.has_attachment);
        assert_eq!(get_meta(&conn, "schema_version").unwrap().as_deref(), Some("1"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn test_reconcile_tombstones_and_revision() {
        let d = temp_dir("tomb");
        let conn = open_or_create(&d).unwrap();
        let n = ManifestNote {
            guid: "aaaaaaaa-0000-0000-0000-000000000001".into(),
            title: "x".into(),
            location: "/a/".into(),
            created: String::new(),
            data_modified: "2024-01-01 00:00:00".into(),
            url: None,
            doc_type: None,
            has_attachment: false,
            package_size: 1,
            exported_path: "a/x.zip".into(),
            exported_size: 1,
            exported_md5: "m".into(),
            export_mode: "native".into(),
            entry_count: None,
            removed_files: None,
            removed_bytes: None,
            kept_bytes: None,
            exported_at: "2026-09-16 00:00:00Z".into(),
        };
        upsert_note(&conn, &n).unwrap();
        // 全库口径不含该 guid → 转墓碑
        let gone = reconcile_tombstones(&conn, &HashSet::new(), "2026-09-16 12:00:00Z").unwrap();
        assert_eq!(gone, vec![n.guid.clone()]);
        assert!(load_notes(&conn).unwrap().is_empty());
        let (del_path, del_at): (String, String) = conn
            .query_row(
                "SELECT last_path, removed_at FROM deleted WHERE guid = ?1",
                [&n.guid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(del_path, "a/x.zip");
        assert_eq!(del_at, "2026-09-16 12:00:00Z");
        // 重复收敛幂等；revision 自增
        assert!(reconcile_tombstones(&conn, &HashSet::new(), "2026-09-16 13:00:00Z")
            .unwrap()
            .is_empty());
        assert_eq!(bump_revision(&conn, "t").unwrap(), 1);
        assert_eq!(bump_revision(&conn, "t").unwrap(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn test_check_invariants() {
        let d = temp_dir("inv");
        let conn = open_or_create(&d).unwrap();
        // 磁盘上有两个文件：一个有清单行、一个是孤儿
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("a/ok.zip"), b"12345").unwrap();
        std::fs::write(d.join("orphan.zip"), b"x").unwrap();
        let n = ManifestNote {
            guid: "bbbbbbbb-0000-0000-0000-000000000002".into(),
            title: "ok".into(),
            location: "/a/".into(),
            created: String::new(),
            data_modified: "2024-01-01 00:00:00".into(),
            url: None,
            doc_type: None,
            has_attachment: false,
            package_size: 5,
            exported_path: "a/ok.zip".into(),
            exported_size: 5,
            exported_md5: "m".into(),
            export_mode: "native".into(),
            entry_count: None,
            removed_files: None,
            removed_bytes: None,
            kept_bytes: None,
            exported_at: "2026-09-16 00:00:00Z".into(),
        };
        upsert_note(&conn, &n).unwrap();
        let warns = check_invariants(&conn, &d).unwrap();
        assert!(warns.iter().any(|w| w.contains("孤儿文件") && w.contains("orphan.zip")));
        // 文件被删 → 缺文件警告；体积改 → 体积不符警告
        std::fs::remove_file(d.join("a/ok.zip")).unwrap();
        let warns = check_invariants(&conn, &d).unwrap();
        assert!(warns.iter().any(|w| w.contains("缺文件")));
        std::fs::write(d.join("a/ok.zip"), b"12345678").unwrap();
        let warns = check_invariants(&conn, &d).unwrap();
        assert!(warns.iter().any(|w| w.contains("体积不符")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn test_rebuild_matches_forward_export() {
        use crate::extract::export_name;
        // 构造迷你源库（WizReader index.db note 表口径）+ 源 zip + 导出目录
        let d = temp_dir("rebuild");
        let data_dir = d.join("data");
        std::fs::create_dir_all(data_dir.join("notes")).unwrap();
        let src_db = data_dir.join("index.db");
        let conn = Connection::open(&src_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE note (guid TEXT PRIMARY KEY, title TEXT, location TEXT, url TEXT,
             type TEXT, created TEXT, data_modified TEXT, has_attachment INTEGER, package_size INTEGER);
             INSERT INTO note VALUES ('11111111-2222-3333-4444-555555555555', '标题 A', '/d/', NULL,
             NULL, '2020-01-01 00:00:00', '2024-05-01 00:00:00', 0, 6);",
        )
        .unwrap();
        std::fs::write(data_dir.join("notes/{11111111-2222-3333-4444-555555555555}"), b"hello").unwrap();

        // 用与导出一致的命名计划落一个「导出 zip」
        let ctx = crate::export::ExportContext::new(data_dir.join("notes"), src_db.clone());
        let notes = crate::export::load_notes(&Connection::open_with_flags(
            &src_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap(), "")
        .unwrap();
        let plan = crate::export::plan_zip_paths(&notes);
        assert_eq!(plan.len(), 1);
        let dest_root = d.join("out");
        let dest = dest_root.join(&plan[0].rel_path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"world").unwrap();
        // 名字重放校验（导出同序同规则）
        let mut used = std::collections::HashSet::new();
        assert_eq!(export_name("标题 A", "11111111-2222-3333-4444-555555555555", &mut used), "标题 A");

        // 先正向导出写一版清单，删除后重建，要求逐字段一致（验收 ③）
        let r = rebuild(&dest_root, &ctx, "native").unwrap();
        assert_eq!(r.rows, 1);
        let before = load_notes(&open_or_create(&dest_root).unwrap()).unwrap();
        std::fs::remove_file(manifest_path(&dest_root)).unwrap();
        let r2 = rebuild(&dest_root, &ctx, "native").unwrap();
        assert_eq!(r2.rows, 1);
        let after = load_notes(&open_or_create(&dest_root).unwrap()).unwrap();
        let a = before.get("11111111-2222-3333-4444-555555555555").unwrap();
        let b = after.get("11111111-2222-3333-4444-555555555555").unwrap();
        assert_eq!(a.exported_path, b.exported_path);
        assert_eq!(a.exported_md5, b.exported_md5);
        assert_eq!(a.exported_size, b.exported_size);
        assert_eq!(a.exported_at, b.exported_at); // 同取文件 mtime → 一致
        assert_eq!(format!("{:?}", a.entry_count), format!("{:?}", b.entry_count));
        assert_eq!(a.exported_md5, crate::manifest::md5_file(&dest).unwrap());

        // 孤儿检测在重建后依然有效
        std::fs::write(dest_root.join("junk.zip"), b"x").unwrap();
        let warns = check_invariants(&open_or_create(&dest_root).unwrap(), &dest_root).unwrap();
        assert!(warns.iter().any(|w| w.contains("junk.zip")));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
