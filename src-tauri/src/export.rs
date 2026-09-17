//! 导出逃生舱（FR-08 / T4.0）
//!
//! - 单篇 zip（`index.html` + `index_files/` + 关联附件）
//! - 单篇自包含 HTML（图片/CSS/附件以 `data:` URI 内联）
//! - 按目录批量 / 全库按 202 个目录还原文件树（56 个非法标题净化 + 重名冲突处理）
//! - 附件随导出带出：Tier1/2/3 → 同级 `attachments/`（剥 `{GUID}` 前缀，同名加后缀区分）；
//!   Tier4 → 根级 `_unlinked_attachments/`；导出 HTML 中附件区为**相对链接**
//! - 导出 HTML 必须已物化代码块（`textarea` → 静态 `<pre><code>`）
//! - 导出全程不修改源数据（G1）

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::extract::{
    export_name, html_escape, inject_before_body_close, materialize_code_blocks,
    sanitize_location, sanitize_title,
};
use crate::manifest;
use crate::zipserve::ZipService;

regex_of!(re_img, r#"(?i)(<img[^>]*\ssrc=")(index_files/[^"]+)(")"#);
regex_of!(re_link, r#"(?i)(<link[^>]*\shref=")(index_files/[^"]+)("[^>]*>)"#);
regex_of!(re_css_url, r#"url\((['"]?)([^'")]+)(['"]?)\)"#);
// 瘦身引用判定（FR-02）：index.html 内对 `index_files/` 资源的引用（口径同 tmp_tools/probe2.py）
regex_of!(re_res_ref, r#"index_files/([^"')\s>]+)"#);

pub struct ExportContext {
    pub notes_dir: PathBuf,
    pub index_db: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ExportAttachment {
    /// 显示名（已剥离 `{GUID}` 前缀）
    pub display_name: String,
    /// 源文件绝对路径；`db-missing:` 前缀 = 库内有记录但文件未随导出下载
    pub src: String,
    pub size: i64,
}

impl ExportAttachment {
    pub fn missing(&self) -> bool {
        self.src.starts_with("db-missing:")
    }
}

#[derive(Debug, serde::Serialize)]
pub struct ExportReport {
    pub notes_exported: usize,
    pub attachments_exported: usize,
    /// 库内有记录但文件缺失、未能带出的附件数
    pub attachments_missing: usize,
    pub folders_exported: usize,
    /// 物化进导出文件的代码块数
    pub code_blocks_materialized: usize,
    pub skipped: Vec<String>,
    pub elapsed_ms: u128,
}

impl ExportContext {
    pub fn new(notes_dir: PathBuf, index_db: PathBuf) -> Self {
        Self { notes_dir, index_db }
    }
}

pub(crate) fn open_index_ro(ctx: &ExportContext) -> Result<Connection, String> {
    Connection::open_with_flags(&ctx.index_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())
}

/// 附件区 HTML 片段。`links` 与传入附件同序：href（空串 = 不可打开）
fn attachments_block(items: &[(ExportAttachment, String)]) -> String {
    // 脱离本软件后仍能点开，故样式全部内联，不引外部资源
    let mut rows = String::new();
    for (att, href) in items {
        let size = if att.size > 0 {
            format!(" <span style=\"color:#999;\">（{}）</span>", human_size(att.size))
        } else {
            String::new()
        };
        let row = if href.is_empty() {
            format!(
                "<li><span style=\"color:#999;\">📎 {}{}（文件未随导出下载）</span></li>",
                html_escape(&att.display_name),
                size
            )
        } else {
            format!(
                "<li><a href=\"{}\" style=\"color:#2b6cb0;\">📎 {}{}</a></li>",
                html_escape(href),
                html_escape(&att.display_name),
                size
            )
        };
        rows.push_str(&row);
    }
    format!(
        r#"<div style="border-top:1px solid #ddd;margin-top:2em;padding-top:1em;font-family:sans-serif;">
<h3 style="font-size:14px;color:#666;margin:0 0 .5em;">附件（{}）</h3>
<ul style="list-style:none;padding:0;margin:0;font-size:13px;">{}</ul>
</div>"#,
        items.len(),
        rows
    )
}

fn human_size(bytes: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// 读取笔记 HTML 并物化代码块，返回 (html, 物化块数)
fn materialized_html(zip_svc: &ZipService, guid: &str) -> Result<(String, usize), String> {
    let html = zip_svc.read_index_html(guid).map_err(|e| e.message())?;
    let (html, n) = materialize_code_blocks(&html);
    Ok((html, n))
}

/// zip 条目名落地前的防护：拒绝穿越与绝对路径（源数据虽可信，导出路径必须可控）
fn safe_entry_name(entry: &str) -> Option<String> {
    let normalized = entry.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    ZipService::sanitize_entry_path(&normalized).ok()
}

fn read_file_bytes(path: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut buf))
        .map(|_| buf)
        .map_err(|e| format!("{}: {}", path, e))
}

// ---------------------------------------------------------------- 单篇 zip

/// 单篇导出为 zip：`index.html` + `index_files/` + `attachments/`，双击即可阅读
pub fn export_note_zip(
    _ctx: &ExportContext,
    zip_svc: &ZipService,
    guid: &str,
    attachments: &[ExportAttachment],
    dest_zip: &Path,
) -> Result<ExportReport, String> {
    let t0 = std::time::Instant::now();
    let file = File::create(dest_zip).map_err(|e| e.to_string())?;
    let mut zw = ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut skipped = Vec::new();

    let (mut html, mats) = materialized_html(zip_svc, guid)?;
    let mut used: HashSet<String> = HashSet::new();
    let mut items: Vec<(ExportAttachment, String)> = Vec::new();
    let mut payload: Vec<(String, Vec<u8>)> = Vec::new(); // (zip 内路径, 字节)
    let mut missing = 0usize;
    for att in attachments {
        if att.missing() || Path::new(&att.src).metadata().is_err() {
            missing += 1;
            items.push((att.clone(), String::new()));
            continue;
        }
        let out_name = unique_name(&sanitize_title(&att.display_name), &mut used);
        match read_file_bytes(&att.src) {
            Ok(bytes) => {
                payload.push((format!("attachments/{out_name}"), bytes));
                items.push((att.clone(), format!("attachments/{out_name}")));
            }
            Err(e) => {
                skipped.push(e);
                missing += 1;
                items.push((att.clone(), String::new()));
            }
        }
    }
    html = inject_before_body_close(&html, &attachments_block(&items));

    zw.start_file("index.html", opts).map_err(|e| e.to_string())?;
    zw.write_all(html.as_bytes()).map_err(|e| e.to_string())?;

    for entry in zip_svc.list_entries(guid).map_err(|e| e.message())? {
        if entry == "index.html" {
            continue;
        }
        let Some(name) = safe_entry_name(&entry) else {
            skipped.push(format!("{}: 非法条目名 {}", guid, entry));
            continue;
        };
        match zip_svc.read_entry(guid, &entry) {
            Ok(bytes) => {
                zw.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
                zw.write_all(&bytes).map_err(|e| e.to_string())?;
            }
            Err(e) => skipped.push(format!("{}: {} ({})", guid, entry, e.message())),
        }
    }
    let att_count = payload.len();
    for (name, bytes) in payload {
        zw.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
        zw.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    zw.finish().map_err(|e| e.to_string())?;

    Ok(ExportReport {
        notes_exported: 1,
        attachments_exported: att_count,
        attachments_missing: missing,
        folders_exported: 0,
        code_blocks_materialized: mats,
        skipped,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

// ------------------------------------------------------ 单篇自包含 HTML

/// 单篇导出为自包含 HTML：图片/CSS 以 `data:` URI 内联，附件同样内联为可点开的链接
pub fn export_note_single_html(
    _ctx: &ExportContext,
    zip_svc: &ZipService,
    guid: &str,
    attachments: &[ExportAttachment],
    dest_html: &Path,
) -> Result<ExportReport, String> {
    let t0 = std::time::Instant::now();
    let (html, mats) = materialized_html(zip_svc, guid)?;
    let mut skipped = Vec::new();

    // <img src="index_files/..."> → data URI
    let re_img = re_img();
    let html = re_img.replace_all(&html, |c: &regex::Captures| match inline_as_data_uri(zip_svc, guid, &c[2]) {
        Some(uri) => format!("{}{}{}", &c[1], uri, &c[3]),
        None => c[0].to_string(),
    });
    // <link href="index_files/*.css"> → <style>（其内 url() 亦转 data URI）
    let re_link = re_link();
    let html = re_link.replace_all(&html, |c: &regex::Captures| match inline_css(zip_svc, guid, &c[2]) {
        Some(css) => format!("<style>{}</style>", css),
        None => c[0].to_string(),
    });

    let mut missing = 0usize;
    let items: Vec<(ExportAttachment, String)> = attachments
        .iter()
        .map(|att| {
            if att.missing() {
                missing += 1;
                skipped.push(format!("附件未随导出下载: {}", att.display_name));
                return (att.clone(), String::new());
            }
            match read_file_bytes(&att.src) {
                Ok(bytes) => {
                    let mime = crate::zipserve::content_type(&att.display_name)
                        .split(';')
                        .next()
                        .unwrap_or("application/octet-stream");
                    (
                        att.clone(),
                        format!("data:{};base64,{}", mime, base64_encode(&bytes)),
                    )
                }
                Err(e) => {
                    missing += 1;
                    skipped.push(format!("附件内联失败 {}（{e}）", att.display_name));
                    (att.clone(), String::new())
                }
            }
        })
        .collect();
    let att_count = items.iter().filter(|(_, h)| !h.is_empty()).count();
    let html = inject_before_body_close(&html, &attachments_block(&items));

    std::fs::write(dest_html, html.as_bytes()).map_err(|e| e.to_string())?;
    Ok(ExportReport {
        notes_exported: 1,
        attachments_exported: att_count,
        attachments_missing: missing,
        folders_exported: 0,
        code_blocks_materialized: mats,
        skipped,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

fn inline_as_data_uri(zip_svc: &ZipService, guid: &str, path: &str) -> Option<String> {
    let bytes = zip_svc.read_entry(guid, path).ok()?;
    let mime = crate::zipserve::content_type(path)
        .split(';')
        .next()
        .unwrap_or("application/octet-stream");
    Some(format!("data:{};base64,{}", mime, base64_encode(&bytes)))
}

fn inline_css(zip_svc: &ZipService, guid: &str, path: &str) -> Option<String> {
    let bytes = zip_svc.read_entry(guid, path).ok()?;
    let css = crate::zipserve::decode_utf8_sig(&bytes);
    // 重写 css 内相对 url(...) 为 data: URI（同目录相对）
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let re_url = re_css_url();
    let out = re_url.replace_all(&css, |c: &regex::Captures| {
        let target = &c[2];
        if target.starts_with("data:") || target.starts_with("http") {
            return c[0].to_string();
        }
        let full = if dir.is_empty() {
            target.to_string()
        } else {
            format!("{}/{}", dir, target)
        };
        match inline_as_data_uri(zip_svc, guid, &full) {
            Some(uri) => format!("url({})", uri),
            None => c[0].to_string(),
        }
    });
    Some(out.to_string())
}

// ------------------------------------------------------ 目录 / 全库

/// 按目录 / 全库导出：还原目录层级为真实文件夹结构。
/// `location` 为空表示全库（此时 Tier 4 未关联附件导出到根级 `_unlinked_attachments/`）。
pub fn export_folder(
    ctx: &ExportContext,
    zip_svc: &ZipService,
    location: &str,
    dest_root: &Path,
    progress: &dyn Fn(usize, usize),
) -> Result<ExportReport, String> {
    let t0 = std::time::Instant::now();
    std::fs::create_dir_all(dest_root).map_err(|e| e.to_string())?;
    let conn = open_index_ro(ctx)?;

    let notes = load_notes(&conn, location)?;
    let atts_by_doc = load_attachment_map(&conn)?;

    let mut skipped = Vec::new();
    let mut notes_exported = 0usize;
    let mut attachments_exported = 0usize;
    let mut attachments_missing = 0usize;
    let mut mats = 0usize;
    let total = notes.len();

    // 每个目录内重名去重用
    let mut used_names_by_dir: HashMap<String, HashSet<String>> = HashMap::new();
    let mut made_dirs: HashSet<PathBuf> = HashSet::new();

    for (i, note) in notes.iter().enumerate() {
        // P3：目录名同样可能含文件系统非法字符，逐段净化
        let dir_rel = sanitize_location(&note.location).trim_matches('/').to_string();
        let dir_abs = dest_root.join(&dir_rel);
        if dir_abs.as_path() != dest_root && made_dirs.insert(dir_abs.clone()) {
            std::fs::create_dir_all(&dir_abs).map_err(|e| e.to_string())?;
        }
        let used = used_names_by_dir.entry(dir_rel).or_default();
        let mut note_dir = dir_abs.join(export_name(&note.title, &note.guid, used));
        if made_dirs.insert(note_dir.clone()) {
            std::fs::create_dir_all(&note_dir).map_err(|e| e.to_string())?;
        }
        // 同名笔记（净化后仍撞名）已由 export_name 追加 GUID 前 8 位，正常不会走到这里

        let list = atts_by_doc.get(&note.guid).cloned().unwrap_or_default();
        match export_one_to_dir(zip_svc, &note.guid, &list, &mut note_dir) {
            Ok(r) => {
                notes_exported += 1;
                attachments_exported += r.0;
                attachments_missing += r.1;
                mats += r.2;
                skipped.extend(r.3);
            }
            Err(e) => {
                skipped.push(format!("{}: {}", note.guid, e));
                // 失败的这篇不占用名字
                if let Some(used) = used_names_by_dir.get_mut(
                    &sanitize_location(&note.location).trim_matches('/').to_string(),
                ) {
                    let base = sanitize_title(&note.title);
                    used.remove(&base);
                }
            }
        }
        if (i + 1) % 50 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }

    // Tier4 未关联附件 → 根级 _unlinked_attachments/（仅全库导出时）
    if location.is_empty() {
        let unlinked = load_unlinked(&conn)?;
        if !unlinked.is_empty() {
            let ua = dest_root.join("_unlinked_attachments");
            std::fs::create_dir_all(&ua).map_err(|e| e.to_string())?;
            let mut used: HashSet<String> = HashSet::new();
            for att in unlinked {
                let out = ua.join(unique_name(&sanitize_title(&att.display_name), &mut used));
                match read_file_bytes(&att.src) {
                    Ok(bytes) => {
                        std::fs::write(out, bytes).map_err(|e| e.to_string())?;
                        attachments_exported += 1;
                    }
                    Err(e) => skipped.push(e),
                }
            }
        }
    }

    let folders_exported = made_dirs.len();
    Ok(ExportReport {
        notes_exported,
        attachments_exported,
        attachments_missing,
        folders_exported,
        code_blocks_materialized: mats,
        skipped,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

/// 把一篇笔记写入 `<note_dir>/index.html` + `index_files/` + `attachments/`
/// 返回 (带出附件数, 缺失附件数, 物化代码块数, 跳过项)
fn export_one_to_dir(
    zip_svc: &ZipService,
    guid: &str,
    list: &[ExportAttachment],
    note_dir: &Path,
) -> Result<(usize, usize, usize, Vec<String>), String> {
    let (mut html, mats) = materialized_html(zip_svc, guid)?;
    let mut skipped = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut items: Vec<(ExportAttachment, String)> = Vec::new();
    let mut missing = 0usize;
    let mut copied = 0usize;
    let att_dir = note_dir.join("attachments");
    for att in list {
        if att.missing() {
            missing += 1;
            items.push((att.clone(), String::new()));
            continue;
        }
        match read_file_bytes(&att.src) {
            Ok(bytes) => {
                std::fs::create_dir_all(&att_dir).map_err(|e| e.to_string())?;
                let out_name = unique_name(&sanitize_title(&att.display_name), &mut used);
                std::fs::write(att_dir.join(&out_name), bytes).map_err(|e| e.to_string())?;
                copied += 1;
                items.push((att.clone(), format!("attachments/{out_name}")));
            }
            Err(e) => {
                skipped.push(e);
                missing += 1;
                items.push((att.clone(), String::new()));
            }
        }
    }
    html = inject_before_body_close(&html, &attachments_block(&items));
    std::fs::write(note_dir.join("index.html"), html.as_bytes()).map_err(|e| e.to_string())?;

    for entry in zip_svc.list_entries(guid).map_err(|e| e.message())? {
        if entry == "index.html" {
            continue;
        }
        let Some(name) = safe_entry_name(&entry) else {
            skipped.push(format!("{}: 非法条目名 {}", guid, entry));
            continue;
        };
        let bytes = match zip_svc.read_entry(guid, &entry) {
            Ok(b) => b,
            Err(e) => {
                skipped.push(format!("{}: {} ({})", guid, entry, e.message()));
                continue;
            }
        };
        let out = note_dir.join(&name);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(out, bytes).map_err(|e| e.to_string())?;
    }
    Ok((copied, missing, mats, skipped))
}

// ------------------------------------- 每份笔记 zip（FR-08.1 批量形态 / FR-02 存储瘦身）

/// 每份笔记导出为一个 zip 的结果：通用报告之上补充瘦身统计与清单计数（§5）
#[derive(Debug, serde::Serialize)]
pub struct FolderZipExportReport {
    #[serde(flatten)]
    pub report: ExportReport,
    /// 是否执行了 FR-02 存储瘦身
    pub slim: bool,
    /// 瘦身删除的冗余资源文件总数
    pub slim_files_removed: u64,
    /// 瘦身删除的冗余资源字节总数（解压后口径）
    pub slim_bytes_removed: u64,
    /// 瘦身报告 CSV 路径（slim=false 时为 None）
    pub slim_report_path: Option<String>,
    /// 同步清单 export.db 路径（§5）
    pub manifest_path: Option<String>,
    /// 本轮新增（源库新笔记）
    pub notes_added: usize,
    /// 本轮重导（源变化/改名/文件缺失）
    pub notes_reexported: usize,
    /// 本轮复用（未变，零重写，§6.2 第一级）
    pub notes_reused: usize,
    /// 本轮转入墓碑（源库已消失，§6.3）
    pub notes_removed: usize,
    /// 清单不变量自检警告（§5.5，供抽查，不中断导出）
    pub manifest_warnings: Vec<String>,
}

/// 单篇瘦身的体积统计（解压后口径；pub(crate) 供 manifest::rebuild 重算）
pub(crate) struct SlimStat {
    pub(crate) orig_files: u64,
    pub(crate) kept_files: u64,
    pub(crate) orig_bytes: u64,
    pub(crate) kept_bytes: u64,
}

/// 每份笔记导出为一个 zip，输出按目录层级还原：
/// - slim = false：逐篇**字节级原样复制**为知原生 zip 包（仅重命名为「净化标题.zip」，继承为知格式）
/// - slim = true：FR-02 存储瘦身 —— 重建 zip，仅保留 index.html 与被其引用的 index_files/ 条目，
///   并在目标根目录生成「瘦身报告.csv」（每篇删掉多少文件/字节，供抽查）。默认关闭，调用方须二次确认
/// 同时维护导出根同步清单 `export.db`（manifest.rs，§5）：
/// - 未变篇目（data_modified/package_size/模式/落地路径均未变且文件在）直接**复用现有 zip，零重写**（§6.2 第一级）
/// - 标题/目录变更的篇目重导并按 Q4 直接删除旧路径文件
/// - 源库已消失的篇目转入墓碑（只记录，不删文件，§6.3）
/// 注意：原生 zip 不含附件（继承为知格式），附件随「通用文件」导出（[`export_folder`]）走；全程不改源数据。
pub fn export_folder_zips(
    ctx: &ExportContext,
    location: &str,
    dest_root: &Path,
    slim: bool,
    progress: &dyn Fn(usize, usize),
) -> Result<FolderZipExportReport, String> {
    let t0 = std::time::Instant::now();
    std::fs::create_dir_all(dest_root).map_err(|e| e.to_string())?;
    let conn = open_index_ro(ctx)?;
    let notes = load_notes(&conn, location)?;
    let plan = plan_zip_paths(&notes);
    let mode = if slim { "slim" } else { "native" };

    // 同步清单（§5）：建库载入旧行，供增量复用判定（§6.2 第一级）
    let mconn = manifest::open_or_create(dest_root)?;
    let prev_rows = manifest::load_notes(&mconn)?;

    let mut skipped = Vec::new();
    let mut notes_exported = 0usize;
    let mut notes_added = 0usize;
    let mut notes_reexported = 0usize;
    let mut notes_reused = 0usize;
    let mut slim_files_removed = 0u64;
    let mut slim_bytes_removed = 0u64;

    // 瘦身报告（FR-02.4）：BOM 让 Excel 正确识别 UTF-8；复用篇目的统计从清单行带出
    let mut csv: Option<(PathBuf, File)> = if slim {
        let p = dest_root.join("瘦身报告.csv");
        let mut f = File::create(&p).map_err(|e| e.to_string())?;
        f.write_all(
            "\u{feff}标题,目录,原文件数,保留文件数,删除文件数,原字节,保留字节,删除字节\n".as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        Some((p, f))
    } else {
        None
    };

    let mut made_dirs: HashSet<PathBuf> = HashSet::new();
    let total = notes.len();

    for (i, (n, p)) in notes.iter().zip(&plan).enumerate() {
        // 目录已由 plan_zip_paths 逐段净化；根级（空目录）不建目录
        let dir_abs = if p.dir_rel.is_empty() {
            dest_root.to_path_buf()
        } else {
            let d = dest_root.join(&p.dir_rel);
            if made_dirs.insert(d.clone()) {
                std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
            }
            d
        };
        let dest = dir_abs.join(&p.zip_name);
        // 源包：notes/{GUID}（带花括号、无扩展名）
        let src = ctx.notes_dir.join(format!("{{{}}}", n.guid));

        // 增量复用判定（§6.2 第一级）：源字段/模式/落地路径均未变且文件在 → 零重写
        let prev = prev_rows.get(&n.guid);
        let unchanged = matches!(&prev, Some(o)
            if o.data_modified == n.data_modified
                && o.package_size == n.package_size
                && o.export_mode == mode
                && o.exported_path == p.rel_path
                && dest.is_file());

        if unchanged {
            notes_reused += 1;
            notes_exported += 1;
            let o = prev.unwrap();
            if slim {
                let (orig_f, kept_f, orig_b, kept_b) = slim_stat_from_row(o);
                write_slim_csv_row(&mut csv, &n.title, &n.location, orig_f, kept_f, orig_b, kept_b);
            }
            // 源库元数据仍刷新（url/标题等可能变了）；exported_* 与 exported_at 保持不变
            manifest::upsert_note(
                &mconn,
                &manifest::ManifestNote {
                    guid: n.guid.clone(),
                    title: n.title.clone(),
                    location: n.location.clone(),
                    created: n.created.clone(),
                    data_modified: n.data_modified.clone(),
                    url: n.url.clone(),
                    doc_type: n.doc_type.clone(),
                    has_attachment: n.has_attachment,
                    package_size: n.package_size,
                    exported_path: o.exported_path.clone(),
                    exported_size: o.exported_size,
                    exported_md5: o.exported_md5.clone(),
                    export_mode: o.export_mode.clone(),
                    entry_count: o.entry_count,
                    removed_files: o.removed_files,
                    removed_bytes: o.removed_bytes,
                    kept_bytes: o.kept_bytes,
                    exported_at: o.exported_at.clone(),
                },
            )?;
        } else {
            // 标题/目录变更遗留旧文件：Q4 直接删除（导出目录是派生物）
            if let Some(o) = prev {
                if o.exported_path != p.rel_path {
                    let old = dest_root.join(&o.exported_path);
                    if old.is_file() {
                        let _ = std::fs::remove_file(&old);
                    }
                }
            }
            let write = || -> Result<(SlimStat, String, i64), String> {
                if slim {
                    let stat = slim_build_zip(&src, &dest)?;
                    // slim：重建后算 MD5（§9.1 Q2）
                    let md5 = manifest::md5_file(&dest)?;
                    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                    Ok((stat, md5, size as i64))
                } else {
                    // native：边拷边算 MD5，不做二次读盘（§9.1 Q2）
                    let (len, md5) = manifest::copy_with_md5(&src, &dest)?;
                    Ok((
                        SlimStat { orig_files: 0, kept_files: 0, orig_bytes: 0, kept_bytes: 0 },
                        md5,
                        len as i64,
                    ))
                }
            };
            match write() {
                Ok((stat, md5, size)) => {
                    notes_exported += 1;
                    if prev.is_some() {
                        notes_reexported += 1;
                    } else {
                        notes_added += 1;
                    }
                    if slim {
                        let removed_files = stat.orig_files.saturating_sub(stat.kept_files);
                        let removed_bytes = stat.orig_bytes.saturating_sub(stat.kept_bytes);
                        slim_files_removed += removed_files;
                        slim_bytes_removed += removed_bytes;
                        write_slim_csv_row(
                            &mut csv,
                            &n.title,
                            &n.location,
                            stat.orig_files,
                            stat.kept_files,
                            stat.orig_bytes,
                            stat.kept_bytes,
                        );
                    }
                    // exported_at 取落地文件 mtime：与重建入口（manifest::rebuild）同源，保证重建结果逐字段一致
                    let exported_at = std::fs::metadata(&dest)
                        .and_then(|m| m.modified())
                        .map(manifest::format_utc)
                        .unwrap_or_else(|_| manifest::format_utc(std::time::SystemTime::now()));
                    manifest::upsert_note(
                        &mconn,
                        &manifest::ManifestNote {
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
                            exported_size: size,
                            exported_md5: md5,
                            export_mode: mode.to_string(),
                            entry_count: slim.then(|| stat.kept_files),
                            removed_files: slim.then(|| stat.orig_files.saturating_sub(stat.kept_files)),
                            removed_bytes: slim.then(|| stat.orig_bytes.saturating_sub(stat.kept_bytes)),
                            kept_bytes: slim.then(|| stat.kept_bytes),
                            exported_at,
                        },
                    )?;
                }
                Err(e) => {
                    skipped.push(format!("{}: {}", n.guid, e));
                    // 注：命名计划已预排，失败篇目不再回退占名（比旧逻辑更确定，后续同名篇照常加后缀）
                }
            }
        }
        if (i + 1) % 50 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }
    let slim_report_path = csv
        .as_ref()
        .map(|(p, _)| p.to_string_lossy().into_owned());
    drop(csv);

    // 墓碑收敛：源库已消失（全库口径）的清单行转 deleted（§6.3，只记录不删文件）
    let all_guids = load_all_guids(&conn)?;
    let now = manifest::format_utc(std::time::SystemTime::now());
    let tombstoned = manifest::reconcile_tombstones(&mconn, &all_guids, &now)?;
    // meta：revision 单调递增（§8，未来多端新旧判定）；源目录与工具版本存档
    manifest::bump_revision(&mconn, &now)?;
    manifest::set_meta(&mconn, "exported_at", &now)?;
    manifest::set_meta(&mconn, "export_mode", mode)?;
    manifest::set_meta(&mconn, "source_data_dir", &ctx.notes_dir.to_string_lossy())?;
    manifest::set_meta(&mconn, "tool_version", env!("CARGO_PKG_VERSION"))?;

    // 不变量自检（§5.5）：警告进报告，不中断导出
    let manifest_warnings = manifest::check_invariants(&mconn, dest_root)?;

    Ok(FolderZipExportReport {
        report: ExportReport {
            notes_exported,
            attachments_exported: 0,
            attachments_missing: 0,
            folders_exported: made_dirs.len(),
            code_blocks_materialized: 0,
            skipped,
            elapsed_ms: t0.elapsed().as_millis(),
        },
        slim,
        slim_files_removed,
        slim_bytes_removed,
        slim_report_path,
        manifest_path: Some(manifest::manifest_path(dest_root).to_string_lossy().into_owned()),
        notes_added,
        notes_reexported,
        notes_reused,
        notes_removed: tombstoned.len(),
        manifest_warnings,
    })
}

/// 从清单行反推瘦身四元组（原文件数，保留文件数，原字节，保留字节）供 CSV 复用行
fn slim_stat_from_row(o: &manifest::ManifestNote) -> (u64, u64, u64, u64) {
    let kept_f = o.entry_count.unwrap_or(0);
    let rb = o.removed_bytes.unwrap_or(0);
    let kb = o.kept_bytes.unwrap_or(0);
    (kept_f + o.removed_files.unwrap_or(0), kept_f, kb + rb, kb)
}

/// 瘦身报告一行（列序：标题,目录,原文件数,保留文件数,删除文件数,原字节,保留字节,删除字节）
fn write_slim_csv_row(
    csv: &mut Option<(PathBuf, File)>,
    title: &str,
    loc: &str,
    orig_files: u64,
    kept_files: u64,
    orig_bytes: u64,
    kept_bytes: u64,
) {
    if let Some((_, f)) = csv.as_mut() {
        let _ = writeln!(
            f,
            "{},{},{},{},{},{},{},{}",
            csv_cell(title),
            csv_cell(loc),
            orig_files,
            kept_files,
            orig_files.saturating_sub(kept_files),
            orig_bytes,
            kept_bytes,
            orig_bytes.saturating_sub(kept_bytes)
        );
    }
}

/// FR-02 瘦身的确定性统计：读源 zip 计算（保留判定与报告口径），不写任何文件。
/// pub(crate) 供 manifest::rebuild 重算统计（导出/重建结果逐字段一致的前提）
pub(crate) fn slim_stat_of(src: &Path) -> Result<(SlimStat, Vec<String>), String> {
    let file = File::open(src).map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    // index.html 原始字节（含 BOM，P9：解码仅用于提取引用）
    let html_bytes = {
        let mut f = ar.by_name("index.html").map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        buf
    };
    let refs = referenced_names(&crate::zipserve::decode_utf8_sig(&html_bytes));

    // 条目清单（名字 + 解压后体积），用于保留判定与瘦身报告统计
    let names: Vec<(String, u64)> = (0..ar.len())
        .filter_map(|i| ar.by_index(i).ok().map(|f| (f.name().to_string(), f.size())))
        .collect();
    let kept: Vec<(String, u64)> = names
        .iter()
        .filter(|(n, _)| entry_referenced(n, &refs))
        .cloned()
        .collect();
    let stat = SlimStat {
        orig_files: names.len() as u64,
        kept_files: kept.len() as u64,
        orig_bytes: names.iter().map(|(_, s)| *s).sum(),
        kept_bytes: kept.iter().map(|(_, s)| *s).sum(),
    };
    Ok((stat, kept.into_iter().map(|(n, _)| n).collect()))
}

/// FR-02 瘦身重建：仅保留 index.html + 被引用的 index_files/ 条目，输出新 zip。
/// 实测预期（需求文档 FR-02）：解压口径 2,428.5 MB → 1,330.5 MB（-45.2%），48,840 → 16,683 个文件
fn slim_build_zip(src: &Path, dest: &Path) -> Result<SlimStat, String> {
    let (stat, kept) = slim_stat_of(src)?;
    let file = File::open(src).map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    // index.html 原始字节（含 BOM，P9：原样写入不回写）
    let html_bytes = {
        let mut f = ar.by_name("index.html").map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        buf
    };

    let out = File::create(dest).map_err(|e| e.to_string())?;
    let mut zw = ZipWriter::new(out);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zw.start_file("index.html", opts).map_err(|e| e.to_string())?;
    zw.write_all(&html_bytes).map_err(|e| e.to_string())?;
    for name in &kept {
        if name == "index.html" {
            continue;
        }
        let mut f = ar.by_name(name).map_err(|e| e.to_string())?;
        // 落地名走 safe_entry_name 防护（P10 怪异后缀原样保留）
        let out_name = safe_entry_name(name).unwrap_or_else(|| name.clone());
        zw.start_file(out_name.as_str(), opts).map_err(|e| e.to_string())?;
        std::io::copy(&mut f, &mut zw).map_err(|e| e.to_string())?;
    }
    zw.finish().map_err(|e| e.to_string())?;
    Ok(stat)
}

/// index.html 引用的资源名集合（`index_files/` 之后的剩余路径）。
/// 兼容 HTML 内 percent 编码与 zip 内原名两种形态（probe2.py 口径 + 编码解码补充）。
/// 实测数据中存在 `src=&quot;index_files/x.png&quot;` 实体引号形态：先在实体定界符处截断，
/// 再把 `&amp;` 还原为 `&`，否则会把实体字符带进文件名导致误判未引用（实测 12 处）
fn referenced_names(html: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for c in re_res_ref().captures_iter(html) {
        let mut raw = c[1].to_string();
        // 实体定界符（&quot;/&#39; 等）说明文件名到此为止，先截断；
        // &amp; 属于文件名本身（文件名含 & 时 HTML 必然这样写），截断后还原
        let cut = raw
            .find("&quot;")
            .or_else(|| raw.find("&#39;"))
            .or_else(|| raw.find("&apos;"));
        if let Some(i) = cut {
            out.insert(raw.clone()); // 原串也入集合（宽松保留，无害）
            raw.truncate(i);
        }
        if raw.contains("&amp;") {
            out.insert(raw.clone());
            raw = raw.replace("&amp;", "&");
        }
        out.insert(raw.clone());
        let decoded = percent_encoding::percent_decode_str(&raw)
            .decode_utf8_lossy()
            .into_owned();
        if decoded != raw {
            out.insert(decoded);
        }
    }
    out
}

/// 条目是否保留：index.html 必留；index_files/ 条目仅限被引用的（FR-02.1）
fn entry_referenced(entry: &str, refs: &HashSet<String>) -> bool {
    if entry == "index.html" {
        return true;
    }
    let Some(rest) = entry.strip_prefix("index_files/") else {
        return false;
    };
    if refs.contains(rest) {
        return true;
    }
    // probe2.py 口径：按文件名匹配（网页剪藏资源基本平铺在 index_files/ 下）
    rest.rsplit('/').next().is_some_and(|b| refs.contains(b))
}

/// CSV 单元格转义：含逗号/引号/换行时加引号并双写内部引号
fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ------------------------------------------------------ 索引读取

/// 导出所需的源库笔记元数据（对照设计文档 §5.2 字段映射：只用 DT_DATA_MODIFIED 等可信字段）
#[derive(Debug, Clone)]
pub struct NoteMeta {
    pub guid: String,
    pub title: String,
    pub location: String,
    pub created: String,
    pub data_modified: String,
    pub url: Option<String>,
    pub doc_type: Option<String>,
    pub has_attachment: bool,
    pub package_size: i64,
}

pub(crate) fn load_notes(conn: &Connection, location: &str) -> Result<Vec<NoteMeta>, String> {
    let sql = if location.is_empty() {
        "SELECT guid, title, location, created, data_modified, url, type, has_attachment, package_size \
         FROM note ORDER BY location, title"
            .to_string()
    } else {
        "SELECT guid, title, location, created, data_modified, url, type, has_attachment, package_size \
         FROM note WHERE location LIKE ?1 ESCAPE '!' ORDER BY location, title"
            .to_string()
    };
    let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let map = |r: &rusqlite::Row| {
        Ok(NoteMeta {
            guid: r.get(0)?,
            title: r.get(1)?,
            location: r.get(2)?,
            created: r.get(3)?,
            data_modified: r.get(4)?,
            url: r.get(5)?,
            doc_type: r.get::<_, Option<String>>(6)?,
            has_attachment: r.get::<_, i64>(7)? > 0,
            package_size: r.get(8)?,
        })
    };
    let rows: Vec<NoteMeta> = if location.is_empty() {
        st.query_map([], map)
            .map_err(|e| e.to_string())?
            .flatten()
            .collect()
    } else {
        st.query_map([like_prefix(location)], map)
            .map_err(|e| e.to_string())?
            .flatten()
            .collect()
    };
    Ok(rows)
}

/// 全库 guid 集合（墓碑收敛的判定口径：目录范围外的行不算消失）
fn load_all_guids(conn: &Connection) -> Result<HashSet<String>, String> {
    let mut st = conn.prepare("SELECT guid FROM note").map_err(|e| e.to_string())?;
    let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 每篇 zip 的落地路径预排（与导出同序同规则：净化 + 重名加 GUID 前 8 位 + zip 占名）。
/// pub(crate) 供 manifest::rebuild 重放命名——保证重建与正向导出产生同一份路径映射
pub(crate) struct PlannedZip {
    pub dir_rel: String,
    pub zip_name: String,
    /// 相对导出根的路径（'/' 分隔）
    pub rel_path: String,
}

pub(crate) fn plan_zip_paths(notes: &[NoteMeta]) -> Vec<PlannedZip> {
    let mut used_by_dir: HashMap<String, HashSet<String>> = HashMap::new();
    notes
        .iter()
        .map(|n| {
            let dir_rel = sanitize_location(&n.location).trim_matches('/').to_string();
            let used = used_by_dir.entry(dir_rel.clone()).or_default();
            let base = export_name(&n.title, &n.guid, used);
            // zip 文件名也占名：避免标题「a」与「a.zip」两篇笔记同盘撞名互相覆盖
            let zip_name = format!("{}.zip", base);
            used.insert(zip_name.clone());
            let rel_path = if dir_rel.is_empty() {
                zip_name.clone()
            } else {
                format!("{}/{}", dir_rel, zip_name)
            };
            PlannedZip { dir_rel, zip_name, rel_path }
        })
        .collect()
}

/// 目录前缀匹配用的 LIKE 模式。
/// 目录名可以含 `_` 与 `%`（实测有 `/分布式文件系统/CEPH/aliyun_hk/`），
/// 不转义则 `_` 退化成单字符通配符，导出范围会扩到同名型的兄弟目录。
fn like_prefix(location: &str) -> String {
    let mut s = String::with_capacity(location.len() + 1);
    for c in location.chars() {
        if matches!(c, '!' | '%' | '_') {
            s.push('!');
        }
        s.push(c);
    }
    s.push('%');
    s
}

/// document_guid → 该笔记的附件（Tier1/2 直挂 + Tier3 多归属，同名的每条都列，不去重 P17）
fn load_attachment_map(conn: &Connection) -> Result<HashMap<String, Vec<ExportAttachment>>, String> {
    let mut out: HashMap<String, Vec<ExportAttachment>> = HashMap::new();
    {
        let mut st = conn
            .prepare("SELECT file_path, display_name, size, document_guid FROM attachment WHERE document_guid IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for (fp, name, size, doc) in rows.flatten() {
            out.entry(doc).or_default().push(ExportAttachment {
                display_name: name,
                src: fp,
                size,
            });
        }
    }
    {
        let mut st = conn
            .prepare(
                "SELECT a.file_path, a.display_name, a.size, d.document_guid
                 FROM attachment a JOIN attachment_doc d ON a.file_path = d.file_path",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for (fp, name, size, doc) in rows.flatten() {
            out.entry(doc).or_default().push(ExportAttachment {
                display_name: name,
                src: fp,
                size,
            });
        }
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| a.display_name.cmp(&b.display_name).then(a.src.cmp(&b.src)));
    }
    Ok(out)
}

/// 单篇笔记的附件清单（口径同 [`load_attachment_map`]：Tier1/2 直挂 + Tier3 多归属，同名不去重）
pub fn note_attachments(ctx: &ExportContext, guid: &str) -> Result<Vec<ExportAttachment>, String> {
    let conn = open_index_ro(ctx)?;
    let mut st = conn
        .prepare(
            "SELECT file_path, display_name, size FROM attachment WHERE document_guid = ?1
             UNION ALL
             SELECT a.file_path, a.display_name, a.size
             FROM attachment a JOIN attachment_doc d ON a.file_path = d.file_path
             WHERE d.document_guid = ?1
             ORDER BY display_name, file_path",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([guid], |r| {
            Ok(ExportAttachment {
                display_name: r.get(1)?,
                src: r.get(0)?,
                size: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 笔记标题（CLI 默认导出文件名用）
pub fn note_title(ctx: &ExportContext, guid: &str) -> String {
    open_index_ro(ctx)
        .ok()
        .and_then(|c| {
            c.query_row("SELECT title FROM note WHERE guid = ?1", [guid], |r| r.get::<_, String>(0))
                .ok()
        })
        .unwrap_or_else(|| guid.to_string())
}

fn load_unlinked(conn: &Connection) -> Result<Vec<ExportAttachment>, String> {
    let mut st = conn
        .prepare("SELECT file_path, display_name, size FROM attachment WHERE tier = 4 AND source = 'unlinked' ORDER BY display_name, file_path")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(ExportAttachment {
                src: r.get(0)?,
                display_name: r.get(1)?,
                size: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

// ------------------------------------------------------ 辅助

fn unique_name(name: &str, used: &mut HashSet<String>) -> String {
    if used.insert(name.to_string()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !name.starts_with('.') => (s.to_string(), format!(".{}", e)),
        _ => (name.to_string(), String::new()),
    };
    let mut i = 2;
    loop {
        let candidate = format!("{}_{}{}", stem, i, ext);
        if used.insert(candidate.clone()) {
            return candidate;
        }
        i += 1;
    }
}

fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TBL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TBL[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_like_prefix_escapes_wildcards() {
        assert_eq!(like_prefix("/a/b/"), "/a/b/%");
        // 目录名里的 `_`、`%`、转义符本身不得当成通配符（导出范围只能按字面目录前缀展开）
        assert_eq!(like_prefix("/CEPH/aliyun_hk/"), "/CEPH/aliyun!_hk/%");
        assert_eq!(like_prefix("/100%/"), "/100!%/%");
        assert_eq!(like_prefix("/a!b/"), "/a!!b/%");
    }

    #[test]
    fn test_unique_name_suffix() {
        let mut used = HashSet::new();
        assert_eq!(unique_name("run.log", &mut used), "run.log");
        assert_eq!(unique_name("run.log", &mut used), "run_2.log");
        assert_eq!(unique_name("run.log", &mut used), "run_3.log");
        // 无扩展名（附件中存在）
        assert_eq!(unique_name("Dockerfile", &mut used), "Dockerfile");
        assert_eq!(unique_name("Dockerfile", &mut used), "Dockerfile_2");
    }

    #[test]
    fn test_safe_entry_name() {
        assert_eq!(safe_entry_name("index_files/a.png").as_deref(), Some("index_files/a.png"));
        assert_eq!(safe_entry_name("../evil"), None);
        assert_eq!(safe_entry_name("/etc/passwd"), None);
        assert_eq!(safe_entry_name("a\\..\\b"), None);
        // P10 怪异但合法的文件名
        assert_eq!(
            safe_entry_name("index_files/x.png;sizingmethod=-crop").as_deref(),
            Some("index_files/x.png;sizingmethod=-crop")
        );
    }

    #[test]
    fn test_referenced_names_and_entry_filter() {
        let html = r#"<img src="index_files/a-b.png"><link href="index_files/x%20y.css">
        <img src="index_files/z.png"><img src="https://elsewhere.com/i.png">"#;
        let refs = referenced_names(html);
        assert!(refs.contains("a-b.png"));
        assert!(refs.contains("x y.css")); // percent 解码后命中
        assert!(refs.contains("x%20y.css")); // 原名也命中（zip 内两种形态都能匹配）
        assert!(refs.contains("z.png"));
        // 实体引号形态（实测 CSDN 剪藏）：&quot; 定界符要截断；&amp; 属于文件名本身要还原
        let html2 = r#"<img src=&quot;index_files/gitcode-key.png&quot;><img src="index_files/a&amp;b.png">"#;
        let refs2 = referenced_names(html2);
        assert!(refs2.contains("gitcode-key.png"));
        assert!(refs2.contains("a&b.png"));
        assert!(!refs2.contains("i.png")); // 远程引用不进集合

        assert!(entry_referenced("index.html", &refs));
        assert!(entry_referenced("index_files/z.png", &refs));
        assert!(entry_referenced("index_files/x y.css", &refs));
        assert!(entry_referenced("index_files/x%20y.css", &refs)); // zip 内原名（含空格）经解码命中
        assert!(!entry_referenced("index_files/unused_font.woff", &refs));
        assert!(!entry_referenced("other/a b.png", &refs));
    }

    #[test]
    fn test_csv_cell() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("说\"话"), "\"说\"\"话\"");
    }

    #[test]
    fn test_base64() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn test_attachments_block() {
        let items = vec![
            (
                ExportAttachment {
                    display_name: "a.log".into(),
                    src: "/x/a.log".into(),
                    size: 2048,
                },
                "attachments/a.log".to_string(),
            ),
            (
                ExportAttachment {
                    display_name: "gone".into(),
                    src: "db-missing:guid".into(),
                    size: 0,
                },
                String::new(),
            ),
        ];
        let html = attachments_block(&items);
        assert!(html.contains("href=\"attachments/a.log\""));
        assert!(html.contains("文件未随导出下载"));
        assert!(html.contains("附件（2）"));
    }
}
