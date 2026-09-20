//! 笔记库服务（FR-11，docs/本地笔记读写实现.md §3/§4.1）
//!
//! 库 = 主数据目录：一个目录即一个库，含 `export.db` 清单 + 目录树 + 每篇 zip。
//! 本模块提供三件事：
//! - **准入校验**（§3.4）：外部目录被指为库根前的四项检查（清单可开 / 版本兼容 /
//!   路径安全 / 无损性 D0），产出 [`LibraryStatus`] 四态供 UI 分支；
//! - **[`LibraryResolver`]**：guid → 清单 `exported_path` → 库内绝对路径（§13.1：
//!   读路径唯一入口 = 清单，绝不拼路径）；
//! - **[`import_attachments`]**：导入时把源附件拷入库根（库自包含，删源后仍全功能可用）。
//!
//! 自 P2（§15.4）起**本模块同时是唯一的写路径**（见文末「写路径」一节）：
//! [`save_note_html`] / [`rename_note`] / [`move_note`] / [`delete_note`]，
//! 三律为 **先临时文件后 rename**、**先落盘后更清单**、**写必持锁**（§13.2 / §4.2 / §4.4）。
//!
//! 边界：清单校验一律**只读**打开（mode=ro），绝不触发 migrate 写库；
//! D0 之后库形态唯一（native），不存在"只读挂载历史瘦身库"分支。

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::Connection;

use crate::manifest;
use crate::zipserve::{normalize_guid, NotePathResolver};

/// 准入校验结果四态（§3.4 / §6.2：UI 按此分支）
#[derive(Debug, Clone, serde::Serialize)]
pub struct LibraryStatus {
    /// `empty`（空目录，可作新库）/ `ready`（有清单）/ `no_manifest`（有 zip 无清单）/ `rejected`
    pub kind: String,
    /// 拒绝原因（kind=rejected 时非空）
    pub reason: String,
    /// 清单 note 行数（ready 时有意义）
    pub note_count: usize,
    /// 清单行指向但磁盘缺失的 zip 数（>0 时提示重建索引或修复）
    pub missing_files: usize,
    /// 抽样告警（最多 10 条，不拒绝）
    pub warnings: Vec<String>,
    /// 库根路径（回显用）
    pub dir: String,
}

impl LibraryStatus {
    fn new(kind: &str, dir: &Path) -> Self {
        Self {
            kind: kind.into(),
            reason: String::new(),
            note_count: 0,
            missing_files: 0,
            warnings: Vec::new(),
            dir: dir.to_string_lossy().into_owned(),
        }
    }
    fn rejected(dir: &Path, reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            ..Self::new("rejected", dir)
        }
    }
}

/// 只读打开清单（mode=ro URI）：校验/解析路径绝不写库、绝不触发 migrate
fn open_manifest_ro(library_dir: &Path) -> Result<Connection, String> {
    let db = manifest::manifest_path(library_dir);
    let uri = format!("file:{}?mode=ro", crate::indexer::url_encode_path(&db));
    Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())
}

/// §3.4 准入校验（设置数据目录 / 启动时调用；只读，不建任何文件）
pub fn validate_library(dir: &Path) -> LibraryStatus {
    if !dir.is_dir() {
        return LibraryStatus::rejected(dir, "目录不存在");
    }
    let manifest_file = manifest::manifest_path(dir);
    if !manifest_file.is_file() {
        // 无清单：目录内有 zip → 引导重建清单；否则视为空目录（可作新库）
        return if has_zip_recursive(dir, 4) {
            LibraryStatus::new("no_manifest", dir)
        } else {
            LibraryStatus::new("empty", dir)
        };
    }
    let conn = match open_manifest_ro(dir) {
        Ok(c) => c,
        Err(e) => return LibraryStatus::rejected(dir, format!("export.db 无法打开：{e}")),
    };
    // 版本兼容：schema_version 高于本程序 → 拒绝（防降级损坏；缺失视同当前版本）
    let ver = manifest::get_meta(&conn, "schema_version")
        .ok()
        .flatten();
    if let Some(v) = &ver {
        let cur: u32 = manifest::SCHEMA_VERSION.parse().unwrap_or(0);
        match v.parse::<u32>() {
            Ok(n) if n <= cur => {}
            _ => {
                return LibraryStatus::rejected(
                    dir,
                    format!("SYNC_MANIFEST_VERSION: 清单 schema_version={v} 高于本程序支持的 {cur}"),
                )
            }
        }
    }
    // 格式标识（§3.4 / §20.4①）：`export_mode` ∈ {`md`, `native`}（缺失视同 native）；
    // 其它值 = 非本程序产出的目录 → 拒绝（不设"只读挂载历史瘦身库"分支：不存在此类数据）。
    // 注意：M2 起「导入到我的笔记库」产 **md** 包，故 `md` 与 `native` 同为合法库形态。
    let mode = manifest::get_meta(&conn, "export_mode").ok().flatten();
    if let Some(m) = &mode {
        if m != crate::export::EXPORT_MODE && m != crate::export::EXPORT_MODE_MD {
            return LibraryStatus::rejected(
                dir,
                format!(
                    "非本程序产出的导出目录（export_mode={m}，仅支持 {} / {}），请重新导出",
                    crate::export::EXPORT_MODE,
                    crate::export::EXPORT_MODE_MD
                ),
            );
        }
    }
    // 逐行路径安全 + 磁盘一致性（缺失只告警不拒绝，提示重建索引或修复）
    let mut st = LibraryStatus::new("ready", dir);
    let rows: Vec<(String, String)> = {
        let mut q = match conn.prepare("SELECT guid, exported_path FROM note") {
            Ok(q) => q,
            Err(e) => return LibraryStatus::rejected(dir, format!("清单 note 表不可读：{e}")),
        };
        q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    };
    st.note_count = rows.len();
    for (guid, p) in &rows {
        if is_unsafe_rel_path(p) {
            if st.warnings.len() < 10 {
                st.warnings.push(format!("路径不安全，跳过: {p}"));
            }
            st.missing_files += 1;
            continue;
        }
        if !dir.join(p).is_file() {
            st.missing_files += 1;
            if st.warnings.len() < 10 {
                st.warnings.push(format!("清单行缺文件: {guid} → {p}"));
            }
        }
    }
    if st.missing_files > 0 && st.warnings.len() < 10 {
        st.warnings
            .push(format!("共 {} 项缺失/不安全（建议重建索引或修复）", st.missing_files));
    }
    st
}

/// 路径安全（§3.4）：拒绝绝对路径与含 `..` 段
pub fn is_unsafe_rel_path(p: &str) -> bool {
    let path = Path::new(p);
    path.is_absolute() || p.starts_with('/') || path.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

/// 目录下（限深递归）是否存在 .zip 文件
fn has_zip_recursive(dir: &Path, depth: u32) -> bool {
    if depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file() {
            if p.extension().map(|x| x == "zip").unwrap_or(false) {
                return true;
            }
        } else if p.is_dir() && has_zip_recursive(&p, depth - 1) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- guid → 库内路径解析器

/// 库模式解析器：缓存清单 `guid → exported_path` 映射（写入/同步后经 [`reload`] 失效重载）。
/// 绝不拼路径（§13.1）——库内路径含中文/空格/emoji（R3），只有清单是权威映射。
pub struct LibraryResolver {
    library_dir: PathBuf,
    map: std::sync::Mutex<HashMap<String, String>>,
}

impl LibraryResolver {
    /// 载入清单映射；清单缺失/不可读时报错（调用方决定是否回退源模式）
    pub fn new(library_dir: PathBuf) -> Result<Self, String> {
        let r = Self {
            library_dir,
            map: std::sync::Mutex::new(HashMap::new()),
        };
        r.reload()?;
        Ok(r)
    }

    /// 重读清单（写入/同步后调用；本轮仅留接口，P2 写路径接入）
    pub fn reload(&self) -> Result<(), String> {
        let conn = open_manifest_ro(&self.library_dir)?;
        let mut st = conn
            .prepare("SELECT guid, exported_path FROM note")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        let mut map = HashMap::new();
        for row in rows.flatten() {
            if !is_unsafe_rel_path(&row.1) {
                map.insert(row.0, row.1);
            }
        }
        *self.map.lock().unwrap() = map;
        Ok(())
    }

    pub fn library_dir(&self) -> &Path {
        &self.library_dir
    }

    /// 清单篇数（索引自检用）
    pub fn note_count(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

impl NotePathResolver for LibraryResolver {
    fn resolve(&self, guid: &str) -> Option<PathBuf> {
        // 花括号形态规范化 + 防注入校验（与源模式同口径）
        let braced = normalize_guid(guid)?;
        let inner = braced.trim_matches(['{', '}']);
        let map = self.map.lock().unwrap();
        let rel = map.get(inner).or_else(|| map.get(braced.as_str()))?;
        Some(self.library_dir.join(rel))
    }
    fn mode_tag(&self) -> &'static str {
        "lib"
    }
}

// ---------------------------------------------------------------- 导入时附件入库

/// 附件入库结果（「导入到我的笔记库」报告用）
#[derive(Debug, Clone, serde::Serialize)]
pub struct AttachmentImportReport {
    /// 本轮拷入库的附件数
    pub copied: usize,
    /// 库内已在（尺寸一致）跳过数
    pub skipped: usize,
    /// 源目录中缺失数
    pub missing: usize,
    pub warnings: Vec<String>,
}

/// 按清单 `attachment` 表把源附件拷入库内落点（§5 前提：库自包含，删源后仍可用）。
/// - **读侧**按源布局：`source_dir/{file_path}`（`file_path` 是源相对路径，Q15）；
/// - **写侧**按库内保留区：`library_dir/{disk_rel_path(file_path)}`，即 Tier 1–3 落
///   `_attachments/…`、Tier 4 落 `_unlinked_attachments/…`（A1'：不得落库根顶层
///   `attachments/`，否则与笔记目录混淆、且 zip 附件会被自检误报孤儿）；
/// - 落点与 `sync_down` 完全一致（两边同用 [`manifest::disk_rel_path`]），
///   云端键推导口径不变；
/// - 已存在且尺寸一致 → 跳过（幂等重跑零 I/O）；
/// - tier=0（db-missing，无文件）与不安全路径跳过；
/// - **只读源、只写库**（G1 不破）。
pub fn import_attachments(
    library_dir: &Path,
    source_dir: &Path,
    conn: &Connection,
) -> Result<AttachmentImportReport, String> {
    let mut rep = AttachmentImportReport {
        copied: 0,
        skipped: 0,
        missing: 0,
        warnings: Vec::new(),
    };
    for a in manifest::load_attachments(conn)? {
        if a.tier == 0 || a.file_path.starts_with("db-missing:") {
            continue; // 仅元数据行，无文件可拷
        }
        if is_unsafe_rel_path(&a.file_path) {
            if rep.warnings.len() < 10 {
                rep.warnings.push(format!("附件路径不安全，跳过: {}", a.file_path));
            }
            continue;
        }
        let src = source_dir.join(&a.file_path);
        let rel_disk = manifest::disk_rel_path(&a.file_path);
        let dest = library_dir.join(&rel_disk);
        if let Ok(m) = std::fs::metadata(&dest) {
            if m.is_file() && (a.size <= 0 || m.len() == a.size as u64) {
                rep.skipped += 1;
                continue;
            }
        }
        if !src.is_file() {
            rep.missing += 1;
            if rep.warnings.len() < 10 {
                rep.warnings.push(format!("源附件缺失: {}", a.file_path));
            }
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // 临时文件 + rename：库内不留半截文件（与 §4.2 写路径三律同构）
        let tmp = dest.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::copy(&src, &tmp).map_err(|e| format!("拷贝附件失败 {}: {e}", a.file_path))?;
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        rep.copied += 1;
    }
    Ok(rep)
}

// ---------------------------------------------------------------- 写路径（P2：原子 zip 重写 + 清单事务 + 锁）
//
// 设计依据 §4.2（写入原子性）、§4.3（写入能力清单）、§4.4（写必持锁）、
// §4.5（一次写 = 四个动作：写 zip → 清单事务 → 缓存失效 → 单篇索引增量）、
// §13.2 写路径三律、§0.1 D0（库必须无损）。
//
// 调用方（命令层/CLI）**必须**在写完成后让 ZipService/解析器缓存失效：
// 写入用 rename 换了 inode，LRU 里缓存的旧 File 句柄仍指向旧内容（见
// `commands::AppState::invalidate_library_cache`）。

/// 写操作类型标识（报告 / 日志）
pub const OP_SAVE_HTML: &str = "save-html";
/// md 包的正文写（§20.3/M3）：与 `save-html` 同一条原子写路径，只是目标条目为 `note.md`
pub const OP_SAVE_MD: &str = "save-md";
pub const OP_RENAME: &str = "rename";
pub const OP_MOVE: &str = "move";
pub const OP_DELETE: &str = "delete";
/// T7：从回收站恢复（与 delete 互逆，报告 op 分列以便日志/UI 区分）
pub const OP_RESTORE: &str = "restore";
/// 库内新建笔记（origin=local，v5 预留位落地）
pub const OP_CREATE: &str = "create";

/// 一次写操作的结果（T5：命令层/CLI 报告体）
#[derive(Debug, Clone, serde::Serialize)]
pub struct NoteWriteReport {
    /// `save-html` / `rename` / `move` / `delete` / `restore`
    pub op: String,
    pub guid: String,
    /// 写后的标题
    pub title: String,
    /// 写后的库内相对路径（delete 时为删除前的路径）
    pub exported_path: String,
    pub exported_size: i64,
    pub exported_md5: String,
    pub data_modified: String,
    /// 行级 revision（写后；delete 为 0 —— 行已移入墓碑）
    pub revision: i64,
    /// 写后的**当前**清单版本 `meta.revision`。
    ///
    /// **v6 起它不是"写后的新版本号"**（`docs/云同步逻辑.md` §4.1/§8 作废清单第 2 条）：
    /// 版本号只在**云同步上行提交点**铸造，库内写路径**不铸版** ⇒ 本字段等于"写之前的值"，
    /// 只在"库从未同步过"时为 0。保留它是为了报告/UI 能显示库的当前版本，**不是**增量证据。
    pub manifest_revision: u64,
    /// 派生索引是否已同步更新（false = 索引缺失或写入失败，需重建；不视为写失败）
    pub index_updated: bool,
    pub warnings: Vec<String>,
}

// ---- 锁（T3/§4.4） ----

/// 写锁令牌：与云同步**共用同一把锁**（`sync::try_acquire` = 进程内 AtomicBool + `.sync.lock`
/// 文件锁），因此"写到一半被上行"在结构上不可能发生。Drop 即释放。
struct WriteLock(#[allow(dead_code)] crate::sync::SyncGuard);

/// 取写锁。正忙 → `LOCK_BUSY`（明确报错，**不排队静默等待**：UI 才能给出确定反馈，T8）。
fn acquire_write_lock(library_dir: &Path) -> Result<WriteLock, String> {
    match crate::sync::try_acquire(library_dir)? {
        Some(g) => Ok(WriteLock(g)),
        None => Err("LOCK_BUSY: 已有同步或写操作在执行，请稍后重试".into()),
    }
}

// ---- 清单连接与行读取 ----

/// 写路径专用打开：清单**必须已存在**（写操作不负责建库；空目录建库走导入流程 §6.2）
fn open_manifest_rw(library_dir: &Path) -> Result<Connection, String> {
    let p = manifest::manifest_path(library_dir);
    if !p.is_file() {
        return Err(format!("LIB_NO_MANIFEST: 库清单不存在 {}", p.display()));
    }
    manifest::open_and_migrate(library_dir)
}

/// 清单行（写路径需要的字段集）。
///
/// **v4 起是"完整行"**：`delete_note` 要把整行序列化进墓碑（`deleted.note_json`），
/// 回收站「恢复」才能逐字段还原（D0：库必须无损）。少取一个字段，
/// 恢复出来的行就与删除前不等值（`created` / `data_modified` 直接丢）。
#[derive(Debug, Clone)]
struct NoteRow {
    guid: String,
    title: String,
    location: String,
    created: String,
    data_modified: String,
    url: Option<String>,
    doc_type: Option<String>,
    has_attachment: bool,
    package_size: i64,
    exported_path: String,
    exported_size: i64,
    exported_md5: String,
    export_mode: String,
    exported_at: String,
    revision: i64,
    /// v5（§3.3）：来源标注（wiznote / local）
    origin: String,
    /// v5（§3.3）：正文表示（html / markdown）
    content_format: String,
}

fn norm_guid(guid: &str) -> Result<String, String> {
    let braced = normalize_guid(guid).ok_or_else(|| format!("BAD_GUID: {guid}"))?;
    Ok(braced.trim_matches(['{', '}']).to_string())
}

const NOTE_ROW_COLS: &str = "guid, title, location, created, data_modified, url, doc_type, \
     has_attachment, package_size, exported_path, exported_size, exported_md5, export_mode, \
     exported_at, revision, origin, content_format";

fn note_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    Ok(NoteRow {
        guid: r.get(0)?,
        title: r.get(1)?,
        location: r.get(2)?,
        created: r.get(3)?,
        data_modified: r.get(4)?,
        url: r.get(5)?,
        doc_type: r.get(6)?,
        has_attachment: r.get::<_, i64>(7)? != 0,
        package_size: r.get(8)?,
        exported_path: r.get(9)?,
        exported_size: r.get(10)?,
        exported_md5: r.get(11)?,
        export_mode: r.get(12)?,
        exported_at: r.get(13)?,
        revision: r.get(14)?,
        origin: r.get(15)?,
        content_format: r.get(16)?,
    })
}

fn load_note_row(conn: &Connection, guid: &str) -> Result<NoteRow, String> {
    conn.query_row(
        &format!("SELECT {NOTE_ROW_COLS} FROM note WHERE guid = ?1"),
        [guid],
        note_row_from,
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("NOTE_NOT_FOUND: 库内无此篇 {guid}"),
        other => other.to_string(),
    })
}

// ---- T2：一次写 = 一次清单事务 ----

/// 清单行的一次性写回载荷（T2/R13：禁止调用方各自零散 UPDATE）
struct NoteRowPatch {
    title: String,
    location: String,
    exported_path: String,
    exported_size: i64,
    exported_md5: String,
    /// 内容落库时间；rename/move 不改内容 → 保持原值（它就是"内容写入时间"）
    exported_at: String,
}

struct CommitResult {
    row_revision: i64,
    /// **当前**清单版本（写路径不铸版 ⇒ 与写前同值，见 [`NoteWriteReport::manifest_revision`]）
    manifest_revision: u64,
    data_modified: String,
}

/// 单事务写回：行内字段 + 行级 `revision+1` + `data_modified` + **脏闩**。
/// 三者必须同生共死（§13.6：写入后 revision 与 exported_md5 必须同步刷新，
/// 否则云同步把旧内容当新内容或反之）。
///
/// **v6 起这里不再铸版**（`docs/云同步逻辑.md` §8 作废清单第 2 条）：清单 `meta.revision`
/// 只在云同步**上行提交点**推进。库内写路径只置**脏闩** —— 这正是 D 轮两处真 bug 的修复：
/// 当时"本地每写一次就推高计数器"，于是"本地改过一版"会把本地 rev 顶到与远端持平，
/// "远端 rev ≥ 本地 rev"这类判据对它恒为假。
///
/// 闩由**字段实差**推出，而不是让每个调用方自己声明"我改了哪一段"：声明的写法漏标一次
/// 就静默漏传，实差的写法不会。两段分开的收益是**改名/搬家零对象上传**
/// （只动 `dirty_info` ⇒ 只重传清单，不重传 zip）。
fn commit_note_write(conn: &Connection, guid: &str, p: &NoteRowPatch) -> Result<CommitResult, String> {
    let data_modified = manifest::now_local_str();
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let (old_md5, old_title, old_loc, old_path): (String, String, String, String) = tx
        .query_row(
            "SELECT exported_md5, title, location, exported_path FROM note WHERE guid = ?1",
            [guid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| format!("NOTE_NOT_FOUND: {guid}（写清单时行已消失）: {e}"))?;
    let data_changed = old_md5 != p.exported_md5;
    let info_changed =
        old_title != p.title || old_loc != p.location || old_path != p.exported_path;
    let n = tx
        .execute(
            "UPDATE note SET title=?2, location=?3, exported_path=?4, exported_size=?5,
                             exported_md5=?6, data_modified=?7, exported_at=?8,
                             revision = revision + 1
             WHERE guid=?1",
            rusqlite::params![
                guid,
                p.title,
                p.location,
                p.exported_path,
                p.exported_size,
                p.exported_md5,
                data_modified,
                p.exported_at,
            ],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("NOTE_NOT_FOUND: {guid}（写清单时行已消失）"));
    }
    manifest::mark_note_dirty(&tx, guid, info_changed, data_changed)?;
    let row_revision = manifest::load_note_revision(&tx, guid)?.unwrap_or(0);
    // v6：清单版本**原样返回**（不递增）。留着它是为了让报告/UI 仍能显示"库当前处于哪一版"，
    // 但它不再是"这次写产生的新版本"。
    let manifest_revision = manifest::current_revision(&tx);
    tx.commit().map_err(|e| e.to_string())?;
    Ok(CommitResult {
        row_revision,
        manifest_revision,
        data_modified,
    })
}

/// 单事务删除：写墓碑（同步用，§7.4）+ 删 note 行。
///
/// **v4 起墓碑同时是"恢复载荷"**：`note_json` 存删除前的完整清单行，
/// `trash_rel` 存该篇在 `_trash/` 下的实际相对路径（同日同名会加 `_{guid8}` 后缀，
/// 只靠 `last_path` 的 basename 反查会找不到）。两者都是**只增不减**的信息，
/// 旧墓碑（源时代 / v3）该两列为 NULL，恢复时按"无可恢复载荷"如实报错。
///
/// **v6 起两处口径变化**（`docs/云同步逻辑.md` §4.5）：
/// ① 不再铸版（本地写路径一律不铸版，见 [`commit_note_write`]）；
/// ② 墓碑的 `synced_revision` **重置为 0** = "这条墓碑还没发布过"，它就是删除维度的脏闩
///    （note 行已删，闩无处可挂，只能挂在墓碑上）。
fn commit_note_delete(
    conn: &Connection,
    row: &NoteRow,
    removed_at: &str,
    trash_rel: &str,
) -> Result<u64, String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO deleted(guid, last_path, removed_at, title, size, exported_md5, note_json, trash_rel, synced_revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0)
         ON CONFLICT(guid) DO UPDATE SET
           last_path=?2, removed_at=?3, title=?4, size=?5, exported_md5=?6,
           note_json=?7, trash_rel=?8,
           synced_revision=0",
        rusqlite::params![
            row.guid,
            row.exported_path,
            removed_at,
            row.title,
            row.exported_size,
            row.exported_md5,
            snapshot_of(row),
            trash_rel,
        ],
    )
    .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM note WHERE guid = ?1", [&row.guid])
        .map_err(|e| e.to_string())?;
    // v6：不铸版（清单版本只在云同步上行提交点推进）
    let rev = manifest::current_revision(&tx);
    tx.commit().map_err(|e| e.to_string())?;
    Ok(rev)
}

/// 墓碑里的「恢复载荷」：删除前清单一行的完整快照（JSON）。
/// 字段名与 `note` 表列名一致，恢复时直接按名回填 —— 不用另一套命名，
/// 免得将来加列时两边对不上。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct NoteSnapshot {
    title: String,
    location: String,
    created: String,
    data_modified: String,
    url: Option<String>,
    doc_type: Option<String>,
    has_attachment: bool,
    package_size: i64,
    exported_path: String,
    exported_size: i64,
    exported_md5: String,
    export_mode: String,
    exported_at: String,
    revision: i64,
    /// v5：老载荷（v4 及更早写的墓碑）没有这两键 → 按 v4 的隐含默认取值，
    /// 而不是让整个载荷反序列化失败（`serde(default)` 的取值必须显式给出，
    /// 空串不是合法的 origin/content_format）
    #[serde(default = "default_origin")]
    origin: String,
    #[serde(default = "default_content_format")]
    content_format: String,
}

fn default_origin() -> String {
    manifest::ORIGIN_WIZNOTE.to_string()
}
fn default_content_format() -> String {
    manifest::FORMAT_HTML.to_string()
}

fn snapshot_of(row: &NoteRow) -> String {
    let s = NoteSnapshot {
        title: row.title.clone(),
        location: row.location.clone(),
        created: row.created.clone(),
        data_modified: row.data_modified.clone(),
        url: row.url.clone(),
        doc_type: row.doc_type.clone(),
        has_attachment: row.has_attachment,
        package_size: row.package_size,
        exported_path: row.exported_path.clone(),
        exported_size: row.exported_size,
        exported_md5: row.exported_md5.clone(),
        export_mode: row.export_mode.clone(),
        exported_at: row.exported_at.clone(),
        revision: row.revision,
        origin: row.origin.clone(),
        content_format: row.content_format.clone(),
    };
    // 序列化不可能失败（无 map 键、无浮点 NaN）；真失败也不该让删除失败 → 退回空串由恢复侧报错
    serde_json::to_string(&s).unwrap_or_default()
}

// ---- T1：zip 原子重写器 ----

/// 同目录临时文件名（确定性，不用 pid：同篇重写时自动清掉上次崩溃的残留）
fn tmp_path_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

/// **zip 原子重写器**（T1，§4.2 / R11）。
///
/// 语义：只替换 `replacements` 里列出的条目，**其余条目整包搬运**。
/// - 搬运走 [`zip::ZipWriter::raw_copy_file`]：不解压、不重压 → 压缩方式、条目顺序、
///   压缩后的字节原样保留（`index_files/` / `attachments/` 一个都不会少，R11 硬约束）；
/// - 待替换条目必须**原本就存在**（`replacements` 命中数不等即失败），不会静默新建条目；
/// - 目标文件**绝不原地截断写**：先写同目录 `{name}.zip.tmp` → flush → `sync_all`（fsync）
///   → 同目录 `rename` 原子覆盖；失败路径删 tmp，**原 zip 始终完好**。
///
/// 返回重写后的字节数。
pub fn rewrite_note_zip(path: &Path, replacements: &[(String, Vec<u8>)]) -> Result<u64, String> {
    rewrite_note_zip_with(path, replacements, false)
}

/// `abort_before_rename` 仅测试用（T9 崩溃注入）：模拟"tmp 已写完、rename 之前进程死掉"，
/// 此时 tmp 留在磁盘上（真实 abort 不会清理）、原 zip 未被触碰。
fn rewrite_note_zip_with(
    path: &Path,
    replacements: &[(String, Vec<u8>)],
    abort_before_rename: bool,
) -> Result<u64, String> {
    if !path.is_file() {
        return Err(format!("NOTE_PACKAGE_MISSING: {}", path.display()));
    }
    let tmp = tmp_path_of(path);
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> Result<u64, String> {
        let src = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut ar = zip::ZipArchive::new(src).map_err(|e| format!("打开 zip 失败: {e}"))?;
        let out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut zw = zip::ZipWriter::new(out);
        let mut replaced = 0usize;
        for i in 0..ar.len() {
            let entry = ar.by_index(i).map_err(|e| format!("读 zip 条目 {i} 失败: {e}"))?;
            let name = entry.name().to_string();
            match replacements.iter().find(|(n, _)| n == &name) {
                Some((_, bytes)) => {
                    let opt = zip::write::SimpleFileOptions::default()
                        .compression_method(entry.compression());
                    zw.start_file(name.clone(), opt).map_err(|e| e.to_string())?;
                    zw.write_all(bytes).map_err(|e| e.to_string())?;
                    replaced += 1;
                }
                None => {
                    zw.raw_copy_file(entry)
                        .map_err(|e| format!("搬运条目 {name} 失败: {e}"))?;
                }
            }
        }
        if replaced != replacements.len() {
            return Err(format!(
                "ENTRY_NOT_FOUND: zip 内缺少待替换条目（命中 {replaced}/{}）",
                replacements.len()
            ));
        }
        let mut out = zw.finish().map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        out.sync_all().map_err(|e| e.to_string())?;
        drop(out);
        // Windows 上必须先释放源句柄才能 rename 覆盖
        drop(ar);
        let new_size = std::fs::metadata(&tmp).map_err(|e| e.to_string())?.len();
        if abort_before_rename {
            return Err("ABORT_BEFORE_RENAME: 崩溃注入（原 zip 未动）".into());
        }
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(new_size)
    })();
    if result.is_err() && !abort_before_rename {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 读 zip 内某条目的原始字节（不存在 → `None`）
fn read_zip_entry(path: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    // 注意：here 必须先把结果落到局部变量，否则 ZipFile 借用会活过 `ar` 的作用域
    let out = match ar.by_name(name) {
        Ok(mut e) => {
            let mut b = Vec::new();
            e.read_to_end(&mut b).map_err(|e| e.to_string())?;
            Ok(Some(b))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(e.to_string()),
    };
    out
}

// ---- 落盘前校验（R12）与路径预排 ----

/// 保存前的正文校验（R12）：**不通过就拒绝保存，绝不写坏库**。
///
/// 与 §15.7 R12 的差别（有意为之）：R12 建议直接复用 `verify::html_purity_check`，
/// 但该检查把 `<script>` 也算违规 —— 那是**导出物**的口径；库内正文里
/// 实测约 1% 的真实笔记含 `<script>`（合法内容），照搬会把合法保存判死。
/// 故此处只把"必然有害"的两条设为硬拒，其余降格为警告：
/// - 硬拒 `EMPTY_HTML`：正文全空白（防手滑清空笔记）；
/// - 硬拒 `HTML_HOST_REF`：含 `wiznote://` 宿主协议引用 —— 那是本软件的注入痕迹，
///   留在库内会让「导出 / 巡检」的纯度检查永远失败；
/// - 警告 `extract_text` 抽不出文本（纯图片/附件笔记合法）、`<script>` 数量。
fn validate_note_html(html: &str) -> Result<Vec<String>, String> {
    if html.trim().is_empty() {
        return Err("EMPTY_HTML: 正文为空，已拒绝保存（避免清空笔记正文）".into());
    }
    if html.contains("wiznote://") {
        return Err(
            "HTML_HOST_REF: 正文含 wiznote:// 宿主引用（本软件注入痕迹），会污染导出与巡检，请删除后再保存"
                .into(),
        );
    }
    let mut w = Vec::new();
    let scripts = regex::Regex::new(r"(?i)<script")
        .map(|re| re.find_iter(html).count())
        .unwrap_or(0);
    if scripts > 0 {
        w.push(format!("正文含 <script> {scripts} 处（合法内容，将随导出/巡检一并保留）"));
    }
    if crate::extract::extract_text(html).trim().is_empty() {
        w.push("正文抽不出文本（可能是纯图片/附件笔记）".into());
    }
    Ok(w)
}

/// 保存 md 正文前的校验（M3/§20.3）：与 [`validate_note_html`] **同口径、换检材** ——
/// 两条硬拒（空正文 / 宿主协议引用）的判据与后果都一致，只是检的是 Markdown 文本。
///
/// 「抽不出文本」这一档改成先 [`crate::md::md_to_html`] 再抽 —— md 的可见文本要经过
/// 渲染才成立（`# 标题` 里没有 HTML 标签，直接对 md 跑 HTML 抽取也会得到文本，
/// 但对 `> 引用` / 表格这类前缀语法会更准；且这正是阅读态看到的文本）。
fn validate_note_md(md: &str) -> Result<Vec<String>, String> {
    if md.trim().is_empty() {
        return Err("EMPTY_MD: 正文为空，已拒绝保存（避免清空笔记正文）".into());
    }
    if md.contains("wiznote://") {
        return Err(
            "MD_HOST_REF: 正文含 wiznote:// 宿主引用（本软件注入痕迹），会污染导出与巡检，请删除后再保存"
                .into(),
        );
    }
    let mut w = Vec::new();
    // md 里的原始 HTML 会**原样穿过** pulldown-cmark 进入阅读态（CSP 仍会挡脚本），
    // 故与 HTML 口径一样只警告、不拒。
    let scripts = regex::Regex::new(r"(?i)<script")
        .map(|re| re.find_iter(md).count())
        .unwrap_or(0);
    if scripts > 0 {
        w.push(format!("正文含 <script> {scripts} 处（合法内容，将随导出/巡检一并保留）"));
    }
    if crate::extract::extract_text(&crate::md::md_to_html(md))
        .trim()
        .is_empty()
    {
        w.push("正文抽不出文本（可能是纯图片/附件笔记）".into());
    }
    Ok(w)
}

fn norm_title(title: &str) -> Result<String, String> {
    let t = title.trim();
    if t.is_empty() {
        return Err("EMPTY_TITLE: 标题不能为空".into());
    }
    Ok(t.to_string())
}

/// location 归一为为知形态 `/{a}/{b}/`（库根为 `/`），并拒绝 `.` / `..` 路径段
fn norm_location(location: &str) -> Result<String, String> {
    let t = location.trim().trim_matches('/');
    if t.is_empty() {
        return Ok("/".into());
    }
    for seg in t.split('/') {
        if seg == "." || seg == ".." {
            return Err(format!("PATH_UNSAFE: location 含非法路径段 `{seg}`"));
        }
    }
    Ok(format!("/{t}/"))
}

/// 目标落地路径预排（复用导出侧命名规则，§3.1）：
/// 目录 = location 逐段净化；文件名 = `净化标题.zip`，同目录撞名由
/// [`crate::extract::export_name`] 追加 guid 前 8 位。
fn plan_note_path(conn: &Connection, guid: &str, title: &str, location: &str) -> Result<String, String> {
    let dir_rel = crate::extract::sanitize_location(location)
        .trim_matches('/')
        .to_string();
    let mut st = conn
        .prepare("SELECT guid, exported_path FROM note")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String)> = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    drop(st);
    // 同目录其它篇目已占用的名字：base 与 zip 两种形态都占（与 export::plan_zip_paths 同口径，
    // 避免标题「a」与「a.zip」两篇同盘撞名互相覆盖）
    let mut used: HashSet<String> = HashSet::new();
    for (g, p) in &rows {
        if g == guid {
            continue;
        }
        let pb = Path::new(p);
        let pdir = pb
            .parent()
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if pdir != dir_rel {
            continue;
        }
        if let Some(fname) = pb.file_name().map(|f| f.to_string_lossy().to_string()) {
            if let Some(base) = fname.strip_suffix(".zip") {
                used.insert(base.to_string());
            }
            used.insert(fname);
        }
    }
    let base = crate::extract::export_name(title, guid, &mut used);
    let zip_name = format!("{base}.zip");
    Ok(if dir_rel.is_empty() {
        zip_name
    } else {
        format!("{dir_rel}/{zip_name}")
    })
}

fn file_mtime_utc(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(manifest::format_utc)
        .unwrap_or_else(|_| manifest::format_utc(SystemTime::now()))
}

/// 移动/删除后清理空目录（best-effort，失败即停：非空目录不会被动到）
pub(crate) fn prune_empty_dirs(from: Option<&Path>, stop: &Path) {
    let mut cur = from.map(|p| p.to_path_buf());
    while let Some(d) = cur {
        if d == *stop || !d.starts_with(stop) || !d.is_dir() {
            break;
        }
        if std::fs::remove_dir(&d).is_err() {
            break;
        }
        cur = d.parent().map(|p| p.to_path_buf());
    }
}

/// 回收站落点（**唯一定名口径**）：`{库根}/_trash/{今天}/{原名}.zip`；
/// 同名已存在则追加 `_{guid8}` → `_{guid8}_{i}`，**绝不覆盖**回收站已有文件。
///
/// 为什么抽成函数、且必须是 `pub`：`delete_note`（库内删除）与 `sync_down`（远端删了这篇）
/// 往**同一个**回收站里放东西。定名各写一套的话，两边的"同名保护"互不知情 ——
/// 同一天先删一篇、再同步删一篇同名文件，第二篇会直接盖掉第一篇。
///
/// 返回 `(落点, 是否因重名改过名)`：删除路径要把改名这件事写进 warnings（否则用户按原文件名
/// 找不到），同步路径不关心。
///
/// 已知缺口（P3 补）：改过名的条目，「恢复」按文件名回找时**找不到**（`restore_note` 靠
/// basename/`{stem}_` 前缀猜），需要条目级恢复（墓碑记的 `trash_rel` 是精确的，见
/// `delete_note`——它把实际落点写进墓碑；只有同步移入的这批没有墓碑行）。
pub fn trash_dest(
    library_dir: &Path,
    exported_path: &str,
    guid: &str,
) -> Result<(PathBuf, bool), String> {
    let fname = Path::new(exported_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("{guid}.zip"));
    let date_dir = library_dir.join("_trash").join(crate::sync::today_utc());
    std::fs::create_dir_all(&date_dir).map_err(|e| e.to_string())?;
    let mut dest = date_dir.join(&fname);
    let mut renamed = false;
    if dest.exists() {
        // 同日同名（不同 guid 净化后同名）→ 追加 guid 前 8 位
        let stem = fname.strip_suffix(".zip").unwrap_or(&fname).to_string();
        let g8: String = guid.chars().take(8).collect();
        let mut i = 1;
        loop {
            let cand = if i == 1 {
                format!("{stem}_{g8}.zip")
            } else {
                format!("{stem}_{g8}_{i}.zip")
            };
            dest = date_dir.join(&cand);
            if !dest.exists() {
                break;
            }
            i += 1;
        }
        renamed = true;
    }
    Ok((dest, renamed))
}

/// 写后单篇索引增量（T4/§4.5 第 4 环）。
/// 自建一个只读清单解析器（读的是刚提交的新路径），失败**不改变写结果**，
/// 只回 `index_updated=false` + 警告，由启动自检/「重建索引」兜底。
fn refresh_one_index(library_dir: &Path, index_db: &Path, guid: &str) -> (bool, Vec<String>) {
    if !index_db.is_file() {
        return (false, vec![format!("派生索引不存在（{}），请重建索引", index_db.display())]);
    }
    let resolver = match LibraryResolver::new(library_dir.to_path_buf()) {
        Ok(r) => std::sync::Arc::new(r),
        Err(e) => return (false, vec![format!("索引增量跳过（解析器构建失败）: {e}")]),
    };
    match crate::indexer::update_library_note_index(library_dir, resolver, index_db, guid) {
        Ok(w) => (true, w),
        Err(e) => (false, vec![format!("索引增量失败（需重建索引）: {e}")]),
    }
}

// ---- 四个写操作（§4.3） ----

/// 包内正文形态（**写路径专用**）：直接看 zip 条目，**不经 [`crate::zipserve::ZipService`]**
/// —— 写路径必须看到磁盘实况，不能受 LRU 缓存（写前打开过的旧句柄）影响。
///
/// 与读侧 [`crate::zipserve::BodyFormat`] 同口径（都只看"有没有 `note.md`"），
/// 保证「读到的形态」与「写回的形态」必然是同一个。
fn package_body_format(zip_path: &Path) -> Result<crate::zipserve::BodyFormat, String> {
    let f = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(f).map_err(|e| format!("打开 zip 失败: {e}"))?;
    Ok(if ar.by_name(crate::md::NOTE_MD).is_ok() {
        crate::zipserve::BodyFormat::Md
    } else {
        crate::zipserve::BodyFormat::Html
    })
}

/// 编辑正文的**公共实现**（T2/§4.3/§4.5）：校验 → 原子替换包内正文条目 → 清单事务 →
/// 单篇索引增量。`save_note_html` / `save_note_md` / `save_note_body` 三者的差别
/// **只有正文条目名与校验口径**，其余（整包搬运、`.tmp`+fsync+rename、BOM 形态保持、
/// 一次写 = 一次清单事务、索引增量）必须逐字节一致 —— 故只有这一份实现。
///
/// `forced = None` 表示**按包内实况自动分派**（[`save_note_body`]）；
/// `Some(fmt)` 为强制形态（CLI `--md` / `--html`，或 `save_note_html`/`save_note_md`）。
fn write_note_body(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
    forced: Option<crate::zipserve::BodyFormat>,
    text: &str,
) -> Result<NoteWriteReport, String> {
    use crate::zipserve::BodyFormat;
    // 输入容忍：CLI/编辑器可能把 BOM 当正文首字符带进来（读路径本就剥 BOM）→ 先剔一遍，
    // 否则会被再补一次 BOM 变成 `BOM BOM`。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let guid = norm_guid(guid)?;
    let _guard = acquire_write_lock(library_dir)?;
    let conn = open_manifest_rw(library_dir)?;
    let row = load_note_row(&conn, &guid)?;
    if is_unsafe_rel_path(&row.exported_path) {
        return Err(format!("PATH_UNSAFE: {}", row.exported_path));
    }
    let dest = library_dir.join(&row.exported_path);
    if !dest.is_file() {
        return Err(format!("NOTE_PACKAGE_MISSING: 库内缺文件 {}", row.exported_path));
    }
    let fmt = match forced {
        Some(f) => f,
        None => package_body_format(&dest)?,
    };
    let (entry, op, validate): (&str, &str, fn(&str) -> Result<Vec<String>, String>) = match fmt {
        BodyFormat::Md => (crate::md::NOTE_MD, OP_SAVE_MD, validate_note_md),
        BodyFormat::Html => ("index.html", OP_SAVE_HTML, validate_note_html),
    };
    let mut warnings = validate(text)?;

    // 编码形态与原条目一致：为知 index.html 实测 100% 带 UTF-8 BOM、读路径剥 BOM（G3），
    // 写回必须补回，否则"打开 → 原样保存"会静默改变文件编码形态。
    // md 包的 `note.md` 由 [`crate::export::write_md_package`] 写成 **UTF-8 无 BOM**，
    // 故走同一条"保持原状"逻辑即得规范形态（不需要按形态分叉）。
    let bytes = match read_zip_entry(&dest, entry)? {
        None => {
            return Err(format!(
                "ENTRY_NOT_FOUND: 该篇 zip 内无 {entry}（写路径只改已有正文，不新建条目）"
            ))
        }
        Some(orig) => {
            let bom = orig.starts_with(&[0xEF, 0xBB, 0xBF]);
            let mut v = Vec::with_capacity(orig.len() + text.len());
            if bom {
                v.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            v.extend_from_slice(text.as_bytes());
            v
        }
    };

    // ① 落盘（原子 rename）→ ② 清单事务 → ③ 缓存失效（调用方）→ ④ 单篇索引
    rewrite_note_zip(&dest, &[(entry.to_string(), bytes)])?;
    let size = std::fs::metadata(&dest).map_err(|e| e.to_string())?.len() as i64;
    let md5 = manifest::md5_file(&dest)?;
    let patch = NoteRowPatch {
        title: row.title.clone(),
        location: row.location.clone(),
        exported_path: row.exported_path.clone(),
        exported_size: size,
        exported_md5: md5.clone(),
        // 内容刚写入 → exported_at 取落地文件 mtime（与导出/重建同源，保证重建逐字段一致）
        exported_at: file_mtime_utc(&dest),
    };
    let c = commit_note_write(&conn, &guid, &patch)?;
    warnings.extend(manifest::check_invariants(&conn, library_dir)?);
    let (index_updated, iw) = refresh_one_index(library_dir, index_db, &guid);
    warnings.extend(iw);

    Ok(NoteWriteReport {
        op: op.into(),
        guid,
        title: patch.title,
        exported_path: patch.exported_path,
        exported_size: size,
        exported_md5: md5,
        data_modified: c.data_modified,
        revision: c.row_revision,
        manifest_revision: c.manifest_revision,
        index_updated,
        warnings,
    })
}

/// 编辑正文（**按包内形态自动分派**，§4.3/§20.8/M3）：
/// md 包写 `note.md`（Markdown），为知原生包写 `index.html`（HTML）。
///
/// UI「编辑正文」与 CLI `save-note` 走这条 —— 编辑器拿到的源码形态与包内形态一致
/// （见 [`crate::zipserve::ZipService::read_note_body`]），故**调用方不必知道库是哪一种**；
/// 形态由**包内条目**决定（不是清单 `export_mode`），与读侧同一判据。
pub fn save_note_body(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
    text: &str,
) -> Result<NoteWriteReport, String> {
    write_note_body(library_dir, index_db, guid, None, text)
}

/// 编辑正文（§4.3，为知原生包）：只替换 zip 内 `index.html`，其余条目整包搬运（D0 无损）。
/// `html` 为完整 HTML 文本（读取路径 `read_index_html` 剥 BOM 后的形态，写回时按原编码补回 BOM）。
pub fn save_note_html(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
    html: &str,
) -> Result<NoteWriteReport, String> {
    write_note_body(
        library_dir,
        index_db,
        guid,
        Some(crate::zipserve::BodyFormat::Html),
        html,
    )
}

/// 编辑正文（§20.3/M3，md 包）：只替换 zip 内 `note.md`，其余条目（`index_files/` 等）
/// 整包搬运 —— 附件在包内，故改名/移动/删除都不必额外处理（与 `save_note_html` 同构）。
/// `md` 为 Markdown 文本（读路径 `read_note_body` 剥 BOM 后的形态）。
pub fn save_note_md(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
    md: &str,
) -> Result<NoteWriteReport, String> {
    write_note_body(
        library_dir,
        index_db,
        guid,
        Some(crate::zipserve::BodyFormat::Md),
        md,
    )
}

/// 重命名标题（§4.3/T6）：落地文件名随标题变，`exported_path` 同步更新；
/// **云端键按 guid 规范化（F6）→ 不引发云端 churn**。
pub fn rename_note(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
    new_title: &str,
) -> Result<NoteWriteReport, String> {
    relocate_note(library_dir, index_db, guid, OP_RENAME, Some(new_title), None)
}

/// 移动目录（§4.3/T6）：location 变更 → 目录树下的落地路径变更；同样不影响云端键。
pub fn move_note(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
    new_location: &str,
) -> Result<NoteWriteReport, String> {
    relocate_note(library_dir, index_db, guid, OP_MOVE, None, Some(new_location))
}

/// 重命名/移动的公共实现：只改落地路径与清单元数据，**zip 字节不变**（不重写内容）。
fn relocate_note(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
    op: &str,
    new_title: Option<&str>,
    new_location: Option<&str>,
) -> Result<NoteWriteReport, String> {
    let guid = norm_guid(guid)?;
    let _guard = acquire_write_lock(library_dir)?;
    let conn = open_manifest_rw(library_dir)?;
    let row = load_note_row(&conn, &guid)?;
    if is_unsafe_rel_path(&row.exported_path) {
        return Err(format!("PATH_UNSAFE: {}", row.exported_path));
    }
    let title = match new_title {
        Some(t) => norm_title(t)?,
        None => row.title.clone(),
    };
    let location = match new_location {
        Some(l) => norm_location(l)?,
        None => row.location.clone(),
    };
    let target_rel = plan_note_path(&conn, &guid, &title, &location)?;
    if is_unsafe_rel_path(&target_rel) {
        return Err(format!("PATH_UNSAFE: {target_rel}"));
    }
    let old_abs = library_dir.join(&row.exported_path);
    if !old_abs.is_file() {
        return Err(format!("NOTE_PACKAGE_MISSING: 库内缺文件 {}", row.exported_path));
    }
    let new_abs = library_dir.join(&target_rel);
    let mut warnings = Vec::new();
    if target_rel != row.exported_path {
        if new_abs.exists() {
            return Err(format!("PATH_TAKEN: 目标路径已存在 {target_rel}"));
        }
        if let Some(p) = new_abs.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        // 文件搬家（同盘 rename 原子；跨盘由 OS 拒绝，不静默降级为拷贝+删除）
        std::fs::rename(&old_abs, &new_abs).map_err(|e| format!("移动文件失败: {e}"))?;
        prune_empty_dirs(old_abs.parent(), library_dir);
    } else {
        warnings.push("落地路径未变（标题净化后同名/同目录），仅更新清单元数据".into());
    }
    let size = std::fs::metadata(&new_abs).map_err(|e| e.to_string())?.len() as i64;
    let md5 = manifest::md5_file(&new_abs)?;
    let patch = NoteRowPatch {
        title: title.clone(),
        location: location.clone(),
        exported_path: target_rel.clone(),
        exported_size: size,
        exported_md5: md5.clone(),
        // 内容未变 → exported_at 保持原值（它的语义是"内容落库时间"）
        exported_at: row.exported_at.clone(),
    };
    let c = commit_note_write(&conn, &guid, &patch)?;
    warnings.extend(manifest::check_invariants(&conn, library_dir)?);
    let (index_updated, iw) = refresh_one_index(library_dir, index_db, &guid);
    warnings.extend(iw);

    Ok(NoteWriteReport {
        op: op.into(),
        guid,
        title,
        exported_path: target_rel,
        exported_size: size,
        exported_md5: md5,
        data_modified: c.data_modified,
        revision: c.row_revision,
        manifest_revision: c.manifest_revision,
        index_updated,
        warnings,
    })
}

/// 删除笔记（T7/§4.3/Q8）：文件移入 `_trash/{YYYY-MM-DD}/` + 写 `deleted` 墓碑 + 删清单行。
/// 不走物理删除（保留 30 天 GC 与恢复能力，复用 `sync::gc_trash` 的目录口径）。
pub fn delete_note(library_dir: &Path, index_db: &Path, guid: &str) -> Result<NoteWriteReport, String> {
    let guid = norm_guid(guid)?;
    let _guard = acquire_write_lock(library_dir)?;
    let conn = open_manifest_rw(library_dir)?;
    let row = load_note_row(&conn, &guid)?;
    if is_unsafe_rel_path(&row.exported_path) {
        return Err(format!("PATH_UNSAFE: {}", row.exported_path));
    }
    let src = library_dir.join(&row.exported_path);
    if !src.is_file() {
        return Err(format!("NOTE_PACKAGE_MISSING: 库内缺文件 {}", row.exported_path));
    }
    // 回收站落点与 `sync_down`（远端删除）共用同一定名口径：同名绝不覆盖
    let (dest, renamed) = trash_dest(library_dir, &row.exported_path, &guid)?;
    let mut warnings = Vec::new();
    if renamed {
        warnings.push(format!(
            "回收站已有同名文件，本次存为 {}（恢复需按该文件名）",
            dest.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    std::fs::rename(&src, &dest).map_err(|e| format!("移入回收站失败: {e}"))?;
    prune_empty_dirs(src.parent(), library_dir);

    let removed_at = manifest::format_utc(SystemTime::now());
    // 实际落点（含必要时追加的 `_{guid8}` 后缀）——恢复按此精确回找，不靠 basename 猜
    let trash_rel = dest
        .strip_prefix(library_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            format!(
                "_trash/{}/{}",
                crate::sync::today_utc(),
                dest.file_name().unwrap_or_default().to_string_lossy()
            )
        });
    let manifest_revision = commit_note_delete(&conn, &row, &removed_at, &trash_rel)?;
    warnings.extend(manifest::check_invariants(&conn, library_dir)?);
    let (index_updated, iw) = refresh_one_index(library_dir, index_db, &guid);
    warnings.extend(iw);

    Ok(NoteWriteReport {
        op: OP_DELETE.into(),
        guid,
        title: row.title,
        exported_path: row.exported_path,
        exported_size: row.exported_size,
        exported_md5: row.exported_md5,
        data_modified: manifest::now_local_str(),
        revision: 0,
        manifest_revision,
        index_updated,
        warnings,
    })
}

// ---- T7：回收站（列表 / 逐字段恢复） ----

/// 回收站一项（墓碑 + 磁盘实况）。`restorable` 是 UI 的判据，不由前端自己拼。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrashEntry {
    pub guid: String,
    pub title: String,
    /// 删除前的库内相对路径
    pub last_path: String,
    /// `_trash/` 下的实际相对路径（旧墓碑为 None）
    pub trash_rel: Option<String>,
    pub removed_at: String,
    pub size: i64,
    /// 磁盘上是否找得到可恢复的文件
    pub restorable: bool,
    /// 不可恢复的原因（`restorable=false` 时非空）
    pub reason: String,
    /// 距保留期结束的天数（可能为负 = 已超期，等 GC）
    pub days_left: i64,
    /// 墓碑是否带 v4 恢复载荷（false = 源时代/v3 墓碑，只能恢复文件不能还原行）
    pub has_snapshot: bool,
}

/// 列出回收站（库根 `_trash/` + 墓碑）。
///
/// 与源时代口径的差别：**库模式下列表来自库自己的清单**（`library_dir/export.db` 的
/// `deleted` 表），而不是"扫 `_trash/` 目录看有什么文件"—— 后者在换机/拷贝后
/// 分不清哪些是"被删的笔记"、哪些是别的东西。
///
/// 只读打开清单（`open_readonly`）：列回收站是**看**，不该触发迁移或写任何字节。
///
/// 代价：**不能假定 v4 的两列已存在** —— 迁移只在写路径发生，而"看一眼回收站"不该是写动作。
/// 故按列存在性降级：列缺失（v1–v3 库）→ 等价 `NULL`，即"该墓碑无恢复载荷"，与
/// `has_snapshot=false` 同义；`deleted` 表本身不存在（v1 老库）→ 空列表而非 SQL 报错。
pub fn list_trash(library_dir: &Path) -> Result<Vec<TrashEntry>, String> {
    let conn = manifest::open_readonly(library_dir)?;
    let cols = table_columns(&conn, "deleted")?;
    if cols.is_empty() {
        return Ok(Vec::new()); // 无 deleted 表 ⇒ 没有墓碑
    }
    let legacy = !(cols.iter().any(|c| c == "note_json") && cols.iter().any(|c| c == "trash_rel"));
    let sql = format!(
        "SELECT guid, ifnull(title,''), last_path, removed_at, ifnull(size,0), {}, {}
         FROM deleted ORDER BY removed_at DESC",
        if legacy { "NULL" } else { "note_json" },
        if legacy { "NULL" } else { "trash_rel" }
    );
    let today = crate::sync::today_days();
    let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (guid, title, last_path, removed_at, size, note_json, trash_rel) in rows.flatten() {
        let reason = match locate_trash_file(library_dir, &last_path, trash_rel.as_deref()) {
            Ok(Some(_)) => String::new(),
            Ok(None) => "回收站内已无该文件（可能已超期清理）".to_string(),
            Err(e) => e,
        };
        let days_left = crate::sync::parse_removed_date(&removed_at)
            .map(|d| crate::sync::TRASH_RETENTION_DAYS as i64 - (today - d))
            .unwrap_or(0);
        out.push(TrashEntry {
            guid,
            title,
            last_path,
            trash_rel,
            removed_at,
            size,
            restorable: reason.is_empty(),
            reason,
            days_left,
            has_snapshot: note_json.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false),
        });
    }
    Ok(out)
}

/// 读某张表的列名（表不存在 → 空 Vec，不报错）。
///
/// 专供**只读路径**按列存在性降级：`open_readonly` 不迁移，因此不能假定 v4 新增列
/// （`deleted.note_json` / `deleted.trash_rel`）已在位。表名只由本文件内的字面量传入。
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut st = conn
        .prepare(&format!("PRAGMA table_info('{table}')"))
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 在 `_trash/` 下定位某一篇的可恢复文件。
/// 优先级：① 墓碑记的 `trash_rel`（精确）→ ② 按 `last_path` 的 basename 扫日期目录
/// （兼容 v3 及更早的墓碑，以及同日同名追加 `_{guid8}` 后缀的情形）。
fn locate_trash_file(
    library_dir: &Path,
    last_path: &str,
    trash_rel: Option<&str>,
) -> Result<Option<PathBuf>, String> {
    if let Some(rel) = trash_rel {
        if is_unsafe_rel_path(rel) {
            return Err(format!("PATH_UNSAFE: 墓碑记的回收站路径不安全 {rel}"));
        }
        let p = library_dir.join(rel);
        if p.is_file() {
            return Ok(Some(p));
        }
    }
    let Some(fname) = Path::new(last_path).file_name() else {
        return Ok(None);
    };
    let fname = fname.to_string_lossy().to_string();
    let stem = fname.trim_end_matches(".zip").to_string();
    let trash = library_dir.join("_trash");
    if !trash.is_dir() {
        return Ok(None);
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    for date_dir in std::fs::read_dir(&trash).map_err(|e| e.to_string())?.flatten() {
        if !date_dir.path().is_dir() {
            continue;
        }
        for f in std::fs::read_dir(date_dir.path()).map_err(|e| e.to_string())?.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if name == fname || (name.starts_with(&format!("{stem}_")) && name.ends_with(".zip")) {
                cands.push(f.path());
            }
        }
    }
    // 多个候选（同日多篇同名）→ 取排序最后的一支，不猜内容
    cands.sort();
    Ok(cands.pop())
}

/// **恢复**（T7/§4.3）：把 `_trash/` 里的文件移回删除前的路径，并按墓碑快照
/// **逐字段还原** note 行（v4 载荷；无载荷 → `NO_TOMBSTONE_PAYLOAD` 明确报错，
/// 不做"能恢复多少算多少"的静默降级）。
///
/// 顺序（与 §4.2 的三律一致）：**持锁 → 只读校验 → 落文件 → 清单事务**。
/// 事务失败时 best-effort 把文件挪回回收站，避免"文件已就位但清单无行"的半残状态。
pub fn restore_note(
    library_dir: &Path,
    index_db: &Path,
    guid: &str,
) -> Result<NoteWriteReport, String> {
    let guid = norm_guid(guid)?;
    let _guard = acquire_write_lock(library_dir)?;
    let conn = open_manifest_rw(library_dir)?;

    let (last_path, trash_rel, note_json, tomb_title): (String, Option<String>, Option<String>, String) =
        conn.query_row(
            "SELECT last_path, trash_rel, note_json, ifnull(title,'') FROM deleted WHERE guid = ?1",
            [&guid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                format!("TOMBSTONE_NOT_FOUND: 回收站内无此篇 {guid}")
            }
            other => other.to_string(),
        })?;
    if is_unsafe_rel_path(&last_path) {
        return Err(format!("PATH_UNSAFE: {last_path}"));
    }
    let payload = note_json.unwrap_or_default();
    if payload.trim().is_empty() {
        return Err(
            "NO_TOMBSTONE_PAYLOAD: 该墓碑没有恢复载荷（v3 及更早的墓碑）—— \
             只能恢复文件，无法还原清单行；请先「重建清单」（wiz-cli manifest-rebuild）后重试"
                .into(),
        );
    }
    let snap: NoteSnapshot = serde_json::from_str(&payload)
        .map_err(|e| format!("TOMBSTONE_PAYLOAD_BAD: 墓碑载荷无法解析（{e}）"))?;
    // 行已不在 note 表（恢复的前提）；若在，说明清单与墓碑不一致，宁可报错也不覆盖
    if conn
        .query_row("SELECT 1 FROM note WHERE guid = ?1", [&guid], |_| Ok(()))
        .is_ok()
    {
        return Err(format!("NOTE_ALREADY_EXISTS: 清单里已有该篇 {guid}，不覆盖"));
    }
    let src = locate_trash_file(library_dir, &last_path, trash_rel.as_deref())?
        .ok_or_else(|| format!("TRASH_FILE_MISSING: 回收站内找不到该篇的文件（{last_path}）"))?;
    let dest = library_dir.join(&last_path);
    if dest.exists() {
        return Err(format!("PATH_TAKEN: 目标路径已被占用 {last_path}（先移走再恢复）"));
    }
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let mut warnings = Vec::new();
    std::fs::rename(&src, &dest).map_err(|e| format!("从回收站恢复失败: {e}"))?;
    // 实际字节为准：文件在回收站期间被外部改动过的话，快照里的 size/md5 就是错的
    let size = std::fs::metadata(&dest).map_err(|e| e.to_string())?.len() as i64;
    let md5 = manifest::md5_file(&dest)?;
    if size != snap.exported_size || md5 != snap.exported_md5 {
        warnings.push(format!(
            "恢复的文件与删除前不一致（体积 {}→{}，md5 {}→{}），已按磁盘实况写回清单",
            snap.exported_size, size, snap.exported_md5, md5
        ));
    }

    // 恢复 = **撤销删除**：`data_modified` 还原成删除前的值，而不是恢复时刻。
    // 若写恢复时刻，旧笔记会在"按修改时间排序"的列表里冒到最前面 —— 那是假信息，
    // 与 D0（库必须无损）相悖。`revision` 是例外：恢复本身是一次写，故 +1。
    let data_modified = if snap.data_modified.trim().is_empty() {
        manifest::now_local_str() // 兜底：载荷缺该字段（正常不会发生，载荷是完整行序列化）
    } else {
        snap.data_modified.clone()
    };
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let res = tx
        .execute(
            // v6：恢复出的行**置脏**（`dirty_data=1`）—— 它是"重新进入活表"的内容，
            // 必须能被下一次上行带走（`docs/云同步逻辑.md` §5 末条"恢复即置脏"）。
            // `synced_revision` 留默认 0 = "这一版还没进过云端"，提交点会补上铸造号。
            "INSERT INTO note(guid, title, location, created, data_modified, url, doc_type,
                              has_attachment, package_size, exported_path, exported_size,
                              exported_md5, export_mode, exported_at, revision,
                              origin, content_format, dirty_data)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,1)",
            rusqlite::params![
                guid,
                snap.title,
                snap.location,
                snap.created,
                data_modified,
                snap.url,
                snap.doc_type,
                if snap.has_attachment { 1 } else { 0 },
                snap.package_size,
                last_path,
                size,
                md5,
                snap.export_mode,
                snap.exported_at,
                snap.revision + 1,
                snap.origin,
                snap.content_format,
            ],
        )
        .and_then(|_| {
            tx.execute("DELETE FROM deleted WHERE guid = ?1", [&guid])?;
            Ok(())
        });
    if let Err(e) = res {
        // 事务没提交 → 墓碑还在；把文件挪回回收站，避免"库里有文件、清单里没行"的孤儿
        let back = library_dir.join("_trash").join(crate::sync::today_utc());
        if std::fs::create_dir_all(&back).is_ok() {
            if let Some(f) = dest.file_name() {
                let _ = std::fs::rename(&dest, back.join(f));
            }
        }
        return Err(format!("恢复失败（已回滚文件移动）: {e}"));
    }
    // v6：不铸版（清单版本只在云同步上行提交点推进）
    let manifest_revision = manifest::current_revision(&tx);
    tx.commit().map_err(|e| e.to_string())?;
    warnings.extend(manifest::check_invariants(&conn, library_dir)?);
    let (index_updated, iw) = refresh_one_index(library_dir, index_db, &guid);
    warnings.extend(iw);

    Ok(NoteWriteReport {
        op: OP_RESTORE.into(),
        guid,
        title: if snap.title.is_empty() { tomb_title } else { snap.title },
        exported_path: last_path,
        exported_size: size,
        exported_md5: md5,
        data_modified,
        revision: snap.revision + 1,
        manifest_revision,
        index_updated,
        warnings,
    })
}

/// 新建笔记（**库内新增**，v5 预留的 `origin=local` 终于落地）：
/// 生成 guid → 组装 md 包（单条目 `note.md`，UTF-8 无 BOM，与 [`crate::export::write_md_package`]
/// 产物同形态）→ 清单插行（经 [`manifest::upsert_note`]：**新行按定义置脏**
/// `dirty_info=1, dirty_data=1`，云同步上行才会带走它）→ 单篇索引增量。
///
/// 与四个编辑写操作同守三律：先临时文件后 rename、先落盘后清单（清单失败则回收 zip）、
/// 写必持锁。**不铸版**（`meta.revision` 不动，v6 铁律）；落地路径/同名净化复用
/// [`plan_note_path`]（与导出侧同一套命名口径，同目录撞名自动追加 guid 前 8 位）。
///
/// `md` 为初始正文（Markdown）。空正文按 [`validate_note_md`] 拒绝 —— 前端应传
/// `# 标题` 起手的模板，保证"新建出来即可打开编辑"。
pub fn create_note(
    library_dir: &Path,
    index_db: &Path,
    title: &str,
    location: &str,
    md: &str,
) -> Result<NoteWriteReport, String> {
    let title = norm_title(title)?;
    let location = norm_location(location)?;
    let mut warnings = validate_note_md(md)?;
    let guid = new_note_guid();
    let _guard = acquire_write_lock(library_dir)?;
    let conn = open_manifest_rw(library_dir)?;

    // 新 guid 与活表/墓碑都不得撞（uuid v4 撞概率可忽略，但校验是零成本的）
    if conn
        .query_row("SELECT 1 FROM note WHERE guid = ?1", [&guid], |_| Ok(()))
        .is_ok()
    {
        return Err(format!("NOTE_ALREADY_EXISTS: guid 撞行（几乎不可能）{guid}"));
    }
    let target_rel = plan_note_path(&conn, &guid, &title, &location)?;
    if is_unsafe_rel_path(&target_rel) {
        return Err(format!("PATH_UNSAFE: {target_rel}"));
    }

    // ① 落盘：`note.md` 单条目 md 包（UTF-8 无 BOM；tmp + fsync + rename 原子替换）
    let dest_abs = library_dir.join(&target_rel);
    if let Some(p) = dest_abs.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    write_new_md_package(&dest_abs, md)?;

    // ② 清单插行。upsert 的 INSERT 支带脏闩；失败（如磁盘/约束问题）则回滚 zip，
    //    避免"库里有文件、清单里没行"的孤儿。
    let size = std::fs::metadata(&dest_abs).map_err(|e| e.to_string())?.len() as i64;
    let md5 = manifest::md5_file(&dest_abs)?;
    let exported_at = file_mtime_utc(&dest_abs);
    let now = manifest::now_local_str();
    let row = manifest::ManifestNote {
        guid: guid.clone(),
        title: title.clone(),
        location: location.clone(),
        created: now.clone(),
        data_modified: now,
        url: None,
        doc_type: None,
        has_attachment: false,
        package_size: size,
        exported_path: target_rel.clone(),
        exported_size: size,
        exported_md5: md5.clone(),
        export_mode: crate::export::EXPORT_MODE_MD.into(),
        exported_at,
        origin: manifest::ORIGIN_LOCAL.into(),
        content_format: manifest::FORMAT_MARKDOWN.into(),
    };
    let insert = manifest::upsert_note(&conn, &row);
    if let Err(e) = insert {
        let _ = std::fs::remove_file(&dest_abs);
        prune_empty_dirs(dest_abs.parent(), library_dir);
        return Err(format!("新建失败（已回滚文件落盘）: {e}"));
    }

    manifest::check_invariants(&conn, library_dir)?;
    // v6：只读当前清单版本，**不铸版**（版本号只在云同步上行提交点推进）
    let manifest_revision = manifest::current_revision(&conn);
    let (index_updated, iw) = refresh_one_index(library_dir, index_db, &guid);
    warnings.extend(iw);
    let revision = manifest::load_note_revision(&conn, &guid)?.unwrap_or(0);

    Ok(NoteWriteReport {
        op: OP_CREATE.into(),
        guid,
        title,
        exported_path: target_rel,
        exported_size: size,
        exported_md5: md5,
        data_modified: row.data_modified,
        revision,
        manifest_revision,
        index_updated,
        warnings,
    })
}

/// 生成新笔记 guid：uuid v4 小写连字符形（清单内 guid 统一无花括号，见 [`norm_guid`]）。
fn new_note_guid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 写一个全新的 md 包：单条目 `note.md`（UTF-8 无 BOM，Deflated）。
/// 与 [`crate::export::write_md_package`] 的产物同形态 —— 读侧 [`crate::zipserve::BodyFormat`]
/// 靠"有没有 `note.md`"判定形态，单条目包天然成立；后续编辑走 `save_note_md` 原地替换。
fn write_new_md_package(dest: &Path, md: &str) -> Result<(), String> {
    let tmp = tmp_path_of(dest);
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> Result<(), String> {
        let out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut zw = zip::ZipWriter::new(out);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file(crate::md::NOTE_MD, opts).map_err(|e| e.to_string())?;
        zw.write_all(md.as_bytes()).map_err(|e| e.to_string())?;
        let mut f = zw.finish().map_err(|e| e.to_string())?;
        f.flush().map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 写路径测试的**进程级串行闸门**（`#[cfg(test)]`，**跨模块共用**）。
///
/// 写路径共用 `sync::try_acquire` 的**进程级** AtomicBool：并行跑任意两个"会取写锁"的
/// 测试（库写路径、只读端护栏里的 `delete_note` 等）都会互相判 `LOCK_BUSY`。
/// 凡是调用 `delete_note` / 其他写函数、或 `sync::try_acquire` 的测试，**先持有它**。
///
/// 定义在 `mod tests` **之外**：`sync::tests` 的护栏测试也要取 `delete_note`，
/// 放在测试模块里就跨不过去了（那正是它一开始只保护库里写测试的原因）。
#[cfg(test)]
pub(crate) fn test_write_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wiz-library-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 四态：空目录 / 有清单 ready / 有 zip 无清单 / 非 native 拒绝
    #[test]
    fn test_validate_library_states() {
        // ① 空目录
        let d = temp_dir("empty");
        assert_eq!(validate_library(&d).kind, "empty");

        // ② 有 zip 无清单
        std::fs::write(d.join("某笔记.zip"), b"PK").unwrap();
        assert_eq!(validate_library(&d).kind, "no_manifest");
        std::fs::remove_file(d.join("某笔记.zip")).unwrap();

        // ③ 有清单 → ready（含缺失文件告警）
        let conn = manifest::open_and_migrate(&d).unwrap();
        manifest::set_meta(&conn, "export_mode", "native").unwrap();
        manifest::upsert_note(
            &conn,
            &manifest::ManifestNote {
                guid: "11111111-2222-3333-4444-555555555555".into(),
                title: "笔记一".into(),
                location: "/a/".into(),
                created: "c".into(),
                data_modified: "m".into(),
                url: None,
                doc_type: None,
                has_attachment: false,
                package_size: 1,
                exported_path: "a/笔记一.zip".into(),
                exported_size: 2,
                exported_md5: "x".into(),
                export_mode: "native".into(),
                exported_at: "t".into(),
                origin: crate::manifest::ORIGIN_WIZNOTE.into(),
                content_format: crate::manifest::FORMAT_HTML.into(),
            },
        )
        .unwrap();
        drop(conn);
        let st = validate_library(&d);
        assert_eq!(st.kind, "ready");
        assert_eq!(st.note_count, 1);
        assert_eq!(st.missing_files, 1, "磁盘无该 zip → 缺失计数");

        // 补上文件 → 缺失清零
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("a/笔记一.zip"), b"PK").unwrap();
        let st = validate_library(&d);
        assert_eq!(st.kind, "ready");
        assert_eq!(st.missing_files, 0);

        // ③b md 库同样准入（§20.4①：M2 起「导入到我的笔记库」产的就是 md 包）
        let conn = manifest::open_and_migrate(&d).unwrap();
        manifest::set_meta(&conn, "export_mode", crate::export::EXPORT_MODE_MD).unwrap();
        drop(conn);
        let st = validate_library(&d);
        assert_eq!(st.kind, "ready", "md 形态的库必须被接受");
        // 行级 export_mode 仍是 native 也不影响准入（准入判据是 meta，与"格式标识参与复用比对"两回事）
        // ④ 非 {native, md} → 拒绝（D0：无只读挂载分支）
        let conn = manifest::open_and_migrate(&d).unwrap();
        manifest::set_meta(&conn, "export_mode", "slim").unwrap();
        drop(conn);
        let st = validate_library(&d);
        assert_eq!(st.kind, "rejected");
        assert!(st.reason.contains("native"), "{}", st.reason);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// 解析器：命中 exported_path、缺失返回 None、非法 guid 拒绝、reload 生效
    #[test]
    fn test_library_resolver() {
        let d = temp_dir("resolver");
        let conn = manifest::open_and_migrate(&d).unwrap();
        let guid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        manifest::upsert_note(
            &conn,
            &manifest::ManifestNote {
                guid: guid.into(),
                title: "t".into(),
                location: "/x/".into(),
                created: "c".into(),
                data_modified: "m".into(),
                url: None,
                doc_type: None,
                has_attachment: false,
                package_size: 0,
                exported_path: "x/我的 笔记.zip".into(),
                exported_size: 0,
                exported_md5: String::new(),
                export_mode: "native".into(),
                exported_at: "t".into(),
                origin: crate::manifest::ORIGIN_WIZNOTE.into(),
                content_format: crate::manifest::FORMAT_HTML.into(),
            },
        )
        .unwrap();
        drop(conn);
        let r = LibraryResolver::new(d.clone()).unwrap();
        assert_eq!(r.note_count(), 1);
        // 两种 guid 形态都命中清单路径（含中文/空格）
        assert_eq!(
            r.resolve(guid),
            Some(d.join("x/我的 笔记.zip"))
        );
        assert_eq!(r.resolve(&format!("{{{guid}}}")), Some(d.join("x/我的 笔记.zip")));
        // 未知 guid / 非法 guid → None
        assert_eq!(r.resolve("00000000-0000-0000-0000-000000000000"), None);
        assert_eq!(r.resolve("../../etc/passwd"), None);
        assert_eq!(r.mode_tag(), "lib");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// 路径安全口径
    #[test]
    fn test_unsafe_rel_path() {
        assert!(is_unsafe_rel_path("/abs/a.zip"));
        assert!(is_unsafe_rel_path("../a.zip"));
        assert!(is_unsafe_rel_path("a/../../b.zip"));
        assert!(!is_unsafe_rel_path("a/我的 笔记.zip"));
        assert!(!is_unsafe_rel_path("笔记.zip"));
    }

    /// 附件入库：拷贝、幂等跳过、缺失计数、tier0 跳过、**落点进库内保留区**（A1'）
    #[test]
    fn test_import_attachments() {
        let lib = temp_dir("imp-lib");
        let src = temp_dir("imp-src");
        std::fs::create_dir_all(src.join("attachments")).unwrap();
        std::fs::write(src.join("attachments/{g}a.log"), b"hello").unwrap();
        // Tier4：源布局里本就在 `_unlinked_attachments/`，落库后应原样保留
        std::fs::create_dir_all(src.join("_unlinked_attachments")).unwrap();
        std::fs::write(src.join("_unlinked_attachments/{g}u.log"), b"unlinked").unwrap();

        let conn = manifest::open_and_migrate(&lib).unwrap();
        for (fp, size, tier) in [
            ("attachments/{g}a.log", 5i64, 1i64),
            ("attachments/{g}missing.log", 5, 1),
            ("_unlinked_attachments/{g}u.log", 8, 4),
            ("db-missing:gg", 0, 0),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO attachment(file_path, display_name, size, tier, document_guid, source)
                 VALUES (?1, ?1, ?2, ?3, NULL, 'db-record')",
                rusqlite::params![fp, size, tier],
            )
            .unwrap();
        }
        let rep = import_attachments(&lib, &src, &conn).unwrap();
        assert_eq!((rep.copied, rep.missing), (2, 1), "Tier1 + Tier4 各拷一份，1 个源缺失");
        // A1'：Tier1–3 落库内保留区 `_attachments/`，**不得**落库根顶层 `attachments/`
        assert!(lib.join("_attachments/{g}a.log").is_file(), "Tier1 附件应落 _attachments/");
        assert_eq!(std::fs::read(lib.join("_attachments/{g}a.log")).unwrap(), b"hello");
        assert!(
            !lib.join("attachments").exists(),
            "库根不得出现顶层 attachments/（与笔记目录树/同步根同级）"
        );
        // Tier4 的 file_path 本就在保留区 → 原样落
        assert!(lib.join("_unlinked_attachments/{g}u.log").is_file(), "Tier4 附件应原样落 _unlinked_attachments/");
        // 幂等重跑：尺寸一致 → 跳过
        let rep2 = import_attachments(&lib, &src, &conn).unwrap();
        assert_eq!((rep2.copied, rep2.skipped), (0, 2));
        std::fs::remove_dir_all(&lib).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
    }

    // ---------------------------------------------------------------- P2 写路径测试

    /// 写测试串行化：写路径共用**进程级**全局锁（`sync::try_acquire` 的 AtomicBool），
    /// 并行跑多个写测试会互相判 LOCK_BUSY。这里显式串行（不依赖 RUST_TEST_THREADS）。
    /// 闸门本身是**跨模块共用**的（`super::test_write_lock`）：只读端护栏测试同样要取写锁。
    fn write_test_lock() -> std::sync::MutexGuard<'static, ()> {
        super::test_write_lock()
    }

    fn write_zip_file(path: &Path, html: &str, with_index_files: bool) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        // 与真实语料一致：index.html 带 UTF-8 BOM
        zw.start_file("index.html", opt).unwrap();
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice(html.as_bytes());
        zw.write_all(&body).unwrap();
        if with_index_files {
            for (n, c) in [
                ("index_files/a.css", "body{color:red}"),
                ("index_files/b.js", "console.log(1)"),
                ("index_files/img/c.png", "PNGDATA"),
                ("attachments/innernote.txt", "inner-attachment"),
            ] {
                zw.start_file(n, opt).unwrap();
                zw.write_all(c.as_bytes()).unwrap();
            }
        }
        zw.finish().unwrap();
    }

    fn zip_entry_names(path: &Path) -> Vec<String> {
        let f = std::fs::File::open(path).unwrap();
        let ar = zip::ZipArchive::new(f).unwrap();
        ar.file_names().map(|s| s.to_string()).collect()
    }

    fn zip_entry(path: &Path, name: &str) -> Vec<u8> {
        let out = crate::library::read_zip_entry(path, name).unwrap();
        out.unwrap_or_default()
    }

    /// md 形态的笔记包（`note.md` **无 BOM** + 可选 `index_files/`/`attachments/` 条目）——
    /// 与 [`crate::export::write_md_package`] 的产物同形态（M2 起库内就是这个形状）。
    fn write_md_zip_file(path: &Path, md: &str, with_index_files: bool) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file(crate::md::NOTE_MD, opt).unwrap();
        zw.write_all(md.as_bytes()).unwrap(); // md 包规范：UTF-8 **无** BOM
        if with_index_files {
            for (n, c) in [
                ("index_files/a.css", "body{color:red}"),
                ("index_files/img/c.png", "PNGDATA"),
                ("attachments/innernote.txt", "inner-attachment"),
            ] {
                zw.start_file(n, opt).unwrap();
                zw.write_all(c.as_bytes()).unwrap();
            }
        }
        zw.finish().unwrap();
    }

    /// 最小可写 **md 库**：2 篇 md 包（一篇带 index_files/）+ 清单（export_mode=md）+ 派生索引
    fn mk_writable_md_lib(tag: &str) -> (PathBuf, PathBuf, String, String) {
        let d = temp_dir(tag);
        let g1 = "33333333-3333-3333-3333-333333333333".to_string();
        let g2 = "44444444-4444-4444-4444-444444444444".to_string();
        let p1 = d.join("工作/笔记一.zip");
        let p2 = d.join("工作/笔记二.zip");
        write_md_zip_file(&p1, "# 笔记一\n\n苹果 banana\n", true);
        write_md_zip_file(&p2, "# 笔记二\n\n橙子 orange\n", false);
        let conn = manifest::open_and_migrate(&d).unwrap();
        manifest::set_meta(&conn, "export_mode", "md").unwrap();
        for (g, t, rel, p) in [
            (&g1, "笔记一", "工作/笔记一.zip", &p1),
            (&g2, "笔记二", "工作/笔记二.zip", &p2),
        ] {
            manifest::upsert_note(
                &conn,
                &manifest::ManifestNote {
                    guid: g.clone(),
                    title: t.into(),
                    location: "/工作/".into(),
                    created: "2024-01-01 00:00:00".into(),
                    data_modified: "2024-01-02 00:00:00".into(),
                    url: None,
                    doc_type: None,
                    has_attachment: false,
                    package_size: 10,
                    exported_path: rel.into(),
                    exported_size: std::fs::metadata(p).unwrap().len() as i64,
                    exported_md5: manifest::md5_file(p).unwrap(),
                    export_mode: "md".into(),
                    exported_at: "2024-01-02 00:00:00Z".into(),
                    origin: crate::manifest::ORIGIN_WIZNOTE.into(),
                    content_format: crate::manifest::FORMAT_HTML.into(),
                },
            )
            .unwrap();
        }
        drop(conn);
        let index_db = d.join("derived.db");
        let resolver = std::sync::Arc::new(LibraryResolver::new(d.clone()).unwrap());
        crate::indexer::build_library_index(&d, resolver, &index_db, &|_, _| {}).unwrap();
        (d, index_db, g1, g2)
    }

    /// 最小可写库：2 篇真实 zip（一篇带 index_files/ 与附件条目）+ 清单 + 派生索引
    fn mk_writable_lib(tag: &str) -> (PathBuf, PathBuf, String, String) {
        let d = temp_dir(tag);
        let g1 = "11111111-1111-1111-1111-111111111111".to_string();
        let g2 = "22222222-2222-2222-2222-222222222222".to_string();
        let p1 = d.join("工作/笔记一.zip");
        let p2 = d.join("工作/笔记二.zip");
        write_zip_file(&p1, "<html><body>苹果 banana</body></html>", true);
        write_zip_file(&p2, "<html><body>橙子 orange</body></html>", false);
        let conn = manifest::open_and_migrate(&d).unwrap();
        manifest::set_meta(&conn, "export_mode", "native").unwrap();
        for (g, t, rel, p) in [
            (&g1, "笔记一", "工作/笔记一.zip", &p1),
            (&g2, "笔记二", "工作/笔记二.zip", &p2),
        ] {
            manifest::upsert_note(
                &conn,
                &manifest::ManifestNote {
                    guid: g.clone(),
                    title: t.into(),
                    location: "/工作/".into(),
                    created: "2024-01-01 00:00:00".into(),
                    data_modified: "2024-01-02 00:00:00".into(),
                    url: None,
                    doc_type: None,
                    has_attachment: false,
                    package_size: 10,
                    exported_path: rel.into(),
                    exported_size: std::fs::metadata(p).unwrap().len() as i64,
                    exported_md5: manifest::md5_file(p).unwrap(),
                    export_mode: "native".into(),
                    exported_at: "2024-01-02 00:00:00Z".into(),
                    origin: crate::manifest::ORIGIN_WIZNOTE.into(),
                    content_format: crate::manifest::FORMAT_HTML.into(),
                },
            )
            .unwrap();
        }
        drop(conn);
        let index_db = d.join("derived.db");
        let resolver = std::sync::Arc::new(LibraryResolver::new(d.clone()).unwrap());
        crate::indexer::build_library_index(&d, resolver, &index_db, &|_, _| {}).unwrap();
        (d, index_db, g1, g2)
    }

    fn fts_hits(index_db: &Path, kw: &str) -> i64 {        let conn = Connection::open(index_db).unwrap();
        conn.query_row(
            "SELECT count(*) FROM note_fts WHERE note_fts MATCH ?1",
            [format!("\"{kw}\"")],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// T1/R11：重写后**条目清单、顺序、非目标条目字节**必须完全一致
    #[test]
    fn test_rewrite_note_zip_keeps_all_entries() {
        let d = temp_dir("rw");
        let zp = d.join("n.zip");
        write_zip_file(&zp, "<html><body>old</body></html>", true);
        let names_before = zip_entry_names(&zp);
        assert_eq!(names_before.len(), 5, "样例应含 index.html + 4 个内嵌条目");
        let others: Vec<(String, Vec<u8>)> = names_before
            .iter()
            .filter(|n| n.as_str() != "index.html")
            .map(|n| (n.clone(), zip_entry(&zp, n)))
            .collect();

        let new_body = {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice("<html><body>NEW 苹果</body></html>".as_bytes());
            v
        };
        let size = rewrite_note_zip(&zp, &[("index.html".to_string(), new_body.clone())]).unwrap();
        assert_eq!(size, std::fs::metadata(&zp).unwrap().len());
        assert_eq!(zip_entry_names(&zp), names_before, "条目集合与顺序必须一致（R11）");
        assert_eq!(zip_entry(&zp, "index.html"), new_body, "目标条目应为新内容");
        for (n, b) in others {
            assert_eq!(zip_entry(&zp, &n), b, "非目标条目字节必须原样搬运（R11）: {n}");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// T9 崩溃注入：rename 前死掉 → 原 zip 完好、库仍自洽；缺条目则失败且不留 tmp
    #[test]
    fn test_rewrite_abort_before_rename_keeps_original() {
        let _s = write_test_lock();
        let (lib, _idx, g1, _g2) = mk_writable_lib("abort");
        let zp = lib.join("工作/笔记一.zip");
        let md5_before = manifest::md5_file(&zp).unwrap();
        let names_before = zip_entry_names(&zp);

        let err = rewrite_note_zip_with(
            &zp,
            &[("index.html".to_string(), b"ABORTED".to_vec())],
            true,
        )
        .unwrap_err();
        assert!(err.contains("ABORT_BEFORE_RENAME"), "{err}");
        assert_eq!(manifest::md5_file(&zp).unwrap(), md5_before, "原 zip 必须逐字节不变");
        assert_eq!(zip_entry_names(&zp), names_before, "原 zip 仍可读且条目完整");
        assert!(tmp_path_of(&zp).exists(), "真实 abort 会留下 .tmp（同篇下次重写时自愈清理）");
        // 「原 zip 完好 + 清单未动」→ 库依然自洽（启动自检不会报错）
        let conn = manifest::manifest_path(&lib);
        let conn = Connection::open(conn).unwrap();
        assert!(
            manifest::check_invariants(&conn, &lib).unwrap().is_empty(),
            "崩溃后清单与磁盘仍须一致"
        );

        // 待替换条目不存在 → 明确失败，且失败路径清掉 tmp、原 zip 不变
        let err = rewrite_note_zip(&zp, &[("index_files/missing.png".to_string(), vec![])]).unwrap_err();
        assert!(err.contains("ENTRY_NOT_FOUND"), "{err}");
        assert!(!tmp_path_of(&zp).exists(), "失败路径应清掉 tmp");
        assert_eq!(manifest::md5_file(&zp).unwrap(), md5_before);

        // ---- T9 第二口径：rename 已成功、清单尚未提交时死掉 ----
        // 磁盘已是新内容，清单仍记旧 md5。这里造一个**同体积**不同内容的文件：
        // 轻量档只 stat 体积 → 结构上看不见；深度档必须报「MD5 不符」，
        // 否则这种半写状态（zip 新、清单旧）永远无人知晓。
        let row = load_note_row(&conn, &g1).unwrap();
        let orig_bytes = std::fs::read(&zp).unwrap();
        let mut tampered = orig_bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF; // 末字节取反：体积一字不差，内容已变
        std::fs::write(&zp, &tampered).unwrap();

        assert_eq!(tampered.len(), orig_bytes.len(), "前提：体积必须相同");
        let light = manifest::check_invariants(&conn, &lib).unwrap();
        assert!(
            !light.iter().any(|w| w.contains("体积不符") || w.contains("MD5 不符")),
            "轻量档按约定只看体积，同体积换内容它发现不了: {light:?}"
        );
        let deep = manifest::check_invariants_deep(&conn, &lib).unwrap();
        assert!(
            deep.iter().any(|w| w.contains("MD5 不符") && w.contains("笔记一.zip")),
            "深度档必须发现该篇与清单不符: {deep:?}"
        );

        // 复原（清理前把状态还原，避免影响其它断言）
        std::fs::write(&zp, &orig_bytes).unwrap();
        assert_eq!(
            manifest::md5_file(&zp).unwrap(),
            row.exported_md5,
            "复原后应重新与清单相符"
        );
        let _ = g1;
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// T2/T4：编辑正文 → 原子落盘 + 清单事务（行 rev/meta rev）+ 单篇索引增量 + BOM 保持
    #[test]
    fn test_save_note_html_transaction_and_index() {
        let _s = write_test_lock();
        let (lib, index_db, g1, _g2) = mk_writable_lib("save");
        let zp = lib.join("工作/笔记一.zip");
        // 注：note_fts 是 FTS5 **trigram** 分词 → 检索词至少 3 个字符（中文同）
        assert_eq!(fts_hits(&index_db, "banana"), 1, "写前可检索原文");

        let html = "<html><body>苹果 grape 葡萄</body><script>x=1</script></html>";
        let rep = save_note_html(&lib, &index_db, &g1, html).unwrap();
        assert_eq!(rep.op, OP_SAVE_HTML);
        assert!(rep.index_updated, "索引应随写更新: {:?}", rep.warnings);
        assert_eq!(rep.revision, 1, "行级 revision 写入即 +1");
        // v6：库内写**不铸版** —— `meta.revision` 只在云同步上行提交点推进（§6.3 ②）
        assert_eq!(rep.manifest_revision, 0, "库内写不得推进清单版本");

        // 落盘字节：BOM 保持 + 只替换 index.html
        let raw = zip_entry(&zp, "index.html");
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]), "写回须保持原 BOM 形态");
        assert_eq!(
            String::from_utf8_lossy(&raw[3..]),
            html,
            "剥 BOM 后应逐字节等于传入正文"
        );
        assert_eq!(zip_entry_names(&zp).len(), 5, "index_files/ 与 attachments/ 条目不得丢");
        // 清单事务：md5/size 与磁盘一致，data_modified 刷新，日志有 <script> 警告
        let conn = manifest::open_and_migrate(&lib).unwrap();
        let row = load_note_row(&conn, &g1).unwrap();
        assert_eq!(row.exported_md5, manifest::md5_file(&zp).unwrap());
        assert_eq!(row.exported_size, std::fs::metadata(&zp).unwrap().len() as i64);
        assert_ne!(rep.data_modified, "2024-01-02 00:00:00", "data_modified 应刷新");
        let db_mod: String = conn
            .query_row("SELECT data_modified FROM note WHERE guid=?1", [&g1], |r| r.get(0))
            .unwrap();
        assert_eq!(db_mod, rep.data_modified, "报告与清单行的 data_modified 须一致");
        assert_eq!(manifest::load_note_revision(&conn, &g1).unwrap(), Some(1));
        // §6.3 ②（**回归闩，最重要的一条**）：库内写**不得**改动这两样"云端铸造物"
        assert_eq!(
            manifest::get_meta(&conn, "revision").unwrap().as_deref(),
            Some("0"),
            "库内写不得改动 meta.revision（DDL 把它播种为 0；加回 bump_revision 即变 1 ⇒ FAILED）"
        );
        assert_eq!(
            manifest::load_note_synced_revisions(&conn).unwrap().get(&g1).copied().unwrap_or(0),
            0,
            "库内写不得改动 note.synced_revision"
        );
        // §6.3 ①：但**必须**置脏闩 —— 否则这一版永远上不了云
        assert!(
            manifest::load_unsynced_note_guids(&conn).unwrap().contains(&g1),
            "库内写必须置脏闩"
        );
        assert!(
            manifest::check_invariants(&conn, &lib).unwrap().is_empty(),
            "写后清单自检必须零告警"
        );
        assert!(rep.warnings.iter().any(|w| w.contains("<script>")), "{:?}", rep.warnings);

        // T4：写完**立刻**用新解析器读 → 新内容；索引 FTS 命中新词、旧词消失
        assert_eq!(fts_hits(&index_db, "grape"), 1, "新词可检索");
        assert_eq!(fts_hits(&index_db, "banana"), 0, "旧正文已换掉");
        let idx = Connection::open(&index_db).unwrap();
        let (title, folder): (String, String) = idx
            .query_row(
                "SELECT title, folder FROM note_fts WHERE guid=?1",
                [&g1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((title.as_str(), folder.as_str()), ("笔记一", "/工作/"));
        let n: i64 = idx
            .query_row("SELECT count(*) FROM note WHERE guid=?1", [&g1], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "索引不得出现重复行");

        // 第二次写：行 rev 继续递增（单调）
        let rep2 = save_note_html(&lib, &index_db, &g1, "<html><body>苹果 grape 葡萄2</body></html>")
            .unwrap();
        assert_eq!(rep2.revision, 2);
        assert_eq!(rep2.manifest_revision, 0, "第二次写同样不铸版（行计数器与版本号是两码事）");

        // 空正文 / 宿主引用 → 拒绝，且库不变（R12）
        let md5_before = manifest::md5_file(&zp).unwrap();
        assert!(save_note_html(&lib, &index_db, &g1, "   ").unwrap_err().contains("EMPTY_HTML"));
        assert!(save_note_html(&lib, &index_db, &g1, "<img src=\"wiznote://x/y\">")
            .unwrap_err()
            .contains("HTML_HOST_REF"));
        assert_eq!(manifest::md5_file(&zp).unwrap(), md5_before, "拒绝路径不得改动文件");
        // 未知 guid → NOTE_NOT_FOUND
        assert!(save_note_html(&lib, &index_db, "00000000-0000-0000-0000-000000000000", "<p>x</p>")
            .unwrap_err()
            .contains("NOTE_NOT_FOUND"));
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// 断言"刚刚那次库内写 ① 置了闩、② 没动 `meta.revision`"。
    ///
    /// `keeps_synced_revision` = 该写是否**保留**原有的 `note.synced_revision`。
    /// 只有 `restore_note` 传 `false`：它写的是**新建行**（行刚从墓碑还原回来），云端铸造号
    /// 无从得知（墓碑快照里**不含**该列，它刻意不在 `NoteRow` 里），故按 DDL 默认落 0 ——
    /// 语义是"这一版还没进过云端"，提交点会补上。这是 §6.3 ② 的**唯一**例外，且方向安全：
    /// 它把铸造号**归零**（判据侧只会更保守：可能多留一份远端版副本），**不会**像
    /// `bump_revision` 那样把它推高（那才会把判据顶成恒假）。
    /// 其余写路径改的都是**已存在的行**，一律不得碰这一列。
    fn assert_latch_set_and_no_mint(
        lib: &Path,
        guid: &str,
        before: &(u64, std::collections::HashMap<String, i64>),
        keeps_synced_revision: bool,
        what: &str,
    ) {
        let c = manifest::open_and_migrate(lib).unwrap();
        assert_eq!(
            manifest::current_revision(&c),
            before.0,
            "{what}：不得改动 meta.revision（§6.3 ②）"
        );
        let now = manifest::load_note_synced_revisions(&c).unwrap().get(guid).copied().unwrap_or(0);
        if keeps_synced_revision {
            assert_eq!(
                now,
                before.1.get(guid).copied().unwrap_or(0),
                "{what}：不得改动 note.synced_revision（§6.3 ②）"
            );
        } else {
            assert_eq!(now, 0, "{what}：新建行 ⇒ 铸造号归 0（未知，提交点补）");
        }
        assert!(
            manifest::load_unsynced_note_guids(&c).unwrap().contains(guid),
            "{what}：必须置脏闩（§6.3 ①）"
        );
    }

    /// **§6.3 ①②（承重表里最重要的一条）**：库内**任何**写 —— 改正文 / 改名 / 搬家 / 删除 /
    /// 恢复 —— 都必须 ① 置脏闩、② **不动** `meta.revision` 与 `note.synced_revision`
    /// （`restore_note` 的 `synced_revision` 归 0 是唯一例外，理由见下方助手文档）。
    ///
    /// 为什么它是回归闩：v6 之前每条写路径都调 `bump_revision()`，"本地改过一版"会被自己的
    /// 计数器顶成"与远端持平"，同步判据于是恒假 —— D 轮那两处真 bug 的共同原形。
    /// **变异检查**：把 `bump_revision()` 加回**任何一条**写路径 → 本测试必 FAILED。
    #[test]
    fn test_write_path_sets_latch_and_never_mints() {
        let _s = write_test_lock();
        let (lib, index_db, g1, g2) = mk_writable_lib("latch");

        // 起点：fixture 从未铸过版
        {
            let c = manifest::open_and_migrate(&lib).unwrap();
            assert_eq!(
                manifest::get_meta(&c, "revision").unwrap().as_deref(),
                Some("0"),
                "fixture 从未铸过版（DDL 把 revision 播种为 0）"
            );
        }
        // 先"提交一次"把闩清干净（用提交点的 API，不模拟：`bump_revision` 本来就是它的动作），
        // 这样后面每个操作各自证明"是**它**置的闩"，而不是继承 fixture 的初值。
        {
            let c = manifest::open_and_migrate(&lib).unwrap();
            let rev = manifest::bump_revision(&c, "t").unwrap();
            manifest::mark_notes_synced(&c, &[g1.clone(), g2.clone()], rev).unwrap();
            manifest::recompute_watermarks(&c).unwrap();
        }
        let baseline = {
            let c = manifest::open_and_migrate(&lib).unwrap();
            (manifest::current_revision(&c), manifest::load_note_synced_revisions(&c).unwrap())
        };
        {
            let c = manifest::open_readonly(&lib).unwrap();
            assert!(manifest::load_unsynced_note_guids(&c).unwrap().is_empty(), "清闩后应无待上行项");
        }

        // ---- ① 改正文 ----
        save_note_html(&lib, &index_db, &g1, "<html><body>改过正文</body></html>").unwrap();
        assert_latch_set_and_no_mint(&lib, &g1, &baseline, true, "改正文");

        // ---- ② 改名（只动元信息 ⇒ 对应为知 `INFO_CHANGED` 那一支）----
        rename_note(&lib, &index_db, &g2, "新的标题").unwrap();
        assert_latch_set_and_no_mint(&lib, &g2, &baseline, true, "改名");

        // ---- ③ 搬家 ----
        move_note(&lib, &index_db, &g2, "/别处/").unwrap();
        assert_latch_set_and_no_mint(&lib, &g2, &baseline, true, "搬家");

        // ---- ④ 删除：行进墓碑 ⇒ 闩落在"墓碑未发布"这一维 ----
        delete_note(&lib, &index_db, &g1).unwrap();
        {
            let c = manifest::open_and_migrate(&lib).unwrap();
            assert!(
                manifest::load_unpublished_tombstone_guids(&c).unwrap().contains(&g1),
                "删除必须在墓碑上留下未发布的印记（删除维度的闩）"
            );
            assert_eq!(
                manifest::current_revision(&c),
                baseline.0,
                "删除不得改动 meta.revision（§6.3 ②）"
            );
        }

        // ---- ⑤ 恢复：行回 note 表 ⇒ 必须带闩回来（铸造号归 0：新建行，见助手文档）----
        restore_note(&lib, &index_db, &g1).unwrap();
        assert_latch_set_and_no_mint(&lib, &g1, &baseline, false, "恢复");
    }

    /// T3/§4.4：写锁与同步共用 → 正忙时明确报 LOCK_BUSY（不排队静默）
    #[test]
    fn test_write_lock_busy() {
        let _s = write_test_lock();
        let (lib, index_db, g1, _g2) = mk_writable_lib("lock");
        let guard = crate::sync::try_acquire(&lib).unwrap();
        assert!(guard.is_some(), "首次取锁应成功");
        let err = save_note_html(&lib, &index_db, &g1, "<p>x</p>").unwrap_err();
        assert!(err.contains("LOCK_BUSY"), "{err}");
        assert!(delete_note(&lib, &index_db, &g1).unwrap_err().contains("LOCK_BUSY"));
        drop(guard);
        // 锁释放后可正常写
        assert!(save_note_html(&lib, &index_db, &g1, "<p>ok</p>").is_ok());
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// T6：重命名 / 移动 —— 路径与清单一致、解析器 reload 生效、索引目录树补齐、撞名加 guid8
    #[test]
    fn test_rename_and_move() {
        let _s = write_test_lock();
        let (lib, index_db, g1, g2) = mk_writable_lib("mv");

        // 重命名（同名净化后仍唯一 → 直接换名）
        let rep = rename_note(&lib, &index_db, &g1, "笔记一改名").unwrap();
        assert_eq!(rep.op, OP_RENAME);
        assert_eq!(rep.exported_path, "工作/笔记一改名.zip");
        assert!(lib.join(&rep.exported_path).is_file());
        assert!(!lib.join("工作/笔记一.zip").exists(), "旧路径文件必须已搬走");
        // 内容字节不变 → md5 不变（只有元数据变）
        assert_eq!(rep.exported_md5, manifest::md5_file(&lib.join(&rep.exported_path)).unwrap());
        // 解析器 reload 后命中新路径（T4）
        let r = LibraryResolver::new(lib.clone()).unwrap();
        assert_eq!(r.resolve(&g1), Some(lib.join("工作/笔记一改名.zip")));
        // 索引同步：标题新、无重复行
        assert_eq!(fts_hits(&index_db, "笔记一改名"), 1);

        // 撞名：把第二篇改成同一标题 → 追加 guid 前 8 位
        let rep2 = rename_note(&lib, &index_db, &g2, "笔记一改名").unwrap();
        assert_eq!(rep2.exported_path, format!("工作/笔记一改名_{}.zip", &g2[..8]));

        // 移动到新目录
        let rep3 = move_note(&lib, &index_db, &g1, "生活/子目录").unwrap();
        assert_eq!(rep3.op, OP_MOVE);
        assert_eq!(rep3.exported_path, "生活/子目录/笔记一改名.zip");
        assert!(lib.join(&rep3.exported_path).is_file());
        assert_eq!(rep3.revision, 2, "rename/move 也算一次写入（该篇修订号单调递增）");
        // 空目录被清理（原 /工作/ 仍留有另一篇 → 不应被删）
        assert!(lib.join("工作").is_dir());
        // 索引：folder 祖先补齐 + location 更新
        let idx = Connection::open(&index_db).unwrap();
        let loc: String = idx
            .query_row("SELECT location FROM note WHERE guid=?1", [&g1], |r| r.get(0))
            .unwrap();
        assert_eq!(loc, "/生活/子目录/");
        let folders: i64 = idx
            .query_row("SELECT count(*) FROM folder WHERE path IN ('/生活/','/生活/子目录/')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(folders, 2, "新 location 的目录祖先应补进 folder");
        // 清单自检零告警（旧路径无孤儿、新路径有行）
        let conn = manifest::open_and_migrate(&lib).unwrap();
        assert!(manifest::check_invariants(&conn, &lib).unwrap().is_empty(), "移动后不得留孤儿");

        // 非法的 location 段被拒
        assert!(move_note(&lib, &index_db, &g1, "../etc").unwrap_err().contains("PATH_UNSAFE"));
        assert!(rename_note(&lib, &index_db, &g1, "  ").unwrap_err().contains("EMPTY_TITLE"));
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// T7：删除 → 文件入 `_trash/{日期}/`、墓碑三件套、索引删行、自检零告警
    #[test]
    fn test_delete_note_to_trash() {
        let _s = write_test_lock();
        let (lib, index_db, g1, _g2) = mk_writable_lib("del");
        let today = crate::sync::today_utc();
        let rep = delete_note(&lib, &index_db, &g1).unwrap();
        assert_eq!(rep.op, OP_DELETE);
        assert_eq!(rep.exported_path, "工作/笔记一.zip");
        let trashed = lib.join("_trash").join(&today).join("笔记一.zip");
        assert!(trashed.is_file(), "文件应移入 {}", trashed.display());
        assert!(!lib.join("工作/笔记一.zip").exists());
        assert!(lib.join("工作/笔记二.zip").is_file(), "同目录另一篇不得受影响");

        let conn = manifest::open_and_migrate(&lib).unwrap();
        assert!(load_note_row(&conn, &g1).unwrap_err().contains("NOTE_NOT_FOUND"));
        let (title, size, md5): (String, i64, String) = conn
            .query_row(
                "SELECT title, size, exported_md5 FROM deleted WHERE guid=?1",
                [&g1],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "笔记一");
        assert!(size > 0 && md5.len() == 32, "墓碑须带 size/md5（回收站展示与恢复核对用）");
        // §6.3 ②：库内**删除**同样不铸版 —— fixture 从未铸过版，故仍是"未设置"
        // （把 `bump_revision` 加回删除路径 → 这里会变成 Some("1")，必 FAILED）
        assert_eq!(
            manifest::get_meta(&conn, "revision").unwrap().as_deref(),
            Some("0"),
            "库内删除不得改动 meta.revision（播种 0；加回 bump_revision 即 FAILED）"
        );
        // `_trash/` 是系统保留区 → 其 zip 不算孤儿（否则每次删除都会误报）
        assert!(
            manifest::check_invariants(&conn, &lib).unwrap().is_empty(),
            "删除后清单自检必须零告警（_trash 不入孤儿统计）"
        );
        // 索引：该篇已移除（用 ≥3 字符的词，「苹果」是 2 汉字 → trigram 恒 0，测不出东西）
        assert_eq!(fts_hits(&index_db, "banana"), 0);
        let idx = Connection::open(&index_db).unwrap();
        let n: i64 = idx
            .query_row("SELECT count(*) FROM note WHERE guid=?1", [&g1], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        // 重复删除 → NOTE_NOT_FOUND
        assert!(delete_note(&lib, &index_db, &g1).unwrap_err().contains("NOTE_NOT_FOUND"));
        // 删掉同目录最后一篇 → 空目录被清理（best-effort 剪枝）
        delete_note(&lib, &index_db, &_g2).unwrap();
        assert!(!lib.join("工作").exists(), "目录已空 → 应被清理");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// T7 恢复：**删除 ⇄ 恢复** 后清单行逐字段回到删除前（v4 载荷的意义所在）、
    /// 文件回原路径、墓碑消失、索引重新命中、库自检零告警
    #[test]
    fn test_restore_note_roundtrip() {
        let _s = write_test_lock();
        let (lib, index_db, g1, _g2) = mk_writable_lib("restore");
        let orig_path = lib.join("工作/笔记一.zip");
        let orig_md5 = manifest::md5_file(&orig_path).unwrap();
        let orig_size = std::fs::metadata(&orig_path).unwrap().len() as i64;

        // 删除前记下整行（含 created / data_modified / exported_at —— 这三项正是
        // "没有快照就只能丢"的字段）
        let before = {
            let conn = manifest::open_and_migrate(&lib).unwrap();
            load_note_row(&conn, &g1).unwrap()
        };
        assert_eq!(before.created, "2024-01-01 00:00:00");

        let del = delete_note(&lib, &index_db, &g1).unwrap();
        assert_eq!(del.op, OP_DELETE);
        // 墓碑带上了恢复载荷与实际回收站路径
        {
            let conn = manifest::open_and_migrate(&lib).unwrap();
            let (nj, tr): (Option<String>, Option<String>) = conn
                .query_row("SELECT note_json, trash_rel FROM deleted WHERE guid=?1", [&g1], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            assert!(nj.as_deref().unwrap_or("").contains("笔记一"), "墓碑须存下整行快照: {nj:?}");
            assert_eq!(
                tr.as_deref(),
                Some(format!("_trash/{}/笔记一.zip", crate::sync::today_utc()).as_str())
            );
        }
        // 列表可列出且标为可恢复
        let listed = list_trash(&lib).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].restorable && listed[0].has_snapshot, "{:?}", listed[0]);
        assert_eq!(listed[0].guid, g1);

        let rep = restore_note(&lib, &index_db, &g1).unwrap();
        assert_eq!(rep.op, OP_RESTORE);
        assert_eq!(rep.exported_path, "工作/笔记一.zip");
        assert!(orig_path.is_file(), "文件必须回到删除前的路径");
        assert_eq!(manifest::md5_file(&orig_path).unwrap(), orig_md5, "内容逐字节不变");
        assert_eq!(rep.exported_size, orig_size);
        assert!(rep.revision > 0, "恢复是一次写入 → 行 revision 必须回到 >0（同步要能看见）");
        assert_eq!(rep.warnings, Vec::<String>::new(), "恢复不该产生告警");

        // 逐字段还原：**除 revision（恢复本身是一次写，故 +1）外全等** —— 含 data_modified
        let after = {
            let conn = manifest::open_and_migrate(&lib).unwrap();
            let row = load_note_row(&conn, &g1).unwrap();
            let tomb: i64 = conn
                .query_row("SELECT count(*) FROM deleted WHERE guid=?1", [&g1], |r| r.get(0))
                .unwrap();
            assert_eq!(tomb, 0, "恢复后墓碑必须消失");
            assert!(manifest::check_invariants(&conn, &lib).unwrap().is_empty(), "恢复后库自洽");
            row
        };
        assert_eq!(after.title, before.title);
        assert_eq!(after.location, before.location);
        assert_eq!(after.created, before.created, "created 必须来自快照，不得被兜底值顶掉");
        assert_eq!(after.exported_at, before.exported_at, "exported_at 必须来自快照");
        assert_eq!(after.export_mode, before.export_mode);
        assert_eq!(after.exported_md5, before.exported_md5);
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(
            after.data_modified, before.data_modified,
            "恢复=撤销删除：data_modified 必须来自快照。写恢复时刻会让旧笔记在\
             「按修改时间排序」的列表里冒到最前面 —— 那是假信息（D0 无损）"
        );

        // 索引重新命中该篇（T7 验收要点：还原后解析器路径与索引一致）
        // 注：验收词必须 ≥3 字符 —— `note_fts` 用 trigram 分词，2 个汉字建不进索引
        // （「苹果」恒 0 命中，拿它断言等于什么都没测）
        assert_eq!(fts_hits(&index_db, "banana"), 1);
        let r = LibraryResolver::new(lib.clone()).unwrap();
        assert_eq!(r.resolve(&g1), Some(orig_path.clone()));

        // 幂等边界：再恢复一次 → 墓碑已不在
        assert!(restore_note(&lib, &index_db, &g1).unwrap_err().contains("TOMBSTONE_NOT_FOUND"));
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// T7 恢复的失败路径：无载荷 / 文件已丢 / 目标被占 / 无墓碑 ——
    /// 一律明确报错，**不静默降级**（宁可让用户看见"恢复不了"，也不给半残的笔记）
    #[test]
    fn test_restore_note_failure_paths() {
        let _s = write_test_lock();

        // ① 无载荷（模拟 v3 及更早墓碑）→ NO_TOMBSTONE_PAYLOAD，文件留在回收站
        {
            let (lib, index_db, g1, _g2) = mk_writable_lib("rf-nopayload");
            delete_note(&lib, &index_db, &g1).unwrap();
            {
                let conn = Connection::open(manifest::manifest_path(&lib)).unwrap();
                conn.execute("UPDATE deleted SET note_json = NULL WHERE guid = ?1", [&g1]).unwrap();
            }
            let err = restore_note(&lib, &index_db, &g1).unwrap_err();
            assert!(err.contains("NO_TOMBSTONE_PAYLOAD"), "{err}");
            let e = list_trash(&lib).unwrap().into_iter().find(|e| e.guid == g1).unwrap();
            assert!(!e.has_snapshot, "无载荷的墓碑必须如实标出: {e:?}");
            assert!(e.restorable, "文件在 → 仍标可恢复（只是不能还原清单行）");
            std::fs::remove_dir_all(&lib).unwrap();
        }

        // ② 回收站文件已丢 → TRASH_FILE_MISSING + 列表转「不可恢复」
        {
            let (lib, index_db, g1, _g2) = mk_writable_lib("rf-missing");
            delete_note(&lib, &index_db, &g1).unwrap();
            let e = list_trash(&lib).unwrap().into_iter().find(|e| e.guid == g1).unwrap();
            assert!(e.restorable && e.has_snapshot, "{e:?}");
            std::fs::remove_file(lib.join(e.trash_rel.expect("墓碑须记实际回收站路径"))).unwrap();
            assert!(restore_note(&lib, &index_db, &g1).unwrap_err().contains("TRASH_FILE_MISSING"));
            let e2 = list_trash(&lib).unwrap().into_iter().find(|e| e.guid == g1).unwrap();
            assert!(!e2.restorable && e2.reason.contains("已无该文件"), "{e2:?}");
            std::fs::remove_dir_all(&lib).unwrap();
        }

        // ③ 目标路径被占 → PATH_TAKEN（绝不覆盖别人）
        {
            let (lib, index_db, g1, _g2) = mk_writable_lib("rf-taken");
            delete_note(&lib, &index_db, &g1).unwrap();
            write_zip_file(&lib.join("工作/笔记一.zip"), "<html><body>别的笔记</body></html>", false);
            assert!(restore_note(&lib, &index_db, &g1).unwrap_err().contains("PATH_TAKEN"));
            std::fs::remove_dir_all(&lib).unwrap();
        }

        // ④ 墓碑不存在（含"已恢复过"）
        {
            let (lib, index_db, _g1, _g2) = mk_writable_lib("rf-notomb");
            let err = restore_note(&lib, &index_db, "33333333-3333-3333-3333-333333333333")
                .unwrap_err();
            assert!(err.contains("TOMBSTONE_NOT_FOUND"), "{err}");
            assert!(list_trash(&lib).unwrap().is_empty());
            std::fs::remove_dir_all(&lib).unwrap();
        }
    }

    /// v2 → v3 清单迁移：补 revision 列、版本推进、既有行 revision=0
    #[test]
    fn test_manifest_v2_to_v3_migration() {
        let d = temp_dir("mig-v3");
        let db = manifest::manifest_path(&d);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE note (
                   guid TEXT PRIMARY KEY, title TEXT NOT NULL, location TEXT NOT NULL,
                   created TEXT NOT NULL, data_modified TEXT NOT NULL, url TEXT, doc_type TEXT,
                   has_attachment INTEGER NOT NULL DEFAULT 0, package_size INTEGER NOT NULL,
                   exported_path TEXT NOT NULL, exported_size INTEGER NOT NULL,
                   exported_md5 TEXT NOT NULL, export_mode TEXT NOT NULL, exported_at TEXT NOT NULL);
                 INSERT INTO meta VALUES ('schema_version','2'), ('revision','7');
                 INSERT INTO note VALUES ('aaaaaaaa-1111-2222-3333-444444444444','t','/a/','c','m',
                   NULL,NULL,0,1,'a/t.zip',2,'m','native','e');",
            )
            .unwrap();
        }
        let conn = manifest::open_and_migrate(&d).unwrap();
        // v1/v2/v3 一律推进到当前版本（v3 加 note.revision，v4 给 deleted 补恢复两列）
        assert_eq!(
            manifest::get_meta(&conn, "schema_version").unwrap().as_deref(),
            Some(manifest::SCHEMA_VERSION)
        );
        assert_eq!(
            manifest::load_note_revision(&conn, "aaaaaaaa-1111-2222-3333-444444444444").unwrap(),
            Some(0)
        );
        // 幂等：再迁一次不出错、revision 值不被重置
        drop(conn);
        let conn = manifest::open_and_migrate(&d).unwrap();
        assert_eq!(manifest::get_meta(&conn, "revision").unwrap().as_deref(), Some("7"));
        assert_eq!(manifest::bump_note_revision(&conn, "aaaaaaaa-1111-2222-3333-444444444444").unwrap(), Some(1));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// v3（有 note.revision、deleted 无恢复两列）→ 当前版本：**加列不损数据**
    /// （v4 补 `deleted` 的恢复两列；v5 补 `note.origin` / `note.content_format`）
    #[test]
    fn test_manifest_v3_to_current_migration() {
        let d = temp_dir("mig-v4");
        let db = manifest::manifest_path(&d);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE note (
                   guid TEXT PRIMARY KEY, title TEXT NOT NULL, location TEXT NOT NULL,
                   created TEXT NOT NULL, data_modified TEXT NOT NULL, url TEXT, doc_type TEXT,
                   has_attachment INTEGER NOT NULL DEFAULT 0, package_size INTEGER NOT NULL,
                   exported_path TEXT NOT NULL, exported_size INTEGER NOT NULL,
                   exported_md5 TEXT NOT NULL, export_mode TEXT NOT NULL, exported_at TEXT NOT NULL,
                   revision INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE deleted (
                   guid TEXT PRIMARY KEY, last_path TEXT NOT NULL, removed_at TEXT NOT NULL,
                   title TEXT, size INTEGER, exported_md5 TEXT);
                 CREATE TABLE attachment (file_path TEXT PRIMARY KEY, display_name TEXT NOT NULL,
                   size INTEGER NOT NULL, tier INTEGER NOT NULL, document_guid TEXT,
                   source TEXT NOT NULL, cloud_key TEXT);
                 CREATE TABLE attachment_doc (file_path TEXT NOT NULL, document_guid TEXT NOT NULL,
                   PRIMARY KEY (file_path, document_guid));
                 INSERT INTO meta VALUES ('schema_version','3'), ('revision','9');
                 INSERT INTO deleted VALUES ('bbbbbbbb-1111-2222-3333-444444444444','a/b.zip',
                   '2026-09-01 00:00:00Z','旧墓碑',10,'md5x');",
            )
            .unwrap();
        }
        let conn = manifest::open_and_migrate(&d).unwrap();
        assert_eq!(
            manifest::get_meta(&conn, "schema_version").unwrap().as_deref(),
            Some(manifest::SCHEMA_VERSION)
        );
        // 旧墓碑的既有列不丢；新列为 NULL（源时代墓碑没有恢复载荷 → 恢复时按"文件可能已不在"处理）
        let (title, nj, tr): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT ifnull(title,''), note_json, trash_rel FROM deleted WHERE guid = ?1",
                ["bbbbbbbb-1111-2222-3333-444444444444"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "旧墓碑");
        assert!(nj.is_none() && tr.is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// 兼容回归（S2 真机验收撞到的缺陷）：**未迁移的 v3 库**上「看回收站」不得报 SQL 错。
    ///
    /// `list_trash` 走 `open_readonly`（读不写、不迁移），却引用 v4 的两列 ——
    /// 若不做列存在性降级，v3 库会直接 `no such column: note_json`，而这是用户
    /// **首次打开回收站就会撞上**的路径（只有写操作才会触发迁移）。
    #[test]
    fn test_list_trash_on_v3_library_degrades() {
        let d = temp_dir("trash-v3-degrade");
        let db = manifest::manifest_path(&d);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE deleted (
                   guid TEXT PRIMARY KEY, last_path TEXT NOT NULL, removed_at TEXT NOT NULL,
                   title TEXT, size INTEGER, exported_md5 TEXT);
                 INSERT INTO meta VALUES ('schema_version','3');
                 INSERT INTO deleted VALUES ('cccccccc-1111-2222-3333-444444444444','a/b.zip',
                   '2026-09-01 00:00:00Z','旧墓碑',10,'md5x');",
            )
            .unwrap();
        }
        let items = list_trash(&d).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "旧墓碑");
        assert!(!items[0].has_snapshot, "v3 墓碑没有恢复载荷");
        assert!(items[0].trash_rel.is_none());
        // 关键：只读打开**没**改库（schema 仍 v3、列仍缺）—— 看回收站不该是写动作
        {
            let conn = Connection::open(&db).unwrap();
            assert_eq!(
                manifest::get_meta(&conn, "schema_version").unwrap().as_deref(),
                Some("3")
            );
            let cols: Vec<String> = {
                let mut st = conn.prepare("PRAGMA table_info('deleted')").unwrap();
                st.query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .flatten()
                    .collect()
            };
            assert!(!cols.iter().any(|c| c == "note_json"));
        }
        // 迁移到当前版本后同一句仍工作（这次走新列分支，载荷为 NULL）
        let conn = manifest::open_and_migrate(&d).unwrap();
        assert_eq!(
            manifest::get_meta(&conn, "schema_version").unwrap().as_deref(),
            Some(manifest::SCHEMA_VERSION)
        );
        drop(conn);
        let items = list_trash(&d).unwrap();
        assert_eq!(items.len(), 1);
        assert!(!items[0].has_snapshot);
        // 极端退化：连 deleted 表都没有（v1 老库）→ 空列表而非报表不存在
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("DROP TABLE deleted;").unwrap();
        }
        assert!(list_trash(&d).unwrap().is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    // ---------------------------------------------------------------- M3：md 包正文写（§20.3）

    /// M3 核心：md 包 `save_note_md` → **只换 `note.md`**，其余条目字节不变、
    /// 不新建 `index.html`、写回**无 BOM**（md 包规范）、清单与索引同步。
    #[test]
    fn test_save_note_md_replaces_note_md_only() {
        let _s = write_test_lock();
        let (lib, index_db, g1, _g2) = mk_writable_md_lib("md-save");
        let zp = lib.join("工作/笔记一.zip");
        let names_before = zip_entry_names(&zp);
        assert_eq!(names_before.len(), 4, "note.md + 3 个 index_files/attachments 条目");
        assert!(!names_before.contains(&"index.html".to_string()), "md 包不该有 index.html");
        // 逐条目快照（除 note.md 外必须逐字节不变）
        let others_before: Vec<(String, Vec<u8>)> = names_before
            .iter()
            .filter(|n| n.as_str() != crate::md::NOTE_MD)
            .map(|n| (n.clone(), zip_entry(&zp, n)))
            .collect();
        assert_eq!(fts_hits(&index_db, "banana"), 1, "写前可检索原文");

        let md = "# 笔记一\n\n苹果 grape 葡萄\n\n```sh\nls\n```\n";
        let rep = save_note_md(&lib, &index_db, &g1, md).unwrap();
        assert_eq!(rep.op, OP_SAVE_MD, "op 必须能区分 md 写与 html 写");
        assert!(rep.index_updated, "索引应随写更新: {:?}", rep.warnings);
        assert_eq!(rep.revision, 1, "行级 revision 写入即 +1");
        assert!(rep.warnings.is_empty(), "干净正文不该有警告: {:?}", rep.warnings);

        // 落盘：note.md 逐字节等于传入 md（**无 BOM**，因为 md 包规范就是无 BOM）
        let raw = zip_entry(&zp, crate::md::NOTE_MD);
        assert!(!raw.starts_with(&[0xEF, 0xBB, 0xBF]), "md 包正文不得带 BOM");
        assert_eq!(String::from_utf8(raw).unwrap(), md, "正文应逐字节等于传入 md");
        // 条目集合与其余条目字节：一对一替换，别的条目一个不动（R11 硬约束）
        let names_after = zip_entry_names(&zp);
        assert_eq!(names_after, names_before, "条目集合与顺序不得变");
        for (n, b) in &others_before {
            assert_eq!(&zip_entry(&zp, n), b, "条目 {n} 必须逐字节不变");
        }
        // 也没有凭空多出 index.html
        assert!(!names_after.contains(&"index.html".to_string()));

        // 清单事务与自检
        let conn = manifest::open_and_migrate(&lib).unwrap();
        let row = load_note_row(&conn, &g1).unwrap();
        assert_eq!(row.exported_md5, manifest::md5_file(&zp).unwrap());
        assert_eq!(row.exported_size, std::fs::metadata(&zp).unwrap().len() as i64);
        assert_eq!(row.export_mode, "md", "格式标识仍为 md");
        assert_ne!(row.data_modified, "2024-01-02 00:00:00", "data_modified 应刷新");
        assert_eq!(manifest::load_note_revision(&conn, &g1).unwrap(), Some(1));
        assert!(
            manifest::check_invariants(&conn, &lib).unwrap().is_empty(),
            "写后清单自检必须零告警"
        );

        // 索引：新词进、旧词出，且正文口径就是 md 原文（围栏语法原样进索引，不做 HTML 抽取）
        assert_eq!(fts_hits(&index_db, "grape"), 1, "新词可检索");
        assert_eq!(fts_hits(&index_db, "banana"), 0, "旧正文已换掉");
        let idx = Connection::open(&index_db).unwrap();
        let body: String = idx
            .query_row("SELECT body FROM note_fts WHERE guid=?1", [&g1], |r| r.get(0))
            .unwrap();
        assert_eq!(body, md, "索引正文应逐字节等于 md 原文");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// `save_note_md` 对**为知原生包**必须明确拒绝（不静默改造别处的正文），且零写入
    #[test]
    fn test_save_note_md_on_native_package_is_rejected() {
        let _s = write_test_lock();
        let (lib, index_db, g1, _g2) = mk_writable_lib("md-on-native");
        let zp = lib.join("工作/笔记一.zip");
        let before = std::fs::read(&zp).unwrap();

        let err = save_note_md(&lib, &index_db, &g1, "# 标题\n").unwrap_err();
        assert!(err.starts_with("ENTRY_NOT_FOUND"), "{err}");
        assert!(err.contains("note.md"), "错误必须点名缺的是哪个条目: {err}");
        assert_eq!(std::fs::read(&zp).unwrap(), before, "失败路径不得改动 zip");
        assert!(!tmp_path_of(&zp).exists(), "失败路径应清掉 tmp");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// 校验两档（与 HTML 口径对齐）：空正文 / 宿主引用硬拒；`<script>` 与"无文本"只警告
    #[test]
    fn test_validate_note_md_rules() {
        assert!(validate_note_md("   \n\t ").unwrap_err().starts_with("EMPTY_MD"));
        assert!(validate_note_md("[x](wiznote://a/b)").unwrap_err().starts_with("MD_HOST_REF"));
        // 空正文 / 宿主引用都不得落盘
        let _s = write_test_lock();
        let (lib, index_db, g1, _g2) = mk_writable_md_lib("md-validate");
        let zp = lib.join("工作/笔记一.zip");
        let before = std::fs::read(&zp).unwrap();
        assert!(save_note_md(&lib, &index_db, &g1, "  \n ").is_err());
        assert!(save_note_md(&lib, &index_db, &g1, "见 wiznote://g/x").is_err());
        assert_eq!(std::fs::read(&zp).unwrap(), before, "被拒的写不得触碰 zip");
        // 只警告的两种情况
        let w = validate_note_md("正文 <script>x=1</script>\n").unwrap();
        assert!(w.iter().any(|s| s.contains("<script>")), "{w:?}");
        let w = validate_note_md("![](index_files/a.png)\n").unwrap();
        assert!(w.iter().any(|s| s.contains("抽不出文本")), "{w:?}");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    // ---- 新建笔记（origin=local）----

    /// 新建主链路：md 包落盘（无 BOM、无 index.html）→ 清单行（local/markdown/md、
    /// 新行置脏）→ meta.revision 不动 → 单篇索引可检索。
    #[test]
    fn test_create_note_happy_path() {
        let _s = write_test_lock();
        let (lib, index_db, _g1, _g2) = mk_writable_md_lib("create-happy");
        let rev_before = {
            let c = manifest::open_and_migrate(&lib).unwrap();
            manifest::current_revision(&c)
        };

        let md = "# 我的新笔记\n\n关键词 createword\n";
        let rep = create_note(&lib, &index_db, "我的新笔记", "/工作/", md).unwrap();
        assert_eq!(rep.op, OP_CREATE);
        assert!(rep.index_updated, "索引应随建更新: {:?}", rep.warnings);
        assert_eq!(rep.revision, 0, "新行 revision 从 0 起（行级 revision 记本地写次数）");
        assert_eq!(rep.title, "我的新笔记");
        // guid：无花括号、uuid v4 形态
        assert_eq!(rep.guid.len(), 36, "guid 应为 36 位连字符形：{}", rep.guid);
        assert!(!rep.guid.contains('{'));
        // 落盘：note.md 逐字节等于传入 md（无 BOM），没有 index.html
        let zp = lib.join(&rep.exported_path);
        assert!(zp.is_file(), "落地文件应在 {}", rep.exported_path);
        let raw = zip_entry(&zp, crate::md::NOTE_MD);
        assert!(!raw.starts_with(&[0xEF, 0xBB, 0xBF]), "md 包正文不得带 BOM");
        assert_eq!(String::from_utf8(raw).unwrap(), md, "正文应逐字节等于传入 md");
        assert_eq!(zip_entry_names(&zp), vec![crate::md::NOTE_MD.to_string()]);
        assert_eq!(rep.exported_md5, manifest::md5_file(&zp).unwrap());

        // 清单行：origin/content_format/export_mode + 脏闩 + 时间口径
        let conn = manifest::open_and_migrate(&lib).unwrap();
        let row = load_note_row(&conn, &rep.guid).unwrap();
        assert_eq!(row.origin, manifest::ORIGIN_LOCAL, "新建行必须标 local");
        assert_eq!(row.content_format, manifest::FORMAT_MARKDOWN);
        assert_eq!(row.export_mode, "md");
        assert_eq!(row.location, "/工作/");
        assert_eq!(row.created, row.data_modified, "新建行两者同为创建时刻");
        let (info, data): (i64, i64) = conn
            .query_row(
                "SELECT dirty_info, dirty_data FROM note WHERE guid = ?1",
                [&rep.guid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((info, data), (1, 1), "新行按定义置脏（上行才会带走）");
        assert_eq!(
            manifest::current_revision(&conn),
            rev_before,
            "库内写路径不得铸版（meta.revision 不动）"
        );
        assert!(
            manifest::check_invariants(&conn, &lib).unwrap().is_empty(),
            "建后清单自检必须零告警"
        );

        // 索引：新词立即可检索
        assert_eq!(fts_hits(&index_db, "createword"), 1, "新笔记应立即可检索");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// 新建 → 编辑 → 删除 → 恢复 全链路：行字段与 zip 全程等值（新建行也要能走完整生命周期）。
    #[test]
    fn test_create_note_lifecycle_roundtrip() {
        let _s = write_test_lock();
        let (lib, index_db, _g1, _g2) = mk_writable_md_lib("create-cycle");
        let rep = create_note(&lib, &index_db, "生命周期", "/临时/", "# 生命周期\n\n初稿\n").unwrap();
        let g = rep.guid.clone();

        // 编辑：行级 revision +1
        let rep2 = save_note_md(&lib, &index_db, &g, "# 生命周期\n\n改稿 editword\n").unwrap();
        assert_eq!(rep2.revision, 1);
        assert_eq!(fts_hits(&index_db, "editword"), 1);

        // 删除 → 墓碑；恢复 → 行还原（origin 仍是 local，zip 字节不变）
        let _rep3 = delete_note(&lib, &index_db, &g).unwrap();
        assert!(load_note_row(&manifest::open_and_migrate(&lib).unwrap(), &g).is_err());
        let rep4 = restore_note(&lib, &index_db, &g).unwrap();
        assert_eq!(rep4.op, OP_RESTORE);
        let zp = lib.join(&rep4.exported_path);
        assert_eq!(
            String::from_utf8(zip_entry(&zp, crate::md::NOTE_MD)).unwrap(),
            "# 生命周期\n\n改稿 editword\n",
            "恢复出的正文必须是删除前的版本"
        );
        let conn = manifest::open_and_migrate(&lib).unwrap();
        assert_eq!(load_note_row(&conn, &g).unwrap().origin, manifest::ORIGIN_LOCAL);
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// 输入校验与撞名：空标题/危险 location/空正文/宿主引用全拒且零写入；同名净化不覆盖。
    #[test]
    fn test_create_note_validation_and_collision() {
        let _s = write_test_lock();
        let (lib, index_db, _g1, _g2) = mk_writable_md_lib("create-validate");
        let count_files = || -> usize {
            let n = std::fs::read_dir(&lib).unwrap().count();
            n
        };
        let before = count_files();

        assert!(create_note(&lib, &index_db, "  \n ", "/", "# x\n").unwrap_err().starts_with("EMPTY_TITLE"));
        assert!(create_note(&lib, &index_db, "t", "/../etc/", "# x\n").unwrap_err().starts_with("PATH_UNSAFE"));
        assert!(create_note(&lib, &index_db, "t", "/", "  \n").unwrap_err().starts_with("EMPTY_MD"));
        assert!(create_note(&lib, &index_db, "t", "/", "见 wiznote://g/x").unwrap_err().starts_with("MD_HOST_REF"));
        assert_eq!(count_files(), before, "被拒的新建不得在库里留下任何文件");

        // 与已有篇目同目录同名：净化后自动追加 guid 前 8 位，绝不覆盖
        let r1 = create_note(&lib, &index_db, "笔记一", "/工作/", "# 同名其一 dupword\n").unwrap();
        let r2 = create_note(&lib, &index_db, "笔记一", "/工作/", "# 同名其二\n").unwrap();
        assert_ne!(r1.exported_path, r2.exported_path, "同名两篇必须各得其所");
        assert!(!r2.exported_path.contains("笔记一.zip"), "后建者应带 guid 后缀: {}", r2.exported_path);
        let c = manifest::open_and_migrate(&lib).unwrap();
        assert!(manifest::check_invariants(&c, &lib).unwrap().is_empty());
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// `save_note_body` 自动分派：md 包写 `note.md`、原生包写 `index.html`，调用方无需知道形态
    #[test]
    fn test_save_note_body_dispatches_by_package() {
        let _s = write_test_lock();
        // ① md 包 → 走 md 分支
        let (lib, index_db, g1, _g2) = mk_writable_md_lib("disp-md");
        let rep = save_note_body(&lib, &index_db, &g1, "# 改过了\n").unwrap();
        assert_eq!(rep.op, OP_SAVE_MD);
        let zp = lib.join("工作/笔记一.zip");
        assert_eq!(String::from_utf8(zip_entry(&zp, crate::md::NOTE_MD)).unwrap(), "# 改过了\n");
        assert!(zip_entry(&zp, "index.html").is_empty(), "md 包不该被塞进 index.html");
        std::fs::remove_dir_all(&lib).unwrap();

        // ② 原生包 → 走 html 分支
        let (lib, index_db, g1, _g2) = mk_writable_lib("disp-native");
        let rep = save_note_body(&lib, &index_db, &g1, "<html><body>改过了</body></html>").unwrap();
        assert_eq!(rep.op, OP_SAVE_HTML);
        let zp = lib.join("工作/笔记一.zip");
        let raw = zip_entry(&zp, "index.html");
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]), "原生包 BOM 形态仍须保持");
        assert_eq!(
            String::from_utf8_lossy(&raw[3..]),
            "<html><body>改过了</body></html>"
        );
        assert!(zip_entry(&zp, crate::md::NOTE_MD).is_empty(), "原生包不该被塞进 note.md");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    /// 形态判定只看**包内条目**：清单 export_mode 与包实况相左时，以包为准（写路径一致性）
    #[test]
    fn test_package_body_format_reads_package_not_manifest() {
        let _s = write_test_lock();
        let (lib, _idx, g1, _g2) = mk_writable_md_lib("fmt-truth");
        // 清单写 md、包也是 md → md
        assert_eq!(
            package_body_format(&lib.join("工作/笔记一.zip")).unwrap(),
            crate::zipserve::BodyFormat::Md
        );
        // 把清单标识改成 native（模拟"重导前"的状态）→ 判定仍须是 md（包才是事实）
        let conn = manifest::open_and_migrate(&lib).unwrap();
        manifest::set_meta(&conn, "export_mode", "native").unwrap();
        conn.execute("UPDATE note SET export_mode='native' WHERE guid=?1", [&g1])
            .unwrap();
        drop(conn);
        assert_eq!(
            package_body_format(&lib.join("工作/笔记一.zip")).unwrap(),
            crate::zipserve::BodyFormat::Md
        );
        // 读侧同一判据（经 ZipService）
        let zs = crate::zipserve::ZipService::with_resolver(std::sync::Arc::new(
            LibraryResolver::new(lib.clone()).unwrap(),
        ));
        assert_eq!(zs.body_format(&g1), crate::zipserve::BodyFormat::Md);
        std::fs::remove_dir_all(&lib).unwrap();
    }
}
