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

/// `note.origin` 取值（§3.3 v5）
pub const ORIGIN_WIZNOTE: &str = "wiznote";
/// `note.origin` 取值：库内新建（本期未做新建，预留给 P3 之后）
pub const ORIGIN_LOCAL: &str = "local";
/// `note.content_format` 取值：正文为 HTML（native 包；也是历史库的默认）
pub const FORMAT_HTML: &str = "html";
/// `note.content_format` 取值：正文为 Markdown（md 包）
pub const FORMAT_MARKDOWN: &str = "markdown";

/// v6（`docs/云同步逻辑.md` §3.2 / M3）：**类型级水位**的 `meta` 键 ——
/// 该类对象 `synced_revision` 的上界，即"我这一类对象已经对齐到哪儿了"。
///
/// 直接照抄为知 `WIZ_META.SYNC_INFO` 的形态（它按 key 存 `DOCUMENT` / `ATTACHMENT` /
/// `MESSAGES` / `KEY_FOLDERS_VERSION` 各一条水位）。**冗余存储是有意的**：为知也冗余存，
/// 好处是 O(1) 回答"这一类还有没有没上行的"，不必每次全表 max。
pub const DOCUMENT_WATERMARK: &str = "document_watermark";
/// 见 [`DOCUMENT_WATERMARK`]
pub const ATTACHMENT_WATERMARK: &str = "attachment_watermark";
/// 见 [`DOCUMENT_WATERMARK`]（墓碑维度）
pub const DELETED_WATERMARK: &str = "deleted_watermark";

/// 列存在性探测（幂等迁移与**只读路径降级**共用）。
///
/// 只读路径（`open_readonly`，如 `verify-library` / `list_trash`）刻意**不迁移**，
/// 因此它不得假定 v5 的新列存在 —— 老库上直接引用会报 `no such column`（§19.5 缺陷 ①）。
pub fn column_exists(conn: &Connection, table: &str, col: &str) -> bool {
    let Ok(mut st) = conn.prepare(&format!("PRAGMA table_info('{table}')")) else {
        return false;
    };
    let Ok(rows) = st.query_map([], |r| r.get::<_, String>(1)) else {
        return false;
    };
    let found = rows.flatten().any(|c| c == col);
    found
}

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
    /// 清单格式标识，恒为 `export::EXPORT_MODE`（`"native"`）。**只参与导出复用判定**
    /// （值不同即视为需重导），不承载任何「按格式分支」的含义
    pub export_mode: String,
    pub exported_at: String,
    /// v5（§3.3）：来源标注 —— [`ORIGIN_WIZNOTE`]（导入自源）/ [`ORIGIN_LOCAL`]（库内新建）
    pub origin: String,
    /// v5（§3.3）：正文表示 —— [`FORMAT_HTML`] / [`FORMAT_MARKDOWN`]
    pub content_format: String,
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
  export_mode     TEXT NOT NULL,             -- 格式标识：'md'（库，§20.3）/ 'native'（导出逃生舱）；仅参与复用比对与准入
  exported_at     TEXT NOT NULL,
  revision        INTEGER NOT NULL DEFAULT 0, -- v3：本地内容修订计数器。**v6 起降级为仅 UI 展示/排序**，
                                              --     任何判据不得读它（`docs/云同步逻辑.md` §4.1 纪律）
  origin          TEXT NOT NULL DEFAULT 'wiznote', -- v5：'wiznote'（导入自源）/ 'local'（库内新建，§3.3）
  content_format  TEXT NOT NULL DEFAULT 'html',    -- v5：正文表示 'html' / 'markdown'（§3.3）
  dirty_info      INTEGER NOT NULL DEFAULT 0, -- v6：脏闩·元信息段（标题/目录/URL 变）——写侧置、同步侧清
  dirty_data      INTEGER NOT NULL DEFAULT 0, -- v6：脏闩·正文段（内容变）——同上
  synced_revision INTEGER NOT NULL DEFAULT 0  -- v6：该篇内容**最后一次进入云端**时拿到的铸造号
);
CREATE INDEX IF NOT EXISTS idx_note_location ON note(location);
CREATE INDEX IF NOT EXISTS idx_note_modified ON note(data_modified);
CREATE TABLE IF NOT EXISTS deleted (         -- 墓碑：源库已消失 / 库内被删的篇目
  guid         TEXT PRIMARY KEY,
  last_path    TEXT NOT NULL,
  removed_at   TEXT NOT NULL,
  title        TEXT,                         -- v2：换机端 _trash 列表展示（Q8）
  size         INTEGER,
  exported_md5 TEXT,
  note_json    TEXT,                         -- v4：删除前 note 行的完整快照（库内删除⇄恢复用）
  trash_rel    TEXT,                         -- v4：该篇在 _trash/ 下的实际相对路径（同日同名会加后缀）
  synced_revision INTEGER NOT NULL DEFAULT 0 -- v6：该墓碑发布时拿到的铸造号（0 = 尚未上行）
);
CREATE TABLE IF NOT EXISTS attachment (      -- v2（§5.4）：换机端附件归属复原（Q15：file_path 相对源数据根）
  file_path     TEXT PRIMARY KEY,
  display_name  TEXT NOT NULL,
  size          INTEGER NOT NULL,
  tier          INTEGER NOT NULL,            -- 1/2/3/4；0 = DB 侧缺失（仅元数据，无文件）
  document_guid TEXT,                        -- 单归属时为目标；多归属/未归属为 NULL（见 attachment_doc）
  source        TEXT NOT NULL,               -- 同派生索引 attachment.source 口径
  cloud_key     TEXT,                        -- 云端对象键（相对 prefix）；跳过项（tier=0）为 NULL
  dirty          INTEGER NOT NULL DEFAULT 0, -- v6：脏闩（附件内容变过、尚未上行）
  synced_revision INTEGER NOT NULL DEFAULT 0 -- v6：最后一次铸造号
);
CREATE INDEX IF NOT EXISTS idx_attachment_guid ON attachment(document_guid);
CREATE TABLE IF NOT EXISTS attachment_doc (  -- v2：多归属展开（Tier3，5 例），保持与派生索引同构
  file_path     TEXT NOT NULL,
  document_guid TEXT NOT NULL,
  PRIMARY KEY (file_path, document_guid)
);
-- 注：schema_version 字面量须与 SCHEMA_VERSION 同步
INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '6'), ('revision', '0'),
  ('document_watermark', '0'), ('attachment_watermark', '0'), ('deleted_watermark', '0');
"#;

/// 清单 schema 版本（本期：**v6 = v5 + 云同步口径重定的三件套**，见 `docs/云同步逻辑.md`）。
/// - **v3** 只加 `note.revision`：行级内容版本。**v6 起降级为「本地内容修订计数器，仅供 UI
///   展示/排序」，任何判据不得读它**（它是唯一"两边各写各的"计数器，拿它当判据就是 D 轮
///   两处真 bug 的原形）。
/// - **v4** 只给 `deleted` 加 `note_json` / `trash_rel`：库内删除要把那一行**逐字段**存进墓碑，
///   否则回收站「恢复」只能凭文件重建行（丢 `created` / `data_modified`），与 D0「库必须无损」不符。
/// - **v5** 加 `note.origin`（`wiznote` / `local`）与 `note.content_format`（`html` / `markdown`）：
///   U6 落地。两者都有 `NOT NULL DEFAULT`，旧行自动取「导入自为知 + html」——这正是历史库的实况，
///   故升级无需逐行回填。`writable`（§3.3 第 4 列）**仍不加**：它唯一的用途是「外部目录直接成为库」
///   的只读场景，该场景（§3.4）本期未做，不预埋无消费方的空字段。
/// - **v6**（云同步逻辑 v1.0 §5）加**判据三件套**，把同步判据从"比计数器"改成"闩 + 水位 + md5"：
///   `note.dirty_info` / `note.dirty_data`（**脏闩**：写侧置、同步侧清）、
///   `note.synced_revision` / `attachment.synced_revision` / `deleted.synced_revision`
///   （该对象**最后一次被铸造**时拿到的版本号）、`attachment.dirty`、
///   以及 `meta` 的三个类型级水位键（`document_watermark` / `attachment_watermark` /
///   `deleted_watermark`）。
///   **铸版只发生在上行提交点**：`meta.revision` 不再由库内写路径推进（§8 作废清单第 2 条）。
///   `reader_base_revision` 一并废止（§4.4：只读端不再阻断下行，无需基线）。
///   **无历史数据 ⇒ 不设兼容分支**（本机无导出数据、云端 0 对象），新列默认值 0 即"干净且未发布"。
/// 未知/更高版本拒绝写入（防降级损坏，§5.4）
pub const SCHEMA_VERSION: &str = "6";

/// 打开清单后**无条件调用一次**：幂等增量迁移（v1→v2；重复执行无副作用）
pub fn migrate(conn: &Connection) -> Result<(), String> {
    // 1) v2 结构（IF NOT EXISTS：新库已在 SCHEMA_SQL 建立，v1 旧库在此补齐）
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS attachment (
          file_path     TEXT PRIMARY KEY,
          display_name  TEXT NOT NULL,
          size          INTEGER NOT NULL,
          tier          INTEGER NOT NULL,
          document_guid TEXT,
          source        TEXT NOT NULL,
          cloud_key     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_attachment_guid ON attachment(document_guid);
        CREATE TABLE IF NOT EXISTS attachment_doc (
          file_path     TEXT NOT NULL,
          document_guid TEXT NOT NULL,
          PRIMARY KEY (file_path, document_guid)
        );
        "#,
    )
    .map_err(|e| e.to_string())?;

    // 2) deleted 补三列（SQLite 无 IF NOT EXISTS，先查列存在性）
    let cols: Vec<String> = {
        let mut st = conn
            .prepare("PRAGMA table_info('deleted')")
            .map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    for (col, ddl) in [
        ("title", "ALTER TABLE deleted ADD COLUMN title TEXT"),
        ("size", "ALTER TABLE deleted ADD COLUMN size INTEGER"),
        ("exported_md5", "ALTER TABLE deleted ADD COLUMN exported_md5 TEXT"),
    ] {
        if !cols.iter().any(|c| c == col) {
            conn.execute_batch(ddl).map_err(|e| e.to_string())?;
        }
    }

    // 3) v3（§3.3）：note.revision 行级内容版本（库内写入即 +1）。SQLite 无 IF NOT EXISTS → 查列存在性
    {
        let cols: Vec<String> = {
            let mut st = conn
                .prepare("PRAGMA table_info('note')")
                .map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        if !cols.iter().any(|c| c == "revision") {
            conn.execute_batch("ALTER TABLE note ADD COLUMN revision INTEGER NOT NULL DEFAULT 0")
                .map_err(|e| e.to_string())?;
        }
    }

    // 4) v4：`deleted` 补齐「库内删除 ⇄ 恢复」所需的两列（同一套查列存在性写法，幂等）
    {
        let cols: Vec<String> = {
            let mut st = conn
                .prepare("PRAGMA table_info('deleted')")
                .map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        for (col, ddl) in [
            ("note_json", "ALTER TABLE deleted ADD COLUMN note_json TEXT"),
            ("trash_rel", "ALTER TABLE deleted ADD COLUMN trash_rel TEXT"),
        ] {
            if !cols.iter().any(|c| c == col) {
                conn.execute_batch(ddl).map_err(|e| e.to_string())?;
            }
        }
    }

    // 4) v5（§3.3）：note 的 `origin` / `content_format`（同一套查列存在性写法，幂等）。
    //    两列都有 `NOT NULL DEFAULT`，旧行自动取「导入自为知 + html」= 历史库实况，无需回填。
    {
        let cols: Vec<String> = {
            let mut st = conn
                .prepare("PRAGMA table_info('note')")
                .map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        for (col, ddl) in [
            (
                "origin",
                "ALTER TABLE note ADD COLUMN origin TEXT NOT NULL DEFAULT 'wiznote'",
            ),
            (
                "content_format",
                "ALTER TABLE note ADD COLUMN content_format TEXT NOT NULL DEFAULT 'html'",
            ),
        ] {
            if !cols.iter().any(|c| c == col) {
                conn.execute_batch(ddl).map_err(|e| e.to_string())?;
            }
        }
    }

    // 5) v6（`docs/云同步逻辑.md` §5）：**判据三件套**。
    //    同一套查列存在性写法，幂等。全部 `NOT NULL DEFAULT 0` ⇒ 新库/旧库语义自洽：
    //    "干净且未发布"。**不设兼容分支**（无历史数据），也不回填 —— 回填反而会造假：
    //    v5 库里"某篇 revision>0"与"它有没有未上行改动"根本不是一回事。
    {
        let note_cols: Vec<String> = {
            let mut st = conn.prepare("PRAGMA table_info('note')").map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        for (col, ddl) in [
            ("dirty_info", "ALTER TABLE note ADD COLUMN dirty_info INTEGER NOT NULL DEFAULT 0"),
            ("dirty_data", "ALTER TABLE note ADD COLUMN dirty_data INTEGER NOT NULL DEFAULT 0"),
            (
                "synced_revision",
                "ALTER TABLE note ADD COLUMN synced_revision INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            if !note_cols.iter().any(|c| c == col) {
                conn.execute_batch(ddl).map_err(|e| e.to_string())?;
            }
        }
        let att_cols: Vec<String> = {
            let mut st = conn
                .prepare("PRAGMA table_info('attachment')")
                .map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        for (col, ddl) in [
            ("dirty", "ALTER TABLE attachment ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0"),
            (
                "synced_revision",
                "ALTER TABLE attachment ADD COLUMN synced_revision INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            if !att_cols.iter().any(|c| c == col) {
                conn.execute_batch(ddl).map_err(|e| e.to_string())?;
            }
        }
        let del_cols: Vec<String> = {
            let mut st = conn.prepare("PRAGMA table_info('deleted')").map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        if !del_cols.iter().any(|c| c == "synced_revision") {
            conn.execute_batch(
                "ALTER TABLE deleted ADD COLUMN synced_revision INTEGER NOT NULL DEFAULT 0",
            )
            .map_err(|e| e.to_string())?;
        }
        // 类型级水位键（照为知 `WIZ_META.SYNC_INFO` 冗余存水位）：缺则建 0
        for k in [DOCUMENT_WATERMARK, ATTACHMENT_WATERMARK, DELETED_WATERMARK] {
            if get_meta(conn, k)?.is_none() {
                set_meta(conn, k, "0")?;
            }
        }
    }

    // 6) 版本推进：v1..v5/无版本 → v6；未知更高版本拒绝
    let ver = get_meta(conn, "schema_version")?;
    match ver.as_deref() {
        None => set_meta(conn, "schema_version", SCHEMA_VERSION)?,
        Some("1") | Some("2") | Some("3") | Some("4") | Some("5") | Some(SCHEMA_VERSION) => {
            set_meta(conn, "schema_version", SCHEMA_VERSION)?
        }
        Some(other) => {
            return Err(format!(
                "SYNC_MANIFEST_VERSION: 清单 schema_version={other} 高于本程序支持的 {SCHEMA_VERSION}，拒绝打开（防降级损坏）"
            ))
        }
    }
    Ok(())
}

/// `open_or_create` + `migrate` 的标准入口（阶段二起所有同步路径使用）
pub fn open_and_migrate(dest_root: &Path) -> Result<Connection, String> {
    let conn = open_or_create(dest_root)?;
    migrate(&conn)?;
    Ok(conn)
}

/// **只读**打开现有清单（mode=ro URI）：不建库、不迁移、不写任何字节。
/// 供一致性自检 / 巡检使用 —— 自检是诊断动作，不该有副作用（尤其对真实库）。
/// 前提：清单已存在（不存在 → `MANIFEST_MISSING`，调用方先跑 `validate_library`）。
pub fn open_readonly(dest_root: &Path) -> Result<Connection, String> {
    let db = manifest_path(dest_root);
    if !db.is_file() {
        return Err(format!("MANIFEST_MISSING: {}", db.display()));
    }
    let uri = format!("file:{}?mode=ro", crate::indexer::url_encode_path(&db));
    Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- attachment（v2）

/// 库内附件的**系统保留目录**（下划线前缀，浏览侧不显示）。
///
/// 约定来源：`需求分析文档.md` §FR-11 第 1 条 —— 库根只有「目录树 + 每篇 zip + `export.db`」，
/// `_trash/`、`_conflicts/`、`_attachments/` 是保留区。
///
/// 为什么不能落库根顶层 `attachments/`（A1'）：库根本身就是笔记目录树与同步根，
/// 顶层 `attachments/` 会与同名笔记目录混淆；且 `check_invariants` 的孤儿扫描只豁免
/// 顶层 `_` 前缀目录 ⇒ 附件里若含 `.zip` 会被误报「孤儿文件（无清单行）」。
pub const ATTACH_DIR: &str = "_attachments";

/// Tier 4（未与任何笔记关联）附件的保留目录。清单里 `file_path` 本就是
/// `_unlinked_attachments/…`，故 [`disk_rel_path`] 对它原样返回。
pub const UNLINKED_ATTACH_DIR: &str = "_unlinked_attachments";

/// 清单 `attachment.file_path`（**源相对路径**，Q15）→ **库内磁盘相对路径**。
///
/// - `attachments/x.log`（Tier 1–3）→ `_attachments/x.log`
/// - `_unlinked_attachments/x`（Tier 4）与其它形态 **原样返回**（它们本就在保留区）
///
/// **只影响磁盘落点**：清单 `file_path` 与云端对象键推导仍保持源相对口径
/// （`{prefix}/native/attachments/…`）—— 键即协议，不随本地布局改动。
pub fn disk_rel_path(file_path: &str) -> String {
    match file_path.strip_prefix("attachments/") {
        Some(rest) => format!("{ATTACH_DIR}/{rest}"),
        None => file_path.to_string(),
    }
}

/// `attachment` 表一行（§5.4）
#[derive(Debug, Clone)]
pub struct ManifestAttachment {
    /// 相对源数据根的路径（Q15），如 `attachments/{0f15…}band-rdma-server58-1v1.log`
    pub file_path: String,
    pub display_name: String,
    pub size: i64,
    pub tier: i64,
    /// 单归属时为目标 guid；多归属（Tier3）/未归属（Tier4）为 NULL，明细在 attachment_doc
    pub document_guid: Option<String>,
    pub source: String,
    /// 云端对象键（相对 prefix）；tier=0（db-missing，无文件）为 NULL
    pub cloud_key: Option<String>,
}

pub fn load_attachments(conn: &Connection) -> Result<Vec<ManifestAttachment>, String> {
    let mut st = conn
        .prepare(
            "SELECT file_path, display_name, size, tier, document_guid, source, cloud_key
             FROM attachment ORDER BY file_path",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(ManifestAttachment {
                file_path: r.get(0)?,
                display_name: r.get(1)?,
                size: r.get(2)?,
                tier: r.get(3)?,
                document_guid: r.get(4)?,
                source: r.get(5)?,
                cloud_key: r.get(6)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

fn upsert_attachment(conn: &Connection, a: &ManifestAttachment) -> Result<(), String> {
    conn.execute(
        "INSERT INTO attachment(file_path, display_name, size, tier, document_guid, source, cloud_key)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(file_path) DO UPDATE SET
           display_name=?2, size=?3, tier=?4, document_guid=?5, source=?6, cloud_key=?7",
        rusqlite::params![
            a.file_path,
            a.display_name,
            a.size,
            a.tier,
            a.document_guid,
            a.source,
            a.cloud_key,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn set_attachment_cloud_key(conn: &Connection, file_path: &str, cloud_key: Option<&str>) -> Result<(), String> {
    conn.execute(
        "UPDATE attachment SET cloud_key = ?2 WHERE file_path = ?1",
        rusqlite::params![file_path, cloud_key],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 把派生索引（~/.wizreader/index.db）的 `attachment` + `attachment_doc` 装入清单（§5.4）。
/// - `file_path` 落盘为**相对 data_dir** 的路径（Q15）：已是相对路径的原样保留（幂等），
///   绝对路径则剥去 data_dir 前缀；在 data_dir 之外的行记 warning 并跳过；
/// - 多归属（Tier3）行 document_guid 置 NULL、明细入 attachment_doc；
/// - 幂等：重复执行结果一致（INSERT OR REPLACE）。
pub fn populate_attachments_from_index(
    conn: &Connection,
    derived_index_db: &Path,
    data_dir: &Path,
) -> Result<Vec<String>, String> {
    let src = Connection::open_with_flags(
        derived_index_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("打开派生索引失败: {e}"))?;

    #[derive(Debug)]
    struct Row {
        file_path: String,
        display_name: String,
        size: i64,
        tier: i64,
        document_guid: Option<String>,
        source: String,
    }
    let mut st = src
        .prepare(
            "SELECT file_path, display_name, ifnull(size,0), ifnull(tier,0),
                    document_guid, ifnull(source,'')
             FROM attachment ORDER BY file_path",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<Row> = st
        .query_map([], |r| {
            Ok(Row {
                file_path: r.get(0)?,
                display_name: r.get(1)?,
                size: r.get(2)?,
                tier: r.get(3)?,
                document_guid: r.get(4)?,
                source: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    drop(st);

    // 多归属展开（Tier3）
    let mut docs: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut st = src
            .prepare("SELECT file_path, document_guid FROM attachment_doc")
            .map_err(|e| e.to_string())?;
        let pairs = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .flatten();
        for (fp, dg) in pairs {
            docs.entry(fp).or_default().push(dg);
        }
    }

    let data_root = data_dir.canonicalize().unwrap_or_else(|_| data_dir.to_path_buf());
    let mut warnings = Vec::new();
    let mut count = 0usize;
    for r in rows {
        // Q15：绝对路径 → 相对（保留全部层级，不取 basename）
        let rel = {
            let p = Path::new(&r.file_path);
            match p.strip_prefix(&data_root).or_else(|_| p.strip_prefix(data_dir)) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => {
                    if Path::new(&r.file_path).is_absolute() {
                        warnings.push(format!("附件在源数据目录之外，跳过: {}", r.file_path));
                        continue;
                    }
                    r.file_path.clone() // 已是相对路径（幂等重跑）
                }
            }
        };
        let multi = docs.get(&r.file_path);
        let document_guid = match multi {
            Some(v) if v.len() == 1 => Some(v[0].clone()),
            _ => r.document_guid.clone(), // 多归属/未归属 → NULL，明细在 attachment_doc
        };
        let att = ManifestAttachment {
            file_path: rel.clone(),
            display_name: r.display_name.clone(),
            size: r.size,
            tier: r.tier,
            document_guid,
            source: r.source.clone(),
            cloud_key: None, // 上行时回填
        };
        upsert_attachment(conn, &att)?;
        if let Some(v) = multi {
            if v.len() > 1 {
                for dg in v {
                    conn.execute(
                        "INSERT OR REPLACE INTO attachment_doc(file_path, document_guid) VALUES (?1, ?2)",
                        rusqlite::params![rel, dg],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        count += 1;
    }
    if count > 0 {
        warnings.insert(0, format!("清单装入附件 {count} 条"));
    }
    Ok(warnings)
}

/// 不变量自检第 4 条（§5.4/§9）：attachment 行数与磁盘上的附件实体一致。
///
/// **口径（A1' 后）**：清单 `file_path` 是**源相对路径**（`attachments/…` / `_unlinked_attachments/…`），
/// 库内实体落**系统保留区**（`_attachments/…` / `_unlinked_attachments/…`）⇒ 两侧都用
/// [`disk_rel_path`] 映射到库内磁盘相对路径后再比对，告警文本也以磁盘口径列出。
///
/// 注意：本函数目前**没有调用者**（`check_invariants_impl` 只实现了第 1–3 条），
/// 属待接线项；此处先把口径改对并配单测，避免接线时按错目录比对。
pub fn check_attachment_invariants(conn: &Connection, data_dir: &Path) -> Result<Vec<String>, String> {
    let mut warns = Vec::new();
    let rows = load_attachments(conn)?;

    // 磁盘侧：两个保留区内的实体文件，落库内磁盘相对路径
    let mut disk: HashSet<String> = HashSet::new();
    for dir in [ATTACH_DIR, UNLINKED_ATTACH_DIR] {
        let d = data_dir.join(dir);
        if !d.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
            if e.path().is_file() && e.file_name() != ".DS_Store" {
                disk.insert(format!("{dir}/{}", e.file_name().to_string_lossy()));
            }
        }
    }
    // 清单侧：同口径映射；tier=0（db-missing）行无实体文件，排除
    let listed: HashSet<String> = rows
        .iter()
        .filter(|a| a.tier != 0)
        .map(|a| disk_rel_path(&a.file_path))
        .collect();

    let n_rows_with_file = rows.iter().filter(|a| a.tier != 0).count();
    if rows.len() != n_rows_with_file {
        warns.push(format!("attachment 含 tier=0 行 {} 条（仅元数据，不计文件数）", rows.len() - n_rows_with_file));
    }
    for f in disk.difference(&listed) {
        warns.push(format!("磁盘附件无清单行: {f}"));
    }
    for f in listed.difference(&disk) {
        warns.push(format!("清单行缺文件: {f}"));
    }
    Ok(warns)
}

pub fn load_notes(conn: &Connection) -> Result<HashMap<String, ManifestNote>, String> {
    // v5 列探测：`open_readonly` 刻意不迁移，故读侧不得假定 `origin` / `content_format` 存在
    // （老库上直接引用会报 `no such column`，§3.3 / §19.5 缺陷 ①）。缺失时按 v4 的隐含默认取值。
    let origin_expr = if column_exists(conn, "note", "origin") {
        "origin".to_string()
    } else {
        format!("'{ORIGIN_WIZNOTE}' AS origin")
    };
    let cf_expr = if column_exists(conn, "note", "content_format") {
        "content_format".to_string()
    } else {
        format!("'{FORMAT_HTML}' AS content_format")
    };
    let sql = format!(
        // `ifnull(has_attachment,0)`：老清单上该列可为 NULL（v1 的 DDL 没写 NOT NULL DEFAULT），
        // 而 `r.get::<_,i64>` 遇 NULL 会报错 → 整行被 `flatten()` 丢掉，表现为"库里有清单却读不到任何篇"。
        // 读侧必须比写侧宽容（§19.5 缺陷 ① 的同类问题）。
        // 注：v6 的**脏闩与 synced_revision 刻意不在这里读** —— 它们是判据专用量，只经
        // [`load_unsynced_note_guids`] / [`load_note_synced_revisions`] 访问。这样 `ManifestNote`
        // 就永远不会携带"可用于判据的计数器"，从类型上就挡住了"顺手拿 revision 比大小"。
        "SELECT guid, title, location, created, data_modified, url, doc_type,
                ifnull(has_attachment, 0), package_size, exported_path, exported_size,
                exported_md5, export_mode, exported_at, {origin_expr}, {cf_expr}
         FROM note"
    );
    let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
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
                exported_at: r.get(13)?,
                origin: r.get(14)?,
                content_format: r.get(15)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().map(|n| (n.guid.clone(), n)).collect())
}

/// 全表**行级 revision**（v3 列）→ `guid → revision`。U5 冲突检测用（与远端清单逐篇比对）。
/// 列缺失（v2 老库）→ 返回空表（调用方按 0 处理），不报错。
pub fn load_note_revisions(conn: &Connection) -> Result<HashMap<String, i64>, String> {
    if !column_exists(conn, "note", "revision") {
        return Ok(HashMap::new());
    }
    let mut st = conn
        .prepare("SELECT guid, ifnull(revision, 0) FROM note")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 表是否存在（迁移与只读路径降级共用；用法同 [`column_exists`]）
pub fn table_exists(conn: &Connection, table: &str) -> bool {
    let found = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name=?1")
        .ok()
        .and_then(|mut st| {
            let rows = st.query_map([table], |r| r.get::<_, String>(0)).ok()?;
            Some(rows.flatten().any(|n| n == table))
        })
        .unwrap_or(false);
    found
}

/// 墓碑（`deleted` 表）里的 guid 集合。
///
/// 用途：**区分"远端删过这篇"与"远端从没见过这篇"** —— 两者在 `note` 表里都表现为"没有"，
/// 但同步语义完全相反：前者是正常删除传播（下行把本地文件移进 `_trash`），
/// 后者是本地新增（下行会把这份"清单里没有的文件"当成垃圾处理）。只读端护栏（R7）靠它避免
/// 把前一种误判成"本地改过"而拒绝下行。
pub fn load_tombstone_guids(conn: &Connection) -> Result<HashSet<String>, String> {
    if !table_exists(conn, "deleted") {
        return Ok(HashSet::new());
    }
    let mut st = conn
        .prepare("SELECT guid FROM deleted")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

pub fn upsert_note(conn: &Connection, n: &ManifestNote) -> Result<(), String> {
    // v6：**新插入的行一律置脏**（`dirty_info=1, dirty_data=1`）—— 新行按定义就是
    // "有内容待上行"，这就是 `docs/云同步逻辑.md` §5 末条的"导入即置脏"。
    // **UPDATE 支刻意不碰闩**：复用支（内容未变的重写，如导出增量复用）必须保住原有闩值，
    // 否则每跑一次导出就会把全库置脏 ⇒ 增量上行彻底失效。
    // 由"INSERT 置脏 / UPDATE 不动"这一对行为覆盖所有建库路径（导入、重建、CLI 造库），
    // 不依赖任何调用方记得去置脏。
    conn.execute(
        "INSERT INTO note(guid, title, location, created, data_modified, url, doc_type,
                          has_attachment, package_size, exported_path, exported_size,
                          exported_md5, export_mode, exported_at, origin, content_format,
                          dirty_info, dirty_data)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,1,1)
         ON CONFLICT(guid) DO UPDATE SET
           title=?2, location=?3, created=?4, data_modified=?5, url=?6, doc_type=?7,
           has_attachment=?8, package_size=?9, exported_path=?10, exported_size=?11,
           exported_md5=?12, export_mode=?13, exported_at=?14, origin=?15, content_format=?16",
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
            n.exported_at,
            n.origin,
            n.content_format,
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

/// 清单版本号自增（§8：单调递增、不依赖跨机时钟）；返回新值。
///
/// **v6 起这是"铸版"动作本身**（`docs/云同步逻辑.md` §4.2 第 3 步）：只有 `sync_up` 的
/// **提交点**能调用它，且必须与"清闩 + 写 `synced_revision` + 推水位"同处一个事务。
/// **库内写路径不得调用**（§8 作废清单第 2 条）—— 本地改动只置闩，不铸版。
pub fn bump_revision(conn: &Connection, updated_at: &str) -> Result<u64, String> {
    let old = get_meta(conn, "revision")?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let rev = old + 1;
    set_meta(conn, "revision", &rev.to_string())?;
    set_meta(conn, "updated_at", updated_at)?;
    Ok(rev)
}

// ------------------------------------------------- v6 判据三件套：闩 / 铸造号 / 水位
//
// 全部经**专用函数**访问，刻意不挂进 `ManifestNote`：判据只允许读这三样量，
// 而 `ManifestNote` 里带着 `data_modified` / `package_size` 等"看起来像版本"的字段，
// 让闩与它们混在一个结构体里，早晚会有人顺手拿去做判断。类型上分开，纪律才守得住。

/// 置**脏闩**（M2：写侧置、同步侧清）。`info` = 标题/目录/URL 变了；`data` = 内容变了。
///
/// 用 `OR` 语义累加：一次写路径只置自己那一段，不把另一段已有的脏擦掉。
/// 调用方必须与内容写入**同处一个事务**（置了闩却没写成内容 = 下次上传一份旧内容）。
pub fn mark_note_dirty(conn: &Connection, guid: &str, info: bool, data: bool) -> Result<(), String> {
    conn.execute(
        "UPDATE note SET dirty_info = dirty_info | ?2, dirty_data = dirty_data | ?3 WHERE guid = ?1",
        rusqlite::params![guid, info as i64, data as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 全表置脏（`dirty_data=1`）。用途：整库重建/批量导入后，让第一篇不落地就被漏传 ——
/// 新行按定义就是"有内容待上行"（`docs/云同步逻辑.md` §5 末条的"导入即置脏"）。
/// 列不存在（v5 老库）→ 返回 0，不报错（`rebuild` 走 `open_or_create`，不保证已迁移）。
pub fn mark_all_notes_dirty(conn: &Connection) -> Result<usize, String> {
    if !column_exists(conn, "note", "dirty_data") {
        return Ok(0);
    }
    conn.execute("UPDATE note SET dirty_data = 1", []).map_err(|e| e.to_string())
}

/// 全表附件置脏（同上，附件维度）。
pub fn mark_all_attachments_dirty(conn: &Connection) -> Result<usize, String> {
    if !column_exists(conn, "attachment", "dirty") {
        return Ok(0);
    }
    conn.execute("UPDATE attachment SET dirty = 1", []).map_err(|e| e.to_string())
}

/// **本地有未上行改动**的篇目集合 —— 上行差量的**唯一判据**（§4.2 第 1 步）。
///
/// 注意与 [`load_dirty_guids`] 的区别：那个的语义是"**曾经**被本地写过"（`revision > 0`），
/// 服务于导出覆盖守卫（"别用源版本盖掉用户改过的篇"），是**历史**问题；
/// 这个是"**现在**有没有还没上行的改动"，是**同步**问题。二者的生命周期不同：
/// 同步成功后本集合清空，而 `revision` 永远 > 0。**不得互换使用。**
pub fn load_unsynced_note_guids(conn: &Connection) -> Result<HashSet<String>, String> {
    if !column_exists(conn, "note", "dirty_data") {
        return Ok(HashSet::new()); // v5 老库（只读打开）：无闩 ⇒ 视为无待上行项
    }
    let mut st = conn
        .prepare("SELECT guid FROM note WHERE dirty_info <> 0 OR dirty_data <> 0")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 有未上行改动的篇目（`guid, dirty_info, dirty_data`）—— 供**报告/日志归因**用，
/// 不是判据（判据是 [`load_unsynced_note_guids`] 的集合是否为空）。
pub fn load_unsynced_note_detail(conn: &Connection) -> Result<Vec<(String, bool, bool)>, String> {
    if !column_exists(conn, "note", "dirty_data") {
        return Ok(Vec::new());
    }
    let mut st = conn
        .prepare("SELECT guid, dirty_info, dirty_data FROM note WHERE dirty_info <> 0 OR dirty_data <> 0")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0, r.get::<_, i64>(2)? != 0))
        })
        .map_err(|e| e.to_string())?;
    let mut v: Vec<_> = rows.flatten().collect();
    v.sort();
    Ok(v)
}

/// 附件维度有未上行改动的 `file_path` 集合（同上，判据专用）。
pub fn load_unsynced_attachment_paths(conn: &Connection) -> Result<HashSet<String>, String> {
    if !column_exists(conn, "attachment", "dirty") {
        return Ok(HashSet::new());
    }
    let mut st = conn
        .prepare("SELECT file_path FROM attachment WHERE dirty <> 0")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 尚未发布过的墓碑（`synced_revision = 0`）—— 上行差量的墓碑维度。
pub fn load_unpublished_tombstone_guids(conn: &Connection) -> Result<Vec<String>, String> {
    if !column_exists(conn, "deleted", "synced_revision") {
        return Ok(Vec::new());
    }
    let mut st = conn
        .prepare("SELECT guid FROM deleted WHERE synced_revision = 0")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut v: Vec<String> = rows.flatten().collect();
    v.sort();
    Ok(v)
}

/// `guid → synced_revision`（该篇最后一次进入云端的铸造号）。
/// **v6 里它是判据量，不只是命名用**：`sync_up` 的冲突判据第三条 = 「远端该篇 `synced_revision`
/// > 本端该篇 `synced_revision`」（= 条件 PUT 的本地代理），`sync_down` 的留档定名也用它。
pub fn load_note_synced_revisions(conn: &Connection) -> Result<HashMap<String, i64>, String> {
    if !column_exists(conn, "note", "synced_revision") {
        return Ok(HashMap::new());
    }
    let mut st = conn
        .prepare("SELECT guid, ifnull(synced_revision, 0) FROM note")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 单独改某篇的 `synced_revision`（**下行回写"本地版胜出"行时专用**）。
///
/// 用途很窄但不可省：`sync_down` 会把清单整体换成远端那一份，而"本地脏且内容分歧"的篇目
/// 末尾又要把**本地行**盖回去。此时 `synced_revision` 必须回写成本端**原来的**号，而不是照抄
/// 远端那个 —— 这个号回答的是"我这一版基于云端哪一号"。照抄远端会让 `sync_up` 的第三条判据
/// （远端 srev > 本端 srev）恒假 ⇒ 下一次上行**判不出真冲突**，把别人那一版静默覆盖。
pub fn set_note_synced_revision(conn: &Connection, guid: &str, rev: i64) -> Result<(), String> {
    if !column_exists(conn, "note", "synced_revision") {
        return Ok(());
    }
    conn.execute(
        "UPDATE note SET synced_revision = ?2 WHERE guid = ?1",
        rusqlite::params![guid, rev],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 清闩 + 记铸造号（**提交点专用**）：上行成功的篇目，闩清零、`synced_revision` = 本轮铸造号。
/// 必须在**铸版同一个事务**里调用（`docs/云同步逻辑.md` §4.2 第 3 步）。
pub fn mark_notes_synced(conn: &Connection, guids: &[String], rev: u64) -> Result<usize, String> {
    let mut n = 0;
    for g in guids {
        n += conn
            .execute(
                "UPDATE note SET dirty_info = 0, dirty_data = 0, synced_revision = ?2 WHERE guid = ?1",
                rusqlite::params![g, rev as i64],
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(n)
}

/// 清附件闩 + 记铸造号（同上，附件维度）。
pub fn mark_attachments_synced(
    conn: &Connection,
    file_paths: &[String],
    rev: u64,
) -> Result<usize, String> {
    let mut n = 0;
    for f in file_paths {
        n += conn
            .execute(
                "UPDATE attachment SET dirty = 0, synced_revision = ?2 WHERE file_path = ?1",
                rusqlite::params![f, rev as i64],
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(n)
}

/// 记墓碑的铸造号（同上，墓碑维度）。
pub fn mark_tombstones_published(conn: &Connection, guids: &[String], rev: u64) -> Result<usize, String> {
    let mut n = 0;
    for g in guids {
        n += conn
            .execute(
                "UPDATE deleted SET synced_revision = ?2 WHERE guid = ?1",
                rusqlite::params![g, rev as i64],
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(n)
}

/// 读类型级水位（M3）。
pub fn watermark(conn: &Connection, key: &str) -> u64 {
    get_meta(conn, key)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// 读当前**清单版本** `meta.revision`（= 本端最后一次铸造的号；下行后等于远端清单版本）。
/// **只读**：写入只能经 [`bump_revision`]，且只许提交点调用。
pub fn current_revision(conn: &Connection) -> u64 {
    get_meta(conn, "revision")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// **重算**并写三类水位 = 各类对象 `synced_revision` 的上界。
///
/// 照为知 `WIZ_META.SYNC_INFO` 的形态（`DOCUMENT` 恰等于 `max(WIZ_VERSION)`）：
/// 水位不是"另记一个数"，而是**该类对象铸造号的最大值**，所以它永远不可能与对象脱节。
/// 下行对齐远端清单后也要重算（远端清单里的 `synced_revision` 才是权威值）。
pub fn recompute_watermarks(conn: &Connection) -> Result<(u64, u64, u64), String> {
    let max_of = |tbl: &str, col: &str| -> i64 {
        if !column_exists(conn, tbl, col) {
            return 0;
        }
        conn.query_row(&format!("SELECT ifnull(max({col}), 0) FROM {tbl}"), [], |r| r.get(0))
            .unwrap_or(0)
    };
    let d = max_of("note", "synced_revision").max(0) as u64;
    let a = max_of("attachment", "synced_revision").max(0) as u64;
    let t = max_of("deleted", "synced_revision").max(0) as u64;
    set_meta(conn, DOCUMENT_WATERMARK, &d.to_string())?;
    set_meta(conn, ATTACHMENT_WATERMARK, &a.to_string())?;
    set_meta(conn, DELETED_WATERMARK, &t.to_string())?;
    Ok((d, a, t))
}

// ---------------------------------------------------------------- 行级 revision（v3，本地计数器）

/// 单篇内容版本 +1。返回新值；行不存在返回 `None`。
/// 只由 [`crate::library`] 的写路径调用，且**必须与导出字段更新同处一个事务**（T2/R13）。
///
/// **v6 起语义降级**（`docs/云同步逻辑.md` §4.1）：它只是"本地内容修订计数器"，**仅供 UI 展示/排序**。
/// 它是唯一"两边各写各的"计数器，**任何判据不得读它** —— 同步的"本地改过没"一律读脏闩
/// （[`load_unsynced_note_guids`]）。
pub fn bump_note_revision(conn: &Connection, guid: &str) -> Result<Option<i64>, String> {
    let n = conn
        .execute("UPDATE note SET revision = revision + 1 WHERE guid = ?1", [guid])
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    load_note_revision(conn, guid)
}

/// 读单篇行级 revision（写报告展示 / P3 同步 diff 用）
pub fn load_note_revision(conn: &Connection, guid: &str) -> Result<Option<i64>, String> {
    let mut st = conn
        .prepare("SELECT revision FROM note WHERE guid = ?1")
        .map_err(|e| e.to_string())?;
    let mut rows = st
        .query_map([guid], |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?;
    match rows.next() {
        Some(v) => Ok(Some(v.map_err(|e| e.to_string())?)),
        None => Ok(None),
    }
}

/// **库内被本地写过**的篇目 guid 集合（行级 `revision > 0`）。
///
/// 用途：`export_folder_zips` 的覆盖守卫 —— 从源重导（「导入到我的笔记库」）时，
/// 不得用源库版本静默覆盖库内已被用户编辑过的篇目。
/// 注：`manifest::rebuild`（以磁盘为准的恢复工具）会 `DELETE FROM note` 重插，
/// 因此会把这些计数清零 —— 属恢复路径的已知代价。
///
/// **与 [`load_unsynced_note_guids`] 的区别（v6 起必须分清，不得互换）**：
/// 这个回答"**曾经**被本地写过吗"（历史性质，一旦真值永为真，服务导出守卫）；
/// 那个回答"**现在**还有没有没上行的改动"（同步性质，同步成功后清空，服务上行差量）。
pub fn load_dirty_guids(conn: &Connection) -> Result<HashSet<String>, String> {
    let mut st = conn
        .prepare("SELECT guid FROM note WHERE revision > 0")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
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
//
// 两档口径（调和两份设计文档的张力）：
// - **轻量档**（默认，`check_invariants`）：§5.5 条 1 原文只要求「存在且 size 相符」，
//   且导出结束 / 启动 / 每次写后都要跑 —— 全库 1780 篇须为秒级，故只 stat 不比内容。
// - **深度档**（`check_invariants_deep`）：§15.3.1 T9 的验收要求「清单行 MD5 与磁盘不符时
//   启动自检能发现并提示重建」。该检查无法廉价完成，故做成**显式调用**（CLI verify-library --deep）。
// 两档共用同一遍历骨架，只有是否读文件内容不同。

/// 导出结束后自检（**轻量档**：只 stat 体积，不读文件内容）。
/// 返回警告列表（不抛错，警告进导出报告，供抽查）。
///
/// 启动路径与写后自检走这一档 —— 全库 1780 篇 / 1.5 GB，逐篇算 MD5 需数十秒，
/// 不能挂在启动上。要查"内容被换但体积没变"用 [`check_invariants_deep`]。
pub fn check_invariants(conn: &Connection, dest_root: &Path) -> Result<Vec<String>, String> {
    check_invariants_impl(conn, dest_root, false)
}

/// 自检**深度档**（T9 验收口径）：在轻量档之上，逐篇重算 MD5 与清单行比对。
/// 「体积相符但内容不符」只有这一档能发现（崩溃注入 / 外部篡改 / 同体积覆盖）。
/// 调用方须自行承担全库遍历代价（CLI `verify-library --deep`）。
pub fn check_invariants_deep(conn: &Connection, dest_root: &Path) -> Result<Vec<String>, String> {
    check_invariants_impl(conn, dest_root, true)
}

/// `deep=true` 时逐篇重算 MD5；`false` 时只比体积
fn check_invariants_impl(
    conn: &Connection,
    dest_root: &Path,
    deep: bool,
) -> Result<Vec<String>, String> {
    let rows = load_notes(conn)?;
    let mut warns = Vec::new();

    // 1. 每行 exported_path 指向的文件必须存在、体积相符；deep 时另比 MD5
    for n in rows.values() {
        let p = dest_root.join(&n.exported_path);
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => {
                if m.len() != n.exported_size as u64 {
                    warns.push(format!(
                        "体积不符 {}: 清单 {} B / 磁盘 {} B",
                        n.exported_path, n.exported_size, m.len()
                    ));
                } else if deep && !n.exported_md5.is_empty() {
                    match md5_file(&p) {
                        Ok(disk) if disk != n.exported_md5 => warns.push(format!(
                            "MD5 不符 {}: 清单 {} / 磁盘 {}（内容被改，建议重建清单或从备份恢复）",
                            n.exported_path, n.exported_md5, disk
                        )),
                        Ok(_) => {}
                        Err(e) => warns.push(format!("MD5 计算失败 {}: {e}", n.exported_path)),
                    }
                }
            }
            _ => warns.push(format!("清单行缺文件: {}", n.exported_path)),
        }
    }
    // 2. 导出目录内未被清单覆盖的孤儿 zip（报告，不静默删除）。
    //    顶层 `_` 前缀目录是**系统保留区**（_trash / _conflicts / _unlinked_attachments，§3.1）：
    //    其中的 zip 按设计不入清单（_trash 里是已 tombstone 的篇目），故不算孤儿。
    let covered: HashSet<&str> = rows.values().map(|n| n.exported_path.as_str()).collect();
    let mut stack = vec![(dest_root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let is_reserved = depth == 0
                    && p.file_name()
                        .map(|f| f.to_string_lossy().starts_with('_'))
                        .unwrap_or(false);
                if !is_reserved {
                    stack.push((p, depth + 1));
                }
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
/// **D0**：导出目录只可能是 native，故没有模式参数——恒按 `native` 重建（口径：以磁盘为准）。
pub fn rebuild(
    dest_root: &Path,
    ctx: &crate::export::ExportContext,
) -> Result<RebuildReport, String> {
    let conn = open_or_create(dest_root)?;
    // v6：重建也要把清单升到当前 schema —— 下面 `upsert_note` 会写 v6 的闩列，
    // 不迁移的话在 v5 清单上会直接 `no such column`。
    migrate(&conn)?;
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
                export_mode: crate::export::EXPORT_MODE.to_string(),
                exported_at,
                origin: crate::manifest::ORIGIN_WIZNOTE.into(),
                content_format: crate::manifest::FORMAT_HTML.into(),
            },
        )?;
        rep.rows += 1;
    }
    // 格式标识与 note 行同口径写回，避免下游读到缺失/陈旧值
    set_meta(&conn, "export_mode", crate::export::EXPORT_MODE)?;
    // v6：重建出的行是**从未同步过**的（`synced_revision` 默认 0），且 `DELETE FROM note`
    // 把所有闩都抹平了 ⇒ 必须整表置脏，否则重建后第一次上行会**一篇都不传**
    // （`docs/云同步逻辑.md` §5 末条"导入/恢复即置脏"）。
    mark_all_notes_dirty(&conn)?;
    mark_all_attachments_dirty(&conn)?;
    recompute_watermarks(&conn)?;
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

/// `data_modified` 的写入形态：**本地时间** `YYYY-MM-DD HH:MM:SS`（无时区后缀）。
/// 理由：该字段的既有语义就是"本机看到的时间"——源库（WIZ_DOCUMENT.DT_DATA_MODIFIED）写的是
/// 本地时间且无后缀，消费方（`list_notes`/`search` 的 `ORDER BY data_modified DESC` 与
/// NoteList/Reader/SearchResults 的原样展示）都按字符串直接比较/显示。若改用 UTC，
/// 用户会看到"修改时间比实际早 8 小时"的错乱。
pub fn now_local_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
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

    /// 清单行 upsert/load 往返（含中文标题与路径；二次 upsert 不覆盖为重复行）
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
            export_mode: crate::export::EXPORT_MODE.into(),
            exported_at: "2026-09-16 00:00:00Z".into(),
            origin: crate::manifest::ORIGIN_WIZNOTE.into(),
            content_format: crate::manifest::FORMAT_HTML.into(),
        };
        upsert_note(&conn, &n).unwrap();
        // 二次 upsert（复用路径更新元数据）不产生重复行
        upsert_note(&conn, &n).unwrap();
        let loaded = load_notes(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        let m = loaded.get(&n.guid).unwrap();
        assert_eq!(m.title, "标题<一>");
        assert_eq!(m.export_mode, crate::export::EXPORT_MODE);
        assert!(m.has_attachment);
        assert_eq!(
            get_meta(&conn, "schema_version").unwrap().as_deref(),
            Some(SCHEMA_VERSION)
        );
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
            exported_at: "2026-09-16 00:00:00Z".into(),
            origin: crate::manifest::ORIGIN_WIZNOTE.into(),
            content_format: crate::manifest::FORMAT_HTML.into(),
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
            exported_at: "2026-09-16 00:00:00Z".into(),
            origin: crate::manifest::ORIGIN_WIZNOTE.into(),
            content_format: crate::manifest::FORMAT_HTML.into(),
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

        // ---- 深度档（T9 验收口径）：体积相符但内容不符，只有 deep 能发现 ----
        std::fs::write(d.join("a/ok.zip"), b"12345").unwrap(); // 恢复成清单口径的体积
        let real = md5_file(&d.join("a/ok.zip")).unwrap();
        conn.execute(
            "UPDATE note SET exported_md5 = ?1 WHERE guid = ?2",
            rusqlite::params![real, n.guid],
        )
        .unwrap();
        assert!(
            !check_invariants_deep(&conn, &d)
                .unwrap()
                .iter()
                .any(|w| w.contains("MD5 不符")),
            "内容与清单相符时 deep 不得误报"
        );

        // 同体积换内容（这正是崩溃/篡改的典型形态：stat 全对、内容已错）
        std::fs::write(d.join("a/ok.zip"), b"ABCDE").unwrap();
        let light = check_invariants(&conn, &d).unwrap();
        assert!(
            !light.iter().any(|w| w.contains("体积不符") || w.contains("MD5 不符")),
            "轻量档结构上不可能发现同体积换内容（这是两档分工的证明）: {light:?}"
        );
        let deep = check_invariants_deep(&conn, &d).unwrap();
        assert!(
            deep.iter().any(|w| w.contains("MD5 不符") && w.contains("a/ok.zip")),
            "深度档必须发现 MD5 不符: {deep:?}"
        );

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
        let r = rebuild(&dest_root, &ctx).unwrap();
        assert_eq!(r.rows, 1);
        let before = load_notes(&open_or_create(&dest_root).unwrap()).unwrap();
        // D0：重建恒 native，且显式写回 meta.export_mode（下游同步据此判定准入）
        assert_eq!(
            get_meta(&open_or_create(&dest_root).unwrap(), "export_mode").unwrap().as_deref(),
            Some(crate::export::EXPORT_MODE)
        );
        std::fs::remove_file(manifest_path(&dest_root)).unwrap();
        let r2 = rebuild(&dest_root, &ctx).unwrap();
        assert_eq!(r2.rows, 1);
        let after = load_notes(&open_or_create(&dest_root).unwrap()).unwrap();
        let a = before.get("11111111-2222-3333-4444-555555555555").unwrap();
        let b = after.get("11111111-2222-3333-4444-555555555555").unwrap();
        assert_eq!(a.exported_path, b.exported_path);
        assert_eq!(a.exported_md5, b.exported_md5);
        assert_eq!(a.exported_size, b.exported_size);
        assert_eq!(a.exported_at, b.exported_at); // 同取文件 mtime → 一致
        assert_eq!(a.exported_md5, crate::manifest::md5_file(&dest).unwrap());

        // 孤儿检测在重建后依然有效
        std::fs::write(dest_root.join("junk.zip"), b"x").unwrap();
        let warns = check_invariants(&open_or_create(&dest_root).unwrap(), &dest_root).unwrap();
        assert!(warns.iter().any(|w| w.contains("junk.zip")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A1'：清单 `file_path`（源相对）→ 库内磁盘相对路径的映射
    #[test]
    fn test_disk_rel_path() {
        // Tier 1–3：源布局 `attachments/` → 库内保留区 `_attachments/`
        assert_eq!(disk_rel_path("attachments/a.log"), "_attachments/a.log");
        assert_eq!(disk_rel_path("attachments/{g}band.log"), "_attachments/{g}band.log");
        // 子路径层级不丢
        assert_eq!(disk_rel_path("attachments/sub/a.log"), "_attachments/sub/a.log");
        // Tier4：`file_path` 本就在保留区 → 原样（不得再套一层）
        assert_eq!(disk_rel_path("_unlinked_attachments/u.log"), "_unlinked_attachments/u.log");
        // 其它形态原样（不误伤）
        assert_eq!(disk_rel_path("db-missing:x"), "db-missing:x");
        assert_eq!(disk_rel_path(""), "");
        // 关键反例：`attachments`（无斜杠）与 `attachmentsX/` 都不是附件目录前缀
        assert_eq!(disk_rel_path("attachments"), "attachments");
        assert_eq!(disk_rel_path("attachmentsX/a.log"), "attachmentsX/a.log");
        // 前缀只剥一次（不得退化成同名目录自套）
        assert_eq!(
            disk_rel_path("attachments/attachments/a.log"),
            "_attachments/attachments/a.log"
        );
    }

    /// A1'：附件自检按「保留区实体 ↔ 清单行（同口径映射）」比对。
    /// 旧实现钉死扫 `data_dir/attachments/` ⇒ 落点改到 `_attachments/` 后会把
    /// 每一条 Tier1–3 都误报为「清单行缺文件」。
    #[test]
    fn test_check_attachment_invariants_uses_reserved_dir() {
        let d = temp_dir("att-inv");
        let conn = open_and_migrate(&d).unwrap();

        // tier1 实体在 `_attachments/`，tier4 实体在 `_unlinked_attachments/`
        std::fs::create_dir_all(d.join(ATTACH_DIR)).unwrap();
        std::fs::write(d.join(ATTACH_DIR).join("a.log"), b"hello").unwrap();
        std::fs::create_dir_all(d.join(UNLINKED_ATTACH_DIR)).unwrap();
        std::fs::write(d.join(UNLINKED_ATTACH_DIR).join("u.log"), b"x").unwrap();
        for (fp, tier) in [("attachments/a.log", 1i64), ("_unlinked_attachments/u.log", 4)] {
            conn.execute(
                "INSERT OR REPLACE INTO attachment(file_path, display_name, size, tier, document_guid, source)
                 VALUES (?1, ?1, 5, ?2, NULL, 'db-record')",
                rusqlite::params![fp, tier],
            )
            .unwrap();
        }
        // 行与实体齐 → 零告警
        let w = check_attachment_invariants(&conn, &d).unwrap();
        assert!(w.is_empty(), "落点与清单齐时不得有告警: {w:?}");

        // 清单有行、实体缺失（告警文本用磁盘口径）
        std::fs::remove_file(d.join(ATTACH_DIR).join("a.log")).unwrap();
        let w = check_attachment_invariants(&conn, &d).unwrap();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("清单行缺文件") && w[0].contains("_attachments/a.log"),
            "{w:?}"
        );

        // 磁盘有实体、清单无行（含 .zip：证明保留区内不会被孤儿扫描误报）
        std::fs::write(d.join(ATTACH_DIR).join("stray.zip"), b"z").unwrap();
        let w = check_attachment_invariants(&conn, &d).unwrap();
        assert!(
            w.iter().any(|x| x.contains("磁盘附件无清单行") && x.contains("stray.zip")),
            "{w:?}"
        );

        // 顶层 `_attachments/` 里的 zip **不**算孤儿（`check_invariants` 只豁免顶层 `_` 前缀目录）
        let stray = check_invariants(&conn, &d).unwrap();
        assert!(
            !stray.iter().any(|x| x.contains("stray.zip")),
            "保留区内的附件不应进孤儿扫描: {stray:?}"
        );

        std::fs::remove_dir_all(&d).unwrap();
    }

    // ---------------------------------------------------------------- v5（U6）

    /// 造一个"v4 老库"：note 表**没有** origin / content_format，meta.schema_version='4'
    fn make_v4_lib(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wiz-v5-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = Connection::open(d.join(MANIFEST_NAME)).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE note (
              guid TEXT PRIMARY KEY, title TEXT NOT NULL, location TEXT NOT NULL,
              created TEXT NOT NULL, data_modified TEXT NOT NULL, url TEXT, doc_type TEXT,
              has_attachment INTEGER NOT NULL DEFAULT 0, package_size INTEGER NOT NULL,
              exported_path TEXT NOT NULL, exported_size INTEGER NOT NULL,
              exported_md5 TEXT NOT NULL, export_mode TEXT NOT NULL, exported_at TEXT NOT NULL,
              revision INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO meta(key,value) VALUES ('schema_version','4'),('revision','2');
            INSERT INTO note(guid,title,location,created,data_modified,package_size,
                             exported_path,exported_size,exported_md5,export_mode,exported_at,revision)
            VALUES ('g-old','旧篇','/d/','c','m',1,'d/旧篇.zip',1,'h','md','e',7);
            "#,
        )
        .unwrap();
        d
    }

    /// v4 → 当前版本：两列加得上、旧行取 v4 的隐含默认（导入自为知 + html）、版本推进到
    /// 当前的 `SCHEMA_VERSION`，且迁移幂等
    #[test]
    fn test_migrate_v4_to_v5_adds_provenance_columns() {
        let d = make_v4_lib("mig");
        let conn = open_and_migrate(&d).unwrap();
        assert!(column_exists(&conn, "note", "origin"), "origin 列应被补上");
        assert!(column_exists(&conn, "note", "content_format"), "content_format 列应被补上");
        assert_eq!(get_meta(&conn, "schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));

        let notes = load_notes(&conn).unwrap();
        let n = notes.get("g-old").unwrap();
        assert_eq!(n.origin, ORIGIN_WIZNOTE, "旧行取默认 wiznote");
        assert_eq!(n.content_format, FORMAT_HTML, "旧行取默认 html");
        assert_eq!(n.title, "旧篇", "既有字段不受迁移影响");
        // 行级 revision 仍可读（U5 冲突检测依赖）
        assert_eq!(load_note_revisions(&conn).unwrap().get("g-old"), Some(&7));

        // 幂等：再跑一次不报错、不重复加列
        migrate(&conn).unwrap();
        assert_eq!(get_meta(&conn, "schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
        // 高版本仍拒绝（防降级）
        set_meta(&conn, "schema_version", "99").unwrap();
        assert!(migrate(&conn).unwrap_err().contains("SYNC_MANIFEST_VERSION"));
        drop(conn);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 只读路径降级（§3.3 / §19.5 ①）：`open_readonly` 不迁移，故 `load_notes` 不得假定
    /// v5 两列存在 —— 直接引用会 `no such column`，老库上读列表就全废。
    #[test]
    fn test_load_notes_on_v4_readonly_falls_back() {
        let d = make_v4_lib("ro");
        let conn = open_readonly(&d).unwrap();
        assert!(!column_exists(&conn, "note", "origin"), "只读打开不得迁移");
        let notes = load_notes(&conn).unwrap();
        let n = notes.get("g-old").unwrap();
        assert_eq!((n.origin.as_str(), n.content_format.as_str()), (ORIGIN_WIZNOTE, FORMAT_HTML));
        // revision 列在 v4 是存在的，正常读出
        assert_eq!(load_note_revisions(&conn).unwrap().get("g-old"), Some(&7));
        drop(conn);

        // 反向对照：真的没有 revision 列时（v2）→ 空表而非报错
        let d2 = std::env::temp_dir().join(format!("wiz-v5-ro2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d2);
        std::fs::create_dir_all(&d2).unwrap();
        let c = Connection::open(d2.join(MANIFEST_NAME)).unwrap();
        c.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE note(guid TEXT PRIMARY KEY, title TEXT, location TEXT, created TEXT,
               data_modified TEXT, url TEXT, doc_type TEXT, has_attachment INTEGER,
               package_size INTEGER, exported_path TEXT, exported_size INTEGER,
               exported_md5 TEXT, export_mode TEXT, exported_at TEXT);
             INSERT INTO note(guid,title,location,created,data_modified,package_size,exported_path,
               exported_size,exported_md5,export_mode,exported_at)
             VALUES('g2','t','/d/','c','m',1,'a.zip',1,'h','native','e');",
        )
        .unwrap();
        drop(c);
        let ro = open_readonly(&d2).unwrap();
        assert!(load_note_revisions(&ro).unwrap().is_empty(), "无 revision 列 → 空表");
        assert_eq!(load_notes(&ro).unwrap().get("g2").unwrap().origin, ORIGIN_WIZNOTE);
        drop(ro);
        let _ = std::fs::remove_dir_all(&d2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// v5 两列往返：写 local/markdown → 读回原值（不是默认值）
    #[test]
    fn test_origin_and_content_format_roundtrip() {
        let d = std::env::temp_dir().join(format!("wiz-v5-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = open_and_migrate(&d).unwrap();
        let mut n = ManifestNote {
            guid: "g-rt".into(),
            title: "t".into(),
            location: "/d/".into(),
            created: "c".into(),
            data_modified: "m".into(),
            url: None,
            doc_type: None,
            has_attachment: false,
            package_size: 1,
            exported_path: "a.zip".into(),
            exported_size: 1,
            exported_md5: "h".into(),
            export_mode: "md".into(),
            exported_at: "e".into(),
            origin: ORIGIN_LOCAL.into(),
            content_format: FORMAT_MARKDOWN.into(),
        };
        upsert_note(&conn, &n).unwrap();
        let got = load_notes(&conn).unwrap();
        assert_eq!(got.get("g-rt").unwrap().origin, ORIGIN_LOCAL);
        assert_eq!(got.get("g-rt").unwrap().content_format, FORMAT_MARKDOWN);
        // upsert 冲突分支同样刷新这两列（否则改回 wiznote 会被旧值粘住）
        n.origin = ORIGIN_WIZNOTE.into();
        n.content_format = FORMAT_HTML.into();
        upsert_note(&conn, &n).unwrap();
        let got = load_notes(&conn).unwrap();
        assert_eq!(got.get("g-rt").unwrap().origin, ORIGIN_WIZNOTE);
        assert_eq!(got.get("g-rt").unwrap().content_format, FORMAT_HTML);
        drop(conn);
        let _ = std::fs::remove_dir_all(&d);
    }
}
