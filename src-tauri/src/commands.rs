//! Tauri 命令层（IPC 接口）—— 核心服务与 UI 解耦，二期 PWA/鸿蒙可复用同一接口

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::config::{save_settings, Settings};
use crate::export::{export_folder, export_note_single_html, export_note_zip, ExportContext, ExportReport};
use crate::indexer::{build_index, BuildReport};
use crate::search::{self, SearchResponse};
use crate::zipserve::ZipService;

pub struct AppState {
    pub zip: ZipService,
    pub settings: std::sync::Mutex<Settings>,
}

impl AppState {
    pub fn index_db(&self) -> PathBuf {
        crate::config::wiz_home().join("index.db")
    }
    pub fn data_dir(&self) -> Option<PathBuf> {
        self.settings
            .lock()
            .unwrap()
            .data_dir
            .as_ref()
            .map(PathBuf::from)
    }
}

fn index_db_of(state: &AppState) -> Result<PathBuf, String> {
    let p = state.index_db();
    if !p.exists() {
        return Err("索引尚未构建，请先在设置中构建索引".into());
    }
    Ok(p)
}

fn open_index_ro(state: &AppState) -> Result<rusqlite::Connection, String> {
    let p = index_db_of(state)?;
    rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())
}

// ---------- 设置 ----------

#[tauri::command]
pub fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
pub fn update_settings(state: State<AppState>, settings: Settings) -> Result<(), String> {
    save_settings(&settings)?;
    if let Some(dir) = &settings.data_dir {
        state.zip.set_notes_dir(PathBuf::from(dir).join("notes"));
    }
    *state.settings.lock().unwrap() = settings;
    Ok(())
}

#[tauri::command]
pub fn check_index(state: State<AppState>) -> bool {
    state.index_db().exists()
}

// ---------- 索引构建 ----------

#[tauri::command]
pub fn build_index_cmd(app: AppHandle, state: State<AppState>) -> Result<BuildReport, String> {
    let data_dir = state
        .data_dir()
        .ok_or("未设置数据源目录")?;
    let index_dir = crate::config::wiz_home();
    let report = build_index(&data_dir, &index_dir, &|done, total| {
        let _ = app.emit("index-progress", serde_json::json!({"done": done, "total": total}));
    })?;
    Ok(report)
}

// ---------- 目录树（FR-03） ----------

#[derive(Debug, Serialize)]
pub struct TreeNode {
    pub path: String,
    pub name: String,
    pub pos: Option<i64>,
    pub direct_count: i64,
    pub total_count: i64,
    pub has_attachment: bool,
    pub is_system: bool,
    pub children: Vec<TreeNode>,
}

#[tauri::command]
pub fn get_tree(state: State<AppState>) -> Result<Vec<TreeNode>, String> {
    let conn = open_index_ro(&state)?;
    let mut st = conn
        .prepare("SELECT path, name, pos FROM folder")
        .map_err(|e| e.to_string())?;
    let folders: Vec<(String, String, Option<i64>)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    drop(st);

    let mut st = conn
        .prepare("SELECT location, COUNT(*), MAX(has_attachment) FROM note GROUP BY location")
        .map_err(|e| e.to_string())?;
    let direct: std::collections::HashMap<String, (i64, bool)> = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)? > 0),
            ))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    drop(st);

    // 组树
    let mut nodes: std::collections::HashMap<String, TreeNode> = std::collections::HashMap::new();
    for (path, name, pos) in &folders {
        nodes.insert(
            path.clone(),
            TreeNode {
                path: path.clone(),
                name: name.clone(),
                pos: *pos,
                direct_count: 0,
                total_count: 0,
                has_attachment: false,
                is_system: is_system_folder(path),
                children: vec![],
            },
        );
    }
    for (path, (cnt, att)) in &direct {
        if let Some(n) = nodes.get_mut(path) {
            n.direct_count = *cnt;
            n.has_attachment = *att;
        }
    }
    // 递归计数 + 挂父子
    fn rec(path: &str, nodes: &mut std::collections::HashMap<String, TreeNode>) -> (i64, bool) {
        let (mut total, mut att) = {
            let n = nodes.get(path).unwrap();
            (n.direct_count, n.has_attachment)
        };
        let child_paths: Vec<String> = nodes
            .keys()
            .filter(|p| {
                p.as_str() != path
                    && p.starts_with(path)
                    && !p[path.len()..].trim_matches('/').contains('/')
            })
            .cloned()
            .collect();
        for cp in child_paths {
            let (c, a) = rec(&cp, nodes);
            total += c;
            att = att || a;
            if let Some(child) = nodes.remove(&cp) {
                nodes.get_mut(path).unwrap().children.push(child);
            }
        }
        {
            let n = nodes.get_mut(path).unwrap();
            n.total_count = total;
            n.has_attachment = att;
        }
        (total, att)
    }

    // 顶层（parent == ""）
    let mut roots: Vec<String> = nodes
        .keys()
        .filter(|p| {
            let t = p.trim_matches('/');
            !t.contains('/')
        })
        .cloned()
        .collect();
    for r in roots.clone() {
        rec(&r, &mut nodes);
    }
    let mut out: Vec<TreeNode> = Vec::new();
    roots.sort_by_key(|p| nodes.get(p).and_then(|n| n.pos).unwrap_or(i64::MAX));
    for r in roots {
        if let Some(n) = nodes.remove(&r) {
            out.push(n);
        }
    }
    // 排序 children
    fn sort_children(n: &mut TreeNode) {
        n.children.sort_by_key(|c| c.pos.unwrap_or(i64::MAX));
        for c in &mut n.children {
            sort_children(c);
        }
    }
    for n in &mut out {
        sort_children(n);
    }
    Ok(out)
}

fn is_system_folder(path: &str) -> bool {
    matches!(
        path,
        "/My Notes/" | "/My Drafts/" | "/My Journals/" | "/My Sticky Notes/" | "/Deleted Items/"
    )
}

// ---------- 笔记列表（FR-04） ----------

#[derive(Debug, Serialize)]
pub struct NoteItem {
    pub guid: String,
    pub title: String,
    pub location: String,
    pub data_modified: String,
    pub created: String,
    pub has_attachment: bool,
    pub is_webclip: bool,
    pub doc_type: String,
    pub body_text_length: i64,
    pub package_size: i64,
    pub is_empty: bool,
}

#[tauri::command]
pub fn list_notes(
    state: State<AppState>,
    folder: Option<String>,
    sort: Option<String>,
    filter: Option<String>,
) -> Result<Vec<NoteItem>, String> {
    let conn = open_index_ro(&state)?;
    let order = match sort.as_deref() {
        Some("created") => "created DESC",
        Some("title") => "title COLLATE NOCASE ASC",
        Some("size") => "package_size DESC",
        // G5：默认/指定 modified 都走 DT_DATA_MODIFIED，禁止 DT_MODIFIED
        _ => "data_modified DESC",
    };
    let mut sql = format!(
        "SELECT guid, title, location, data_modified, created, has_attachment, url, type, body_text_length, package_size
         FROM note WHERE 1=1"
    );
    let mut params: Vec<String> = Vec::new();
    if let Some(f) = folder.filter(|f| !f.is_empty()) {
        // 只显示当前目录直属笔记（与为知原生一致）；子目录笔记进子目录查看
        sql.push_str(" AND location = ?");
        params.push(f);
    }
    if let Some(kw) = filter.filter(|f| !f.trim().is_empty()) {
        sql.push_str(" AND title LIKE ?");
        params.push(format!("%{}%", kw.trim()));
    }
    sql.push_str(&format!(" ORDER BY {} LIMIT 5000", order));
    let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = st
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(NoteItem {
                guid: r.get(0)?,
                title: r.get(1)?,
                location: r.get(2)?,
                data_modified: r.get(3)?,
                created: r.get(4)?,
                has_attachment: r.get::<_, i64>(5)? > 0,
                is_webclip: r
                    .get::<_, Option<String>>(6)?
                    .map(|u| !u.is_empty())
                    .unwrap_or(false),
                doc_type: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                body_text_length: r.get(8)?,
                package_size: r.get(9)?,
                is_empty: false,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut items: Vec<NoteItem> = rows.flatten().collect();
    for it in &mut items {
        it.is_empty = it.body_text_length < 10; // P2 近空笔记
    }
    Ok(items)
}

// ---------- 笔记详情（FR-05.6 信息栏 + 附件区） ----------

#[derive(Debug, Serialize)]
pub struct AttachmentItem {
    pub file_path: String,
    pub display_name: String,
    pub size: i64,
    pub tier: i64,
    pub source: String,
    pub exists: bool,
    pub origin: String, // "库内记录" / "按文件名推断" / "归属存疑" / "文件未随导出下载"
}

#[derive(Debug, Serialize)]
pub struct NoteDetail {
    pub guid: String,
    pub title: String,
    pub location: String,
    pub url: Option<String>,
    pub created: String,
    pub data_modified: String,
    pub package_size: i64,
    pub body_text_length: i64,
    pub attachments: Vec<AttachmentItem>,
}

#[tauri::command]
pub fn get_note_detail(state: State<AppState>, guid: String) -> Result<NoteDetail, String> {
    let conn = open_index_ro(&state)?;
    let mut st = conn
        .prepare(
            "SELECT guid, title, location, url, created, data_modified, package_size, body_text_length
             FROM note WHERE guid = ?1",
        )
        .map_err(|e| e.to_string())?;
    let mut detail = st
        .query_row([&guid], |r| {
            Ok(NoteDetail {
                guid: r.get(0)?,
                title: r.get(1)?,
                location: r.get(2)?,
                url: r.get::<_, Option<String>>(3)?,
                created: r.get(4)?,
                data_modified: r.get(5)?,
                package_size: r.get(6)?,
                body_text_length: r.get(7)?,
                attachments: vec![],
            })
        })
        .map_err(|e| e.to_string())?;
    drop(st);

    // 附件：直挂（tier1/2 + db-missing）+ Tier3 多归属
    let mut st = conn
        .prepare(
            "SELECT file_path, display_name, size, tier, source, document_guid FROM attachment WHERE document_guid = ?1
             UNION ALL
             SELECT a.file_path, a.display_name, a.size, a.tier, a.source, NULL
             FROM attachment a JOIN attachment_doc d ON a.file_path = d.file_path
             WHERE d.document_guid = ?1
             ORDER BY display_name, file_path",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([&guid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows.flatten() {
        let (fp, name, size, tier, source) = row;
        let exists = if fp.starts_with("db-missing:") {
            false
        } else {
            PathBuf::new().join(&fp).is_file()
        };
        let origin = match (tier, source.as_str()) {
            (0, "db-missing") => "文件未随导出下载".to_string(),
            (1, _) => "库内记录".to_string(),
            (2, _) => "按文件名推断".to_string(),
            (3, _) => "归属存疑".to_string(),
            _ => String::new(),
        };
        detail.attachments.push(AttachmentItem {
            file_path: fp,
            display_name: name,
            size,
            tier,
            source,
            exists,
            origin,
        });
    }
    Ok(detail)
}

// ---------- 附件打开/预览（FR-05.6 / FR-10） ----------

const TEXT_EXTS: &[&str] = &["log", "jtl", "sh", "jmx", "txt", "conf", "ini", "json", "py", "xml", "csv"];

#[derive(Debug, Serialize)]
pub struct AttachmentPreview {
    pub name: String,
    pub size: i64,
    pub is_text: bool,
    pub content: String,
}

#[tauri::command]
pub fn preview_attachment(_state: State<AppState>, file_path: String) -> Result<AttachmentPreview, String> {
    if file_path.starts_with("db-missing:") {
        return Err("文件未随导出下载".into());
    }
    let p = PathBuf::from(&file_path);
    // G2 安全：仅允许 attachments/ 白名单目录
    if !p.is_file() {
        return Err("文件不存在".into());
    }
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let size = p.metadata().map(|m| m.len() as i64).unwrap_or(0);
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let is_text = TEXT_EXTS.contains(&ext.as_str()) || ext.is_empty();
    if !is_text {
        return Ok(AttachmentPreview {
            name,
            size,
            is_text: false,
            content: String::new(),
        });
    }
    let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
    let mut text = crate::zipserve::decode_utf8_sig(&bytes);
    if ext == "rtf" {
        text = strip_rtf(&text);
    }
    Ok(AttachmentPreview {
        name,
        size,
        is_text: true,
        content: text,
    })
}

/// RTF 降级为纯文本（FR-05.6）
fn strip_rtf(rtf: &str) -> String {
    let re_ctrl = regex::Regex::new(r"\\'[0-9a-fA-F]{2}|\\[a-zA-Z]+-?\d* ?|[{}]").unwrap();
    let cleaned = re_ctrl.replace_all(rtf, "").to_string();
    crate::extract::decode_entities(&cleaned)
}

#[tauri::command]
pub fn open_attachment_external(_state: State<AppState>, file_path: String) -> Result<(), String> {
    if file_path.starts_with("db-missing:") {
        return Err("文件未随导出下载".into());
    }
    tauri_plugin_opener::open_path(file_path, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn reveal_in_finder(_state: State<AppState>, file_path: String) -> Result<(), String> {
    tauri_plugin_opener::reveal_item_in_dir(file_path).map_err(|e| e.to_string())
}

/// 另存为（复制到目标路径，不修改源数据）
#[tauri::command]
pub fn save_attachment_as(_state: State<AppState>, file_path: String, dest: String) -> Result<(), String> {
    if file_path.starts_with("db-missing:") {
        return Err("文件未随导出下载".into());
    }
    std::fs::copy(&file_path, &dest)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_external_url(_state: State<AppState>, url: String) -> Result<(), String> {
    // NFR-3.3：外链一律系统浏览器
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

// ---------- 检索（FR-06） ----------

#[tauri::command]
pub fn search_cmd(
    state: State<AppState>,
    kw: String,
    folder: Option<String>,
) -> Result<SearchResponse, String> {
    let db = index_db_of(&state)?;
    search::search(&db, &kw, folder.as_deref())
}

#[tauri::command]
pub fn get_search_history(state: State<AppState>) -> Result<Vec<(String, i64)>, String> {
    let db = index_db_of(&state)?;
    search::get_history(&db)
}

#[tauri::command]
pub fn clear_search_history(state: State<AppState>) -> Result<(), String> {
    let db = index_db_of(&state)?;
    search::clear_history(&db)
}

// ---------- 未关联附件（FR-10） ----------

#[tauri::command]
pub fn get_unlinked(state: State<AppState>) -> Result<Vec<AttachmentItem>, String> {
    let conn = open_index_ro(&state)?;
    let mut st = conn
        .prepare(
            "SELECT file_path, display_name, size, tier, source FROM attachment
             WHERE tier = 4 AND source = 'unlinked' ORDER BY display_name",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (fp, name, size, tier, source) in rows.flatten() {
        out.push(AttachmentItem {
            exists: PathBuf::new().join(&fp).is_file(),
            file_path: fp,
            display_name: name,
            size,
            tier,
            source,
            origin: String::new(),
        });
    }
    Ok(out)
}

// ---------- 导出（FR-08） ----------

#[tauri::command]
pub fn export_note_zip_cmd(
    app: AppHandle,
    state: State<AppState>,
    guid: String,
    dest: String,
) -> Result<(), String> {
    let detail = get_note_detail(state.clone(), guid.clone())?;
    let ctx = export_ctx(&state)?;
    let items: Vec<(String, String, bool)> = detail
        .attachments
        .iter()
        .map(|a| (a.display_name.clone(), a.file_path.clone(), a.exists))
        .collect();
    let title: String = detail.title.clone();
    export_note_zip(&ctx, &state.zip, &guid, &title, &items, Path::new(&dest))?;
    let _ = app;
    Ok(())
}

#[tauri::command]
pub fn export_note_html_cmd(
    state: State<AppState>,
    guid: String,
    dest: String,
) -> Result<(), String> {
    let detail = get_note_detail(state.clone(), guid.clone())?;
    let ctx = export_ctx(&state)?;
    let items: Vec<(String, String, bool)> = detail
        .attachments
        .iter()
        .map(|a| (a.display_name.clone(), a.file_path.clone(), a.exists))
        .collect();
    let title: String = detail.title.clone();
    export_note_single_html(&ctx, &state.zip, &guid, &title, &items, Path::new(&dest))
}

#[tauri::command]
pub fn export_folder_cmd(
    app: AppHandle,
    state: State<AppState>,
    location: String,
    dest: String,
) -> Result<ExportReport, String> {
    let ctx = export_ctx(&state)?;
    let report = export_folder(&ctx, &state.zip, &location, Path::new(&dest), &|done, total| {
        let _ = app.emit("export-progress", serde_json::json!({"done": done, "total": total}));
    })?;
    Ok(report)
}

fn export_ctx(state: &State<AppState>) -> Result<ExportContext, String> {
    let data_dir = state.data_dir().ok_or("未设置数据源目录")?;
    Ok(ExportContext::new(
        data_dir.join("notes"),
        state.index_db(),
    ))
}

/// 自动探测为知默认数据目录：~/.wiznote/<账号>/data（macOS 隐藏目录，面板中难以导航）。
/// 返回第一个含 index.db 与 notes/ 的候选；无则回退 ~/.wiznote 本身（不存在则 None）。
#[tauri::command]
pub fn detect_wiznote_dir() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let wiznote = PathBuf::from(&home).join(".wiznote");
    if let Ok(accounts) = std::fs::read_dir(&wiznote) {
        let mut cands: Vec<PathBuf> = accounts
            .flatten()
            .map(|e| e.path().join("data"))
            .filter(|d| d.join("index.db").is_file() && d.join("notes").is_dir())
            .collect();
        cands.sort();
        if let Some(first) = cands.first() {
            return Some(first.to_string_lossy().into_owned());
        }
    }
    wiznote.is_dir().then(|| wiznote.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn pick_default_data_dir(state: State<AppState>, dir: String) -> Result<(), String> {
    // 校验目录含 index.db 与 notes/
    let p = PathBuf::from(&dir);
    if !p.join("index.db").is_file() || !p.join("notes").is_dir() {
        return Err("所选目录须包含 index.db 与 notes/".into());
    }
    let mut s = state.settings.lock().unwrap().clone();
    s.data_dir = Some(dir);
    save_settings(&s)?;
    state.zip.set_notes_dir(
        s.data_dir
            .as_ref()
            .map(|d| PathBuf::from(d).join("notes"))
            .unwrap(),
    );
    *state.settings.lock().unwrap() = s;
    Ok(())
}
