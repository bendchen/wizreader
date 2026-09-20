//! 导出逃生舱（FR-08 / T4.0）
//!
//! - 单篇 zip（`index.html` + `index_files/` + 关联附件）
//! - 批量形态两种（§20 后的库形态，见 [`ZipFormat`]）：
//!   **native** = 源 zip 字节级拷贝（「导出数据」逃生舱，D0 恒 native）；
//!   **md** = 库内 md 包（`note.md` 正文 + `index_files/` 原样条目，M2 起库的形态）
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

/// 读取笔记正文（**形态自适应**，M3/§20.3）并物化代码块，返回 (可阅读 HTML, 物化块数)。
///
/// md 包走 [`ZipService::read_note_document`]（把 `note.md` 渲染成 HTML）；原生包原样取
/// `index.html` —— 两种情况都得到"可直接阅读的 HTML"，与阅读态**同一条渲染口径**。
/// 物化那一步对 md 天然是空操作（`materialize_code_blocks` 只认为知的
/// `wiz-code-container` / 隐藏 textarea，md 渲染产物里没有这些），故不必按形态分叉。
fn materialized_html(zip_svc: &ZipService, guid: &str) -> Result<(String, usize), String> {
    let html = zip_svc
        .read_note_document(guid, guid)
        .map_err(|e| e.message())?;
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

    // 导出物**恒为为知原生形态**（D0：导出只产 native）→ 正文条目必为 `index.html`（上面已写）。
    // 源包的正文条目名则随库内形态而变（md 包是 `note.md`）→ 必须把**源包的**正文条目跳过，
    // 否则 md 库导出的包里会同时躺着 `index.html`（新渲染）与 `note.md`（搬运来的）两份正文。
    let src_body_entry = zip_svc.body_format(guid).entry();
    for entry in zip_svc.list_entries(guid).map_err(|e| e.message())? {
        if entry == src_body_entry {
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

// ------------------------------------- 每份笔记 zip（FR-08.1 批量形态）

/// 导出模式：**恒为 `native`**（D0 全局约束「库必须无损 —— 导出只产 native」，2026-09-17 定稿）。
/// 源 zip **字节级原样拷贝**（[`manifest::copy_with_md5`]），不重写、不增删任何条目；
/// slim（FR-02 存储瘦身）已**整体取消**，其代码路径、UI 入口与报告产物一并移除
/// （论证见 `docs/本地笔记读写实现.md` §4.6）。云端对象键的 `native/` 段是**冻结的协议
/// 字面量**（见 `sync::KEY_NATIVE`），与「格式可选」无关。
pub const EXPORT_MODE: &str = "native";

/// 库内主数据格式标识：`md`（§20，M2 起由「导入到我的笔记库」产出）。
/// 只是**格式标识**，只参与「导出 / 导入的复用比对」与库准入；云端键不变。
pub const EXPORT_MODE_MD: &str = "md";

/// 清单 meta 键：建这个 md 库时用的**转换器版本**（[`crate::md::CONVERTER_VERSION`]）。
/// 它参与"能否复用旧包"的判定 —— 没有它就是"从未记录过"（视同陈旧，会全量重导一次）。
pub const MD_CONVERTER_META: &str = "md_converter";

/// 库/导出目录的形态（§20.3 / §4.6.4）。**同一份代码两条路径，不设兼容分支**：
/// - [`ZipFormat::Native`]：源 zip 字节级拷贝 → **「导出数据」逃生舱专用**（D0 硬约束）；
/// - [`ZipFormat::Md`]：库内 md 包（`index.html` → `note.md`，`index_files/` 整包搬运）→
///   **库的形态**（「导入到我的笔记库」与 CLI `build-md-library`）。
///
/// 两者产出的落地路径、文件名、清单 `exported_path` **完全一致**（都是 `{标题}.zip`），
/// 差别只在包内正文条目名 → 云端键推导与增量复用逻辑照旧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZipFormat {
    Native,
    Md,
}

impl ZipFormat {
    /// 清单 `export_mode` 与 `meta.export_mode` 的取值（格式标识）
    pub fn mode(self) -> &'static str {
        match self {
            ZipFormat::Native => EXPORT_MODE,
            ZipFormat::Md => EXPORT_MODE_MD,
        }
    }

    /// 包内正文档名（native 是为知原生的 `index.html`，md 库是 `note.md`）
    pub fn body_entry(self) -> &'static str {
        match self {
            ZipFormat::Native => "index.html",
            ZipFormat::Md => crate::md::NOTE_MD,
        }
    }

    /// 清单 `note.content_format`（§3.3 v5）：正文的**表示形态**（与 `export_mode` 是两个维度 ——
    /// `export_mode` 判复用、`content_format` 描述正文是 HTML 还是 Markdown）。
    pub fn content_format(self) -> &'static str {
        match self {
            ZipFormat::Native => crate::manifest::FORMAT_HTML,
            ZipFormat::Md => crate::manifest::FORMAT_MARKDOWN,
        }
    }
}

/// md 包转换统计（**只有 md 形态有**；native 形态为 `None`）。
///
/// 存在的理由：`build-md-library` 全库跑 1780 篇时，"转了几篇 / 有几篇正文空 / 围栏与表格总数"
/// 是转换质量的**第一手体检口径**（比事后抽查 md 更快、也不会漏）。
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MdPackStats {
    /// 已按 md 形态落地的篇数
    pub notes: usize,
    /// 转换后正文为空的篇数（源正文本身为空/无可见内容）
    pub empty_md: usize,
    /// 空正文的样本（最多 5 条，供抽查）
    pub empty_samples: Vec<String>,
    /// md 正文字节合计
    pub md_bytes: u64,
    /// 代码围栏合计
    pub code_fences: usize,
    /// 其中由"代码排版表"降级而来的围栏合计
    pub code_tables: usize,
    /// GFM 表格合计
    pub tables: usize,
    /// 摊平的块级布局表合计
    pub layout_tables: usize,
    /// 吞掉的 CodeMirror 镜像合计
    pub mirrors: usize,
}

/// 每份笔记导出为一个 zip 的结果：通用报告之上补充清单计数（§5）
#[derive(Debug, serde::Serialize)]
pub struct FolderZipExportReport {
    #[serde(flatten)]
    pub report: ExportReport,
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
    /// md 形态的转换统计；native 形态为 `None`
    pub md: Option<MdPackStats>,
}

/// 批量导出为**每篇一个 zip**（native，D0 逃生舱）：源 zip 字节级原样复制。
/// 见 [`export_folder_zips_fmt`] 的完整说明；`export_mode` 恒写 `native`。
pub fn export_folder_zips(
    ctx: &ExportContext,
    location: &str,
    dest_root: &Path,
    progress: &dyn Fn(usize, usize),
) -> Result<FolderZipExportReport, String> {
    export_folder_zips_fmt(ctx, location, dest_root, ZipFormat::Native, progress)
}

/// 批量导出为**库内 md 包**（§20.3，M2：库的形态）。
/// 与 native 的差别只有一处 —— 包内正文：`index.html` → `note.md`（`index_files/` 整包搬运）。
/// `export_mode` 写 `md`。
pub fn export_folder_zips_md(
    ctx: &ExportContext,
    location: &str,
    dest_root: &Path,
    progress: &dyn Fn(usize, usize),
) -> Result<FolderZipExportReport, String> {
    export_folder_zips_fmt(ctx, location, dest_root, ZipFormat::Md, progress)
}

/// 每份笔记导出为一个 zip，输出按目录层级还原（两种形态共用同一循环，只有"写"这一步不同）：
/// - [`ZipFormat::Native`]：逐篇**字节级原样复制**为知原生 zip 包（仅重命名为「净化标题.zip」）
/// - [`ZipFormat::Md`]：逐篇**转换**为库内 md 包（`note.md` + `index_files/` 原样条目）
/// 同时维护导出根同步清单 `export.db`（manifest.rs，§5）：
/// - 未变篇目（data_modified/package_size/落地路径均未变、清单格式标识与本次形态一致且文件在）
///   直接**复用现有 zip，零重写**（§6.2 第一级）—— 故 native 库在 md 目标下会**全部重导**，
///   反之亦然（格式标识参与复用比对，这是有意的：避免两种形态在同一目录里混着）
/// - 标题/目录变更的篇目重导并按 Q4 直接删除旧路径文件
/// - 源库已消失的篇目转入墓碑（只记录，不删文件，§6.3）
/// 注意：原生 zip 不含附件（继承为知格式），附件随「通用文件」导出（[`export_folder`]）走；全程不改源数据。
pub fn export_folder_zips_fmt(
    ctx: &ExportContext,
    location: &str,
    dest_root: &Path,
    fmt: ZipFormat,
    progress: &dyn Fn(usize, usize),
) -> Result<FolderZipExportReport, String> {
    let t0 = std::time::Instant::now();
    std::fs::create_dir_all(dest_root).map_err(|e| e.to_string())?;
    let conn = open_index_ro(ctx)?;
    let notes = load_notes(&conn, location)?;
    let plan = plan_zip_paths(&notes);

    // 同步清单（§5）：建库载入旧行，供增量复用判定（§6.2 第一级）
    let mconn = manifest::open_or_create(dest_root)?;
    let prev_rows = manifest::load_notes(&mconn)?;
    // **转换器版本闸门**（M3/§20.3）：md 形态下，若本目录上一次是用**别的**转换器版本建的
    // （或从未记录过），则**整体放弃复用、全量重导** —— 否则「改进转换器」这件事永远
    // 落不进已有的 md 库（源没变、清单没变 ⇒ 判定为可复用 ⇒ 老正文一直留着）。
    // 只对 md 形态生效：native 是字节拷贝，与转换器无关。
    let md_conv = manifest::get_meta(&mconn, MD_CONVERTER_META)?.unwrap_or_default();
    let md_conv_stale = fmt == ZipFormat::Md && md_conv != crate::md::CONVERTER_VERSION;
    let mut pre_warnings: Vec<String> = Vec::new();
    if md_conv_stale {
        pre_warnings.push(format!(
            "转换器版本变更（清单记录 {} → 当前 {}）：除库内已本地修改的篇目外，本次全量重导",
            if md_conv.is_empty() { "(无)" } else { md_conv.as_str() },
            crate::md::CONVERTER_VERSION
        ));
    }
    // P2 覆盖守卫：库内被本地写过的篇目（行级 revision > 0）**不参与**从源重导，
    // 否则一次「导入到我的笔记库」就会静默抹掉用户的编辑（§15.7 R14）。
    // 逃生舱导出（目标非库根）不受影响：那种目标目录没有清单，集合恒空。
    let locally_edited = manifest::load_dirty_guids(&mconn)?;

    let mut skipped = Vec::new();
    let mut notes_exported = 0usize;
    let mut notes_added = 0usize;
    let mut notes_reexported = 0usize;
    let mut notes_reused = 0usize;
    // md 形态的转换统计（native 形态保持 None）
    let mut md_stats: Option<MdPackStats> = match fmt {
        ZipFormat::Md => Some(MdPackStats::default()),
        ZipFormat::Native => None,
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

        // P2 覆盖守卫（先于复用判定）：库内版本是用户改过的，重导会用源版本覆盖 → 拒绝并报告
        if locally_edited.contains(&n.guid) {
            notes_exported += 1;
            notes_reused += 1;
            skipped.push(format!(
                "{}「{}」库内已本地修改，跳过覆盖（保留库内版本）",
                n.guid, n.title
            ));
            if (i + 1) % 50 == 0 || i + 1 == total {
                progress(i + 1, total);
            }
            continue;
        }

        // 增量复用判定（§6.2 第一级）：源字段/格式标识/落地路径均未变、文件在，
        // 且**转换器版本未变**（md 形态：版本变了就要按新口径重导）→ 零重写
        let prev = prev_rows.get(&n.guid);
        let unchanged = !md_conv_stale
            && matches!(&prev, Some(o)
                if o.data_modified == n.data_modified
                    && o.package_size == n.package_size
                    && o.export_mode == fmt.mode()
                    && o.exported_path == p.rel_path
                    && dest.is_file());

        if unchanged {
            notes_reused += 1;
            notes_exported += 1;
            let o = prev.unwrap();
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
                    exported_at: o.exported_at.clone(),
                    // 复用支：内容直存原文 → 来源恒为「导入自源」；正文形态与本次 fmt 一致
                    // （`fmt.mode() == o.export_mode` 是走上这一支的前提）
                    origin: manifest::ORIGIN_WIZNOTE.into(),
                    content_format: fmt.content_format().into(),
                },
            )?;
            // v6：复用支只刷新**源库元数据**（url/标题/目录），内容字节没动 ⇒ 只可能脏 `info` 段。
            // **必须按实差置脏**：无条件置脏会让每跑一次导出就把全库标脏 ⇒ 每次同步都重传清单、
            // 版本号空转（水位失去意义）。而完全不置脏则会让源库里的改名/搬家永远传不上去。
            let info_changed = o.title != n.title
                || o.location != n.location
                || o.url != n.url
                || o.doc_type != n.doc_type;
            manifest::mark_note_dirty(&mconn, &n.guid, info_changed, false)?;
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
            // native：边拷边算 MD5，不做二次读盘（§9.1 Q2）；md：整包重写成 md 包（§20.3）
            let write = || -> Result<(String, i64, Option<MdPackWrite>), String> {
                match fmt {
                    ZipFormat::Native => {
                        let (len, md5) = manifest::copy_with_md5(&src, &dest)?;
                        Ok((md5, len as i64, None))
                    }
                    ZipFormat::Md => {
                        let w = write_md_package(&src, &dest)?;
                        Ok((w.md5.clone(), w.size, Some(w)))
                    }
                }
            };
            match write() {
                Ok((md5, size, pack)) => {
                    notes_exported += 1;
                    if prev.is_some() {
                        notes_reexported += 1;
                    } else {
                        notes_added += 1;
                    }
                    if let (Some(acc), Some(w)) = (md_stats.as_mut(), pack) {
                        acc.notes += 1;
                        acc.md_bytes += w.md_len;
                        acc.code_fences += w.stats.code_blocks;
                        acc.code_tables += w.stats.code_tables;
                        acc.tables += w.stats.tables;
                        acc.layout_tables += w.stats.layout_tables;
                        acc.mirrors += w.stats.mirrors;
                        if w.empty {
                            acc.empty_md += 1;
                            if acc.empty_samples.len() < 5 {
                                acc.empty_samples.push(format!("{}「{}」", n.guid, n.title));
                            }
                        }
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
                            export_mode: fmt.mode().to_string(),
                            exported_at,
                            // 重导支：内容来自源库 → 来源「导入自源」；形态随 fmt（md 库 = markdown）
                            origin: manifest::ORIGIN_WIZNOTE.into(),
                            content_format: fmt.content_format().into(),
                        },
                    )?;
                    // v6：重导支 = 包内字节被重写 ⇒ `data` 段必脏（新 md5 要上云）；
                    // `info` 段按实差（标题/目录变过才置），口径与复用支一致。
                    let info_changed = prev
                        .map(|o| {
                            o.title != n.title
                                || o.location != n.location
                                || o.exported_path != p.rel_path
                        })
                        .unwrap_or(true);
                    manifest::mark_note_dirty(&mconn, &n.guid, info_changed, true)?;
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
    // 墓碑收敛：源库已消失（全库口径）的清单行转 deleted（§6.3，只记录不删文件）
    let all_guids = load_all_guids(&conn)?;
    let now = manifest::format_utc(std::time::SystemTime::now());
    let tombstoned = manifest::reconcile_tombstones(&mconn, &all_guids, &now)?;
    // v6（`docs/云同步逻辑.md` §8 作废清单第 2 条）：**导出不再铸版**。
    // 原处分是 `bump_revision`（"revision 单调递增，未来多端新旧判定"）—— 那是旧口径
    // （拿清单计数器当同步判据）的遗留。新口径下 `meta.revision` 只在**云同步上行提交点**
    // 推进，建库/重导路径只置脏闩（后者已由 `upsert_note` 的 INSERT 支与下面两处置脏覆盖）。
    manifest::set_meta(&mconn, "exported_at", &now)?;
    manifest::set_meta(&mconn, "export_mode", fmt.mode())?;
    manifest::set_meta(&mconn, "source_data_dir", &ctx.notes_dir.to_string_lossy())?;
    manifest::set_meta(&mconn, "tool_version", env!("CARGO_PKG_VERSION"))?;
    // md 库额外记一条**转换器版本**（M3）：它决定"下次能否复用"，必须与包内正文的实际
    // 生成口径一致。native 形态清掉它 —— 免得日后同一目录换成 md 形态时误判为"版本未变"。
    manifest::set_meta(
        &mconn,
        MD_CONVERTER_META,
        if fmt == ZipFormat::Md {
            crate::md::CONVERTER_VERSION
        } else {
            ""
        },
    )?;

    // 不变量自检（§5.5）：警告进报告，不中断导出
    let mut manifest_warnings = manifest::check_invariants(&mconn, dest_root)?;
    manifest_warnings.splice(0..0, pre_warnings);

    Ok(FolderZipExportReport {
        report: ExportReport {
            notes_exported,
            attachments_exported: 0,
            attachments_missing: 0,
            folders_exported: made_dirs.len(),
            // native 不物化代码块（源 zip 原样搬）；md 形态如实报"产出的代码围栏数"
            code_blocks_materialized: md_stats.as_ref().map(|s| s.code_fences).unwrap_or(0),
            skipped,
            elapsed_ms: t0.elapsed().as_millis(),
        },
        manifest_path: Some(manifest::manifest_path(dest_root).to_string_lossy().into_owned()),
        notes_added,
        notes_reexported,
        notes_reused,
        notes_removed: tombstoned.len(),
        manifest_warnings,
        md: md_stats,
    })
}

// ------------------------------------------------------ md 包构建（§20.3 / M2）

/// 一次 md 包写盘的结果
struct MdPackWrite {
    /// 落地 zip 的 MD5（清单行用）
    md5: String,
    /// 落地 zip 的字节数
    size: i64,
    /// 包内 `note.md` 的字节数
    md_len: u64,
    /// 正文转换后为空（`note.md` 去空白后为空串）
    empty: bool,
    /// 转换统计
    stats: crate::md::MdStats,
}

/// 同目录临时文件名：`{name}.zip` → `{name}.zip.tmp`（拼接后缀，不替换扩展名）
fn tmp_beside(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

/// 把**源原生 zip** 转成**库内 md 包**（§20.3，M2）：
/// - 包内正文：`index.html`（剥 BOM 解码）→ [`crate::md::html_to_md`] → **`note.md`**（UTF-8 无 BOM）；
/// - **其余条目整包搬运**：`index_files/...` 用 [`zip::ZipWriter::raw_copy_file`] 原样复制 ——
///   条目名、压缩方式、压缩后的字节都不变（附件名一个不改，md 里仍写 `![](index_files/xxx)`）；
/// - 原子性：先写同目录 `{name}.zip.tmp` → `flush` → `sync_all`(fsync) → **rename 覆盖**；
///   任何一步失败就删 tmp，**原有文件不动**（与 [`crate::library::rewrite_note_zip`] 同口径）。
///
/// 与 `rewrite_note_zip` 的分工：那个是**改**已有 md 包（M3 的 `save_note_md`），
/// 这个是**从源 zip 生成** md 包（导入路径），故临时文件名与失败语义相同、用途不同。
fn write_md_package(src: &Path, dest: &Path) -> Result<MdPackWrite, String> {
    let tmp = tmp_beside(dest);
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> Result<MdPackWrite, String> {
        let src_file = File::open(src).map_err(|e| format!("打开源包失败 {}: {e}", src.display()))?;
        let mut ar = zip::ZipArchive::new(src_file).map_err(|e| format!("源包不是合法 zip: {e}"))?;

        // ① 读源正文（为知实测 100% 带 UTF-8 BOM → 按 utf-8-sig 解码）
        let html = {
            let mut e = ar
                .by_name("index.html")
                .map_err(|e| format!("源包内无 index.html: {e}"))?;
            let mut b = Vec::new();
            e.read_to_end(&mut b).map_err(|e| e.to_string())?;
            crate::zipserve::decode_utf8_sig(&b)
        };
        let (md, stats) = crate::md::html_to_md_with_stats(&html);

        // ② 组装：note.md 打头（与原 index.html 的位置一致），其余条目原样搬运
        let out = File::create(&tmp).map_err(|e| e.to_string())?;
        let mut zw = ZipWriter::new(out);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file(crate::md::NOTE_MD, opts).map_err(|e| e.to_string())?;
        zw.write_all(md.as_bytes()).map_err(|e| e.to_string())?;
        for i in 0..ar.len() {
            let entry = ar.by_index(i).map_err(|e| format!("读 zip 条目 {i} 失败: {e}"))?;
            let name = entry.name().to_string();
            // 正文条目换成 note.md；源里若本就有 note.md（不该有）也不重复写
            if name == "index.html" || name == crate::md::NOTE_MD {
                continue;
            }
            zw.raw_copy_file(entry)
                .map_err(|e| format!("搬运条目 {name} 失败: {e}"))?;
        }
        let mut out = zw.finish().map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        out.sync_all().map_err(|e| e.to_string())?;
        drop(out);
        drop(ar); // Windows 上必须先释放源句柄才能 rename 覆盖

        let size = std::fs::metadata(&tmp).map_err(|e| e.to_string())?.len() as i64;
        let md5 = manifest::md5_file(&tmp)?;
        std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
        Ok(MdPackWrite {
            md5,
            size,
            md_len: md.len() as u64,
            empty: md.trim().is_empty(),
            stats,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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

    /// D0 回归护栏（`云同步模块设计-阶段二.md` §0 第 10 条 / `本地笔记读写实现.md` §4.6）：
    /// 导出产物必须与源 zip **逐字节一致**，模式恒 `native`，且不再产生 slim 统计与瘦身报告。
    #[test]
    fn test_native_export_is_byte_identical() {
        let d = std::env::temp_dir().join(format!("wiz-export-d0-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let notes_dir = d.join("data").join("notes");
        std::fs::create_dir_all(&notes_dir).unwrap();

        // 最小源库：native 是字节级拷贝、不解析 zip 内容，故任意字节即可（含 0x00 与高位字节）
        let guid = "11111111-2222-3333-4444-555555555555";
        let bytes: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(notes_dir.join(format!("{{{guid}}}")), &bytes).unwrap();

        let index_db = d.join("index.db");
        {
            let c = Connection::open(&index_db).unwrap();
            c.execute_batch(
                "CREATE TABLE note (guid TEXT PRIMARY KEY, title TEXT, location TEXT, created TEXT, \
                 data_modified TEXT, url TEXT, type TEXT, has_attachment INTEGER, package_size INTEGER);",
            )
            .unwrap();
            c.execute(
                "INSERT INTO note VALUES (?1, 'T', '/d/', 'c', 'm', NULL, NULL, 0, 4096)",
                [guid],
            )
            .unwrap();
        }

        let ctx = ExportContext::new(notes_dir.clone(), index_db);
        let dest = d.join("export");
        let rep = export_folder_zips(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!(rep.report.notes_exported, 1);
        assert!(rep.report.skipped.is_empty(), "{:?}", rep.report.skipped);

        let mconn = manifest::open_and_migrate(&dest).unwrap();
        let rows = manifest::load_notes(&mconn).unwrap();
        let row = rows.get(guid).unwrap();
        let exported = dest.join(&row.exported_path);

        // ① 字节级一致（D0 的核心不变量）
        assert_eq!(std::fs::read(&exported).unwrap(), bytes, "D0：导出必须是源 zip 的字节级拷贝");
        assert_eq!(
            manifest::md5_file(&exported).unwrap(),
            manifest::md5_file(&notes_dir.join(format!("{{{guid}}}"))).unwrap()
        );
        // ② 格式标识恒 native
        assert_eq!(
            manifest::get_meta(&mconn, "export_mode").unwrap().as_deref(),
            Some(EXPORT_MODE)
        );
        assert_eq!(row.export_mode, EXPORT_MODE);
        // ③ 二次导出走复用路径（格式标识一致才复用 → 反证清单里已是 native）
        let rep2 = export_folder_zips(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!(rep2.notes_reused, 1, "格式标识一致才复用：未变篇目应零重写");
        assert_eq!(std::fs::read(&exported).unwrap(), bytes);

        let _ = std::fs::remove_dir_all(&d);
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

    // ---------------- M2：md 包（§20.3） ----------------

    /// 读 zip 全部条目名（顺序保留）
    fn zip_names(p: &Path) -> Vec<String> {
        let f = std::fs::File::open(p).unwrap();
        let mut ar = zip::ZipArchive::new(f).unwrap();
        (0..ar.len())
            .map(|i| ar.by_index(i).unwrap().name().to_string())
            .collect()
    }

    /// 读某条目原始字节（不存在 → None）
    fn zip_bytes(p: &Path, name: &str) -> Option<Vec<u8>> {
        let f = std::fs::File::open(p).unwrap();
        let mut ar = zip::ZipArchive::new(f).unwrap();
        let mut e = ar.by_name(name).ok()?;
        let mut b = Vec::new();
        e.read_to_end(&mut b).unwrap();
        Some(b)
    }

    /// 造一个最小源库（`notes/{GUID}` = 带 BOM 的 index.html + 若干 index_files 条目）
    fn make_source_note(notes_dir: &Path, guid: &str, html: &str, assets: &[(&str, &[u8])]) {
        std::fs::create_dir_all(notes_dir).unwrap();
        let f = std::fs::File::create(notes_dir.join(format!("{{{guid}}}"))).unwrap();
        let mut zw = ZipWriter::new(f);
        let opt = SimpleFileOptions::default();
        zw.start_file("index.html", opt).unwrap();
        zw.write_all(&[0xEF, 0xBB, 0xBF]).unwrap(); // 真实语料 100% 带 BOM
        zw.write_all(html.as_bytes()).unwrap();
        for (name, bytes) in assets {
            zw.start_file(*name, opt).unwrap();
            zw.write_all(bytes).unwrap();
        }
        let mut out = zw.finish().unwrap();
        out.flush().unwrap();
    }

    /// 造最小源索引（note 表一行）
    fn make_source_index(path: &Path, guid: &str, size: i64) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE note (guid TEXT PRIMARY KEY, title TEXT, location TEXT, created TEXT, \
             data_modified TEXT, url TEXT, type TEXT, has_attachment INTEGER, package_size INTEGER);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO note VALUES (?1, '标题一', '/d/', 'c', 'm', NULL, NULL, 0, ?2)",
            rusqlite::params![guid, size],
        )
        .unwrap();
    }

    /// M2 核心（§20.3）：包内 `index.html` → `note.md`，`index_files/` **原样搬运**；
    /// 清单行与 meta 的 `export_mode` 都是 `md`；二次跑走复用；换回 native 会**全部重导**
    /// （格式标识参与复用比对 —— 这条同时是"两种形态不会在同一目录里混着"的护栏）。
    #[test]
    fn test_md_package_replaces_body_keeps_assets() {
        let d = std::env::temp_dir().join(format!("wiz-export-md-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let notes_dir = d.join("data").join("notes");
        let guid = "11111111-2222-3333-4444-555555555555";
        let html = "<html><body><h1>标题一</h1><div>正文 <b>粗体</b></div>\
                    <pre><code class=\"language-rust\">let a = 1;</code></pre>\
                    <img src=\"index_files/pic.png\"></body></html>";
        let png: Vec<u8> = (0..64u32).map(|i| (i * 7 % 256) as u8).collect();
        let css = b".x{color:red}".to_vec();
        make_source_note(
            &notes_dir,
            guid,
            html,
            &[
                ("index_files/pic.png", png.as_slice()),
                ("index_files/a.css", css.as_slice()),
            ],
        );

        let index_db = d.join("index.db");
        let src_size = std::fs::metadata(notes_dir.join(format!("{{{guid}}}"))).unwrap().len() as i64;
        make_source_index(&index_db, guid, src_size);

        let ctx = ExportContext::new(notes_dir.clone(), index_db);
        let dest = d.join("lib");
        let rep = export_folder_zips_md(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!(rep.report.notes_exported, 1);
        assert!(rep.report.skipped.is_empty(), "{:?}", rep.report.skipped);

        let mconn = manifest::open_and_migrate(&dest).unwrap();
        let rows = manifest::load_notes(&mconn).unwrap();
        let row = rows.get(guid).unwrap();
        let pkg = dest.join(&row.exported_path);

        // ① 包结构：note.md + index_files/*，**没有** index.html
        let names = zip_names(&pkg);
        assert_eq!(names.first().map(|s| s.as_str()), Some(crate::md::NOTE_MD), "{names:?}");
        assert!(names.contains(&"index_files/pic.png".to_string()), "{names:?}");
        assert!(names.contains(&"index_files/a.css".to_string()), "{names:?}");
        assert!(!names.contains(&"index.html".to_string()), "index.html 必须被 note.md 取代");

        // ② 正文 = 转换器输出（逐字节一致，说明包内正文就是 md.rs 的产物）
        let md = String::from_utf8(zip_bytes(&pkg, crate::md::NOTE_MD).unwrap()).unwrap();
        assert_eq!(md, crate::md::html_to_md(html));
        assert!(md.contains("# 标题一"), "{md}");
        assert!(md.contains("```rust"), "{md}");
        assert!(md.contains("![](index_files/pic.png)"), "{md}");

        // ③ 随包条目**原样搬运**（字节级）
        assert_eq!(zip_bytes(&pkg, "index_files/pic.png").unwrap(), png);
        assert_eq!(zip_bytes(&pkg, "index_files/a.css").unwrap(), css);

        // ④ 落地路径与文件名与 native 一致（云端键推导不变的前提）
        assert!(row.exported_path.ends_with(".zip"), "{}", row.exported_path);
        // ⑤ 格式标识：行 + meta 都是 md
        assert_eq!(row.export_mode, EXPORT_MODE_MD);
        assert_eq!(
            manifest::get_meta(&mconn, "export_mode").unwrap().as_deref(),
            Some(EXPORT_MODE_MD)
        );
        // ⑥ 转换统计
        let md_stats = rep.md.as_ref().expect("md 形态必须带回统计");
        assert_eq!(md_stats.notes, 1);
        assert_eq!(md_stats.empty_md, 0);
        assert!(md_stats.code_fences >= 1, "{md_stats:?}");
        assert!(md_stats.md_bytes > 0);

        // ⑦ 二次运行：格式/源字段/路径都没变 → 零重写（清单 MD5 不变）
        let before = manifest::md5_file(&pkg).unwrap();
        let rep2 = export_folder_zips_md(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!(rep2.notes_reused, 1, "md → md 应复用");
        assert_eq!(manifest::md5_file(&pkg).unwrap(), before);

        // ⑧ 反向：换 native 导出到同一目录 → 格式标识不符 ⇒ 全部重导成原生拷贝
        let rep3 = export_folder_zips(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!(rep3.notes_reexported, 1, "格式标识变了必须重导，不能复用");
        let names3 = zip_names(&pkg);
        assert!(names3.contains(&"index.html".to_string()), "{names3:?}");
        assert!(!names3.contains(&crate::md::NOTE_MD.to_string()), "{names3:?}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 源正文为空（为知里有 36/37 B 的空页）时：**仍然落地**、包内 note.md 为空、
    /// 统计如实报 `empty_md`（不报错、不静默跳过）
    #[test]
    fn test_md_package_empty_source_still_lands() {
        let d = std::env::temp_dir().join(format!("wiz-export-md-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let notes_dir = d.join("data").join("notes");
        let guid = "22222222-3333-4444-5555-666666666666";
        make_source_note(&notes_dir, guid, "<html><body><div><br></div></body></html>", &[]);
        let index_db = d.join("index.db");
        let src_size = std::fs::metadata(notes_dir.join(format!("{{{guid}}}"))).unwrap().len() as i64;
        make_source_index(&index_db, guid, src_size);

        let ctx = ExportContext::new(notes_dir.clone(), index_db);
        let dest = d.join("lib");
        let rep = export_folder_zips_md(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!(rep.report.notes_exported, 1);
        assert!(rep.report.skipped.is_empty(), "{:?}", rep.report.skipped);

        let md_stats = rep.md.as_ref().unwrap();
        assert_eq!(md_stats.empty_md, 1, "空正文要能被统计报出来");
        assert_eq!(md_stats.empty_samples.len(), 1);

        let mconn = manifest::open_and_migrate(&dest).unwrap();
        let row = manifest::load_notes(&mconn).unwrap();
        let pkg = dest.join(&row.get(guid).unwrap().exported_path);
        assert!(pkg.is_file(), "空正文也要有包（清单行不能指向不存在的文件）");
        assert!(zip_bytes(&pkg, crate::md::NOTE_MD).is_some(), "包内必须有 note.md 条目");
        assert_eq!(zip_bytes(&pkg, crate::md::NOTE_MD).unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// M3：从 **md 包**单篇导出为原生 zip —— 包里只能有**一个**正文条目 `index.html`。
    ///
    /// 这里是回归闸门：导出物恒为原生形态（D0），故正文条目必是 `index.html`；
    /// 而源包的正文条目名随库内形态而变（md 包是 `note.md`）。若照抄"跳过 index.html"的老逻辑，
    /// md 库导出的包里会同时躺着新渲染的 `index.html` 与搬运来的 `note.md`（两份正文）。
    #[test]
    fn test_export_note_zip_from_md_package_has_single_body() {
        let d = std::env::temp_dir().join(format!("wiz-export-mdzip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let notes_dir = d.join("notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        let guid = "66666666-7777-8888-9999-aaaaaaaaaaaa";
        // md 包：note.md（无 BOM）+ index_files 两条
        {
            let f = std::fs::File::create(notes_dir.join(format!("{{{guid}}}"))).unwrap();
            let mut zw = ZipWriter::new(f);
            let opt = SimpleFileOptions::default();
            zw.start_file(crate::md::NOTE_MD, opt).unwrap();
            zw.write_all("# 标题\n\n正文 bodytext\n".as_bytes()).unwrap();
            zw.start_file("index_files/a.css", opt).unwrap();
            zw.write_all(b".x{color:red}").unwrap();
            zw.start_file("index_files/pic.png", opt).unwrap();
            zw.write_all(b"PNGDATA").unwrap();
            let mut out = zw.finish().unwrap();
            out.flush().unwrap();
        }
        // 前置断言：这确实是个 md 包
        let zip_svc = ZipService::new(notes_dir.clone());
        assert_eq!(zip_svc.body_format(guid), crate::zipserve::BodyFormat::Md);

        let index_db = d.join("index.db");
        make_source_index(&index_db, guid, 0);
        let ctx = ExportContext::new(notes_dir.clone(), index_db);
        let dest = d.join("out.zip");
        let rep = export_note_zip(&ctx, &zip_svc, guid, &[], &dest).unwrap();
        assert_eq!(rep.notes_exported, 1);
        assert!(rep.skipped.is_empty(), "{:?}", rep.skipped);

        let names = zip_names(&dest);
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "index.html").count(),
            1,
            "{names:?}"
        );
        assert!(
            !names.contains(&crate::md::NOTE_MD.to_string()),
            "md 正文条目不得被搬运进导出包（会出现两份正文）: {names:?}"
        );
        assert!(names.iter().any(|n| n == "index_files/a.css"), "{names:?}");
        assert!(names.iter().any(|n| n == "index_files/pic.png"), "{names:?}");
        // 导出物是**原生**形态：正文 HTML 即 md 的渲染结果
        let html = String::from_utf8(zip_bytes(&dest, "index.html").unwrap()).unwrap();
        assert!(html.contains("正文 bodytext"), "md 库的单篇导出物正文应是 md 的渲染结果: {html}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// M3：**转换器版本闸门** —— 版本变了必须全量重导，否则"改进转换器"这件事
    /// 永远落不进已建好的 md 库（源没变、清单没变 ⇒ 一律判定可复用）。
    #[test]
    fn test_md_converter_version_forces_reexport() {
        let d = std::env::temp_dir().join(format!("wiz-export-conv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let notes_dir = d.join("data").join("notes");
        let guid = "77777777-8888-9999-aaaa-bbbbbbbbbbbb";
        make_source_note(&notes_dir, guid, "<html><body><div>正文 text</div></body></html>", &[]);
        let index_db = d.join("index.db");
        let src_size = std::fs::metadata(notes_dir.join(format!("{{{guid}}}"))).unwrap().len() as i64;
        make_source_index(&index_db, guid, src_size);
        let ctx = ExportContext::new(notes_dir.clone(), index_db);
        let dest = d.join("lib");

        // ① 首次：新增 1 篇，meta 记下当前转换器版本
        let r1 = export_folder_zips_md(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!((r1.notes_added, r1.notes_reused), (1, 0));
        let mconn = manifest::open_and_migrate(&dest).unwrap();
        assert_eq!(
            manifest::get_meta(&mconn, MD_CONVERTER_META).unwrap().as_deref(),
            Some(crate::md::CONVERTER_VERSION)
        );
        drop(mconn);

        // ② 二次：源与清单都没变 → 全部复用（0 重导）
        let r2 = export_folder_zips_md(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!((r2.notes_added, r2.notes_reexported, r2.notes_reused), (0, 0, 1));

        // ③ 伪造成"上一版转换器建的库" → 必须全量重导，并在报告里说明原因
        let mconn = manifest::open_and_migrate(&dest).unwrap();
        manifest::set_meta(&mconn, MD_CONVERTER_META, "1").unwrap();
        drop(mconn);
        let r3 = export_folder_zips_md(&ctx, "", &dest, &|_, _| {}).unwrap();
        assert_eq!(
            (r3.notes_added, r3.notes_reexported, r3.notes_reused),
            (0, 1, 0),
            "转换器版本变更必须重导: {:?}",
            r3.manifest_warnings
        );
        assert!(
            r3.manifest_warnings.iter().any(|w| w.contains("转换器版本变更")),
            "{:?}",
            r3.manifest_warnings
        );

        // ④ native 形态不受该闸门影响（字节拷贝与转换器无关），且会把 meta 清空
        let dest2 = d.join("lib-native");
        std::fs::create_dir_all(&dest2).unwrap();
        let mconn = manifest::open_and_migrate(&dest2).unwrap();
        manifest::set_meta(&mconn, MD_CONVERTER_META, "1").unwrap();
        drop(mconn);
        let r4 = export_folder_zips(&ctx, "", &dest2, &|_, _| {}).unwrap();
        assert_eq!((r4.notes_added, r4.notes_reused), (1, 0));
        let r5 = export_folder_zips(&ctx, "", &dest2, &|_, _| {}).unwrap();
        assert_eq!((r5.notes_reexported, r5.notes_reused), (0, 1), "native 二次跑仍应复用");
        let mconn = manifest::open_readonly(&dest2).unwrap();
        assert_eq!(manifest::get_meta(&mconn, MD_CONVERTER_META).unwrap().as_deref(), Some(""));

        let _ = std::fs::remove_dir_all(&d);
    }
}
