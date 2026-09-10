//! 导出逃生舱（FR-08 / T4.0）
//!
//! - 单篇 zip / 单篇自包含 HTML / 按目录批量 / 全库还原文件树
//! - 附件随导出带出（Tier1/2/3 → 同级 attachments/，Tier4 → 根级 _unlinked_attachments/）
//! - 导出 HTML 必须物化代码块兼容层（textarea → pre/code）
//! - 导出全程不修改源数据（G1）

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::extract::{export_name, html_escape, inject_before_body_close, materialize_code_blocks, sanitize_title};
use crate::zipserve::ZipService;

pub struct ExportContext {
    pub notes_dir: PathBuf,
    pub index_db: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct ExportReport {
    pub notes_exported: usize,
    pub attachments_exported: usize,
    pub skipped: Vec<String>,
    pub elapsed_ms: u128,
}

impl ExportContext {
    pub fn new(notes_dir: PathBuf, index_db: PathBuf) -> Self {
        Self { notes_dir, index_db }
    }
}

/// 附件区 HTML 片段（相对链接，脱离本软件仍可点开）
fn attachments_block(items: &[(String, String, bool)]) -> String {
    // items: (显示名, 相对链接, 可打开)
    if items.is_empty() {
        return String::new();
    }
    let mut rows = String::new();
    for (name, link, openable) in items {
        let row = if *openable {
            format!(
                r#"<li><a href="{}" style="color:#2b6cb0;">📎 {}</a></li>"#,
                html_escape(link),
                html_escape(name)
            )
        } else {
            format!(
                r#"<li><span style="color:#999;">📎 {}（文件未随导出下载）</span></li>"#,
                html_escape(name)
            )
        };
        rows.push_str(&row);
    }
    format!(
        r#"<div style="border-top:1px solid #ddd;margin-top:2em;padding-top:1em;font-family:sans-serif;">
<h3 style="font-size:14px;color:#666;margin:0 0 .5em;">附件</h3>
<ul style="list-style:none;padding:0;margin:0;font-size:13px;">{}</ul>
</div>"#,
        rows
    )
}

/// 单篇导出为 zip：index.html + index_files/ + attachments/
pub fn export_note_zip(
    _ctx: &ExportContext,
    zip_svc: &ZipService,
    guid: &str,
    _title: &str,
    attachments: &[(String, String, bool)], // (display_name, abs_path_or_empty, exists)
    dest_zip: &Path,
) -> Result<usize, String> {
    let file = File::create(dest_zip).map_err(|e| e.to_string())?;
    let mut zw = ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut count = 0usize;

    let html = zip_svc.read_index_html(guid)?;
    let (html_mat, _) = materialize_code_blocks(&html);
    // 附件区（相对链接 attachments/<name>）
    let mut used: HashSet<String> = HashSet::new();
    let mut items: Vec<(String, String, bool)> = Vec::new();
    let mut att_map: Vec<(String, String)> = Vec::new(); // (源绝对路径, 导出名)
    for (name, path, exists) in attachments {
        if *exists {
            let out_name = unique_name(&sanitize_title(name), &mut used);
            att_map.push((path.clone(), out_name.clone()));
            items.push((name.clone(), format!("attachments/{}", out_name), true));
        } else {
            items.push((name.clone(), String::new(), false));
        }
    }
    let html_final = inject_before_body_close(&html_mat, &attachments_block(&items));
    zw.start_file("index.html", opts).map_err(|e| e.to_string())?;
    zw.write_all(html_final.as_bytes()).map_err(|e| e.to_string())?;
    count += 1;

    // 全部条目
    for entry in zip_svc.list_entries(guid)? {
        if entry == "index.html" {
            continue;
        }
        let bytes = zip_svc.read_entry(guid, &entry)?;
        zw.start_file(entry.as_str(), opts).map_err(|e| e.to_string())?;
        zw.write_all(&bytes).map_err(|e| e.to_string())?;
        count += 1;
    }

    // 附件
    for (src, out_name) in att_map {
        let mut data = Vec::new();
        File::open(&src)
            .and_then(|mut f| f.read_to_end(&mut data))
            .map_err(|e| e.to_string())?;
        zw.start_file(format!("attachments/{}", out_name).as_str(), opts)
            .map_err(|e| e.to_string())?;
        zw.write_all(&data).map_err(|e| e.to_string())?;
        count += 1;
    }

    zw.finish().map_err(|e| e.to_string())?;
    Ok(count)
}

/// 单篇导出为自包含 HTML：图片/CSS/字体以 data: URI 内联
pub fn export_note_single_html(
    _ctx: &ExportContext,
    zip_svc: &ZipService,
    guid: &str,
    _title: &str,
    attachments: &[(String, String, bool)],
    dest_html: &Path,
) -> Result<(), String> {
    let html = zip_svc.read_index_html(guid)?;
    let (html_mat, _) = materialize_code_blocks(&html);

    // 内联 img / link / CSS 内的 url()
    let re_img = regex::Regex::new(r#"(?i)(<img[^>]*\ssrc=")(index_files/[^"]+)(")"#).unwrap();
    let re_link = regex::Regex::new(r#"(?i)(<link[^>]*\shref=")(index_files/[^"]+)("[^>]*>)"#).unwrap();
    let html_inlined = re_img.replace_all(&html_mat, |c: &regex::Captures| {
        let path = &c[2];
        match inline_as_data_uri(zip_svc, guid, path) {
            Some(uri) => format!("{}{}{}", &c[1], uri, &c[3]),
            None => c[0].to_string(),
        }
    });
    let html_inlined = re_link.replace_all(&html_inlined, |c: &regex::Captures| {
        let path = &c[2];
        match inline_css(zip_svc, guid, path) {
            Some(css) => format!("<style>{}</style>", css),
            None => c[0].to_string(),
        }
    });

    let items: Vec<(String, String, bool)> = attachments
        .iter()
        .map(|(n, _, e)| (n.clone(), String::new(), *e))
        .collect();
    // 自包含 HTML 中附件无法相对链接，只列出（附件需走 zip 导出带出）
    let html_final = inject_before_body_close(&html_inlined, &attachments_block(&items));
    std::fs::write(dest_html, html_final.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

fn inline_as_data_uri(zip_svc: &ZipService, guid: &str, path: &str) -> Option<String> {
    let bytes = zip_svc.read_entry(guid, path).ok()?;
    let mime = crate::zipserve::content_type(path).split(';').next().unwrap_or("");
    Some(format!("data:{};base64,", mime))
        .zip(Some(base64_encode(&bytes)))
        .map(|(a, b)| format!("{}{}", a, b))
}

fn inline_css(zip_svc: &ZipService, guid: &str, path: &str) -> Option<String> {
    let bytes = zip_svc.read_entry(guid, path).ok()?;
    let css = crate::zipserve::decode_utf8_sig(&bytes);
    // 重写 css 内相对 url(...) 为 data: URI（同目录相对）
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let re_url = regex::Regex::new(r#"url\((['"]?)([^'")]+)(['"]?)\)"#).unwrap();
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

/// 按目录 / 全库导出：还原目录层级为文件夹结构
/// `location` 为空表示全库。
pub fn export_folder(
    ctx: &ExportContext,
    zip_svc: &ZipService,
    location: &str,
    dest_root: &Path,
    progress: &dyn Fn(usize, usize),
) -> Result<ExportReport, String> {
    let t0 = std::time::Instant::now();
    std::fs::create_dir_all(dest_root).map_err(|e| e.to_string())?;
    let conn = Connection::open_with_flags(&ctx.index_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;

    // notes
    let mut st = conn
        .prepare("SELECT guid, title, location FROM note ORDER BY location, title")
        .map_err(|e| e.to_string())?;
    let all: Vec<(String, String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    drop(st);
    let selected: Vec<_> = all
        .iter()
        .filter(|(_, _, loc)| location.is_empty() || loc.starts_with(location))
        .collect();

    // 附件映射
    let mut st = conn
        .prepare(
            "SELECT file_path, display_name, document_guid, tier FROM attachment",
        )
        .map_err(|e| e.to_string())?;
    let atts: Vec<(String, String, Option<String>, i64)> = st
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    drop(st);
    // Tier3 多归属
    let mut st = conn
        .prepare("SELECT file_path, document_guid FROM attachment_doc")
        .map_err(|e| e.to_string())?;
    let tier3_links: Vec<(String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    drop(st);

    let mut atts_by_doc: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new(); // doc_guid -> [(file_path, display_name)]
    for (fp, name, doc, tier) in &atts {
        match doc {
            Some(d) => {
                atts_by_doc
                    .entry(d.clone())
                    .or_default()
                    .push((fp.clone(), name.clone()));
            }
            None => {}
        }
        if *tier == 3 {
            for (fp2, d) in &tier3_links {
                if fp2 == fp {
                    atts_by_doc
                        .entry(d.clone())
                        .or_default()
                        .push((fp.clone(), name.clone()));
                }
            }
        }
    }

    let mut skipped = Vec::new();
    let mut notes_exported = 0usize;
    let mut attachments_exported = 0usize;
    let total = selected.len();

    // 每个目录内重名去重用
    let mut used_names_by_dir: std::collections::HashMap<String, HashSet<String>> =
        std::collections::HashMap::new();

    for (i, (guid, title, loc)) in selected.iter().enumerate() {
        let dir_rel = loc.trim_matches('/');
        let dir_abs = dest_root.join(dir_rel);
        std::fs::create_dir_all(&dir_abs).map_err(|e| e.to_string())?;
        let used = used_names_by_dir.entry(loc.clone()).or_default();
        let name = export_name(title, guid, used);
        let note_dir = dir_abs.join(&name);
        std::fs::create_dir_all(&note_dir).map_err(|e| e.to_string())?;

        match zip_svc.read_index_html(guid) {
            Ok(html) => {
                let (html_mat, _) = materialize_code_blocks(&html);
                // 附件
                let mut items: Vec<(String, String, bool)> = Vec::new();
                let att_dir = note_dir.join("attachments");
                let att_list = atts_by_doc.get(guid).cloned().unwrap_or_default();
                let mut used_att: HashSet<String> = HashSet::new();
                for (fp, disp) in &att_list {
                    if fp.starts_with("db-missing:") {
                        items.push((disp.clone(), String::new(), false));
                        continue;
                    }
                    std::fs::create_dir_all(&att_dir).ok();
                    let out_name = unique_name(&sanitize_title(disp), &mut used_att);
                    let dest = att_dir.join(&out_name);
                    if std::fs::copy(fp, &dest).is_ok() {
                        attachments_exported += 1;
                        items.push((disp.clone(), format!("attachments/{}", out_name), true));
                    } else {
                        items.push((disp.clone(), String::new(), false));
                    }
                }
                let html_final = inject_before_body_close(&html_mat, &attachments_block(&items));
                let index_path = note_dir.join("index.html");
                std::fs::write(index_path, html_final.as_bytes()).map_err(|e| e.to_string())?;
                // index_files/
                for entry in zip_svc.list_entries(guid)? {
                    if entry == "index.html" {
                        continue;
                    }
                    let bytes = match zip_svc.read_entry(guid, &entry) {
                        Ok(b) => b,
                        Err(_) => {
                            skipped.push(format!("{}: {}", guid, entry));
                            continue;
                        }
                    };
                    let out = note_dir.join(entry);
                    if let Some(parent) = out.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(out, bytes).map_err(|e| e.to_string())?;
                }
                notes_exported += 1;
            }
            Err(e) => skipped.push(format!("{}: {}", guid, e.message())),
        }
        if (i + 1) % 20 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }

    // Tier4 未关联附件 → 根级 _unlinked_attachments/
    let unlinked: Vec<(String, String)> = atts
        .iter()
        .filter(|(_, _, doc, tier)| doc.is_none() && *tier == 4)
        .map(|(fp, name, _, _)| (fp.clone(), name.clone()))
        .collect();
    if !unlinked.is_empty() {
        let ua = dest_root.join("_unlinked_attachments");
        std::fs::create_dir_all(&ua).map_err(|e| e.to_string())?;
        let mut used: HashSet<String> = HashSet::new();
        for (fp, name) in unlinked {
            let out = ua.join(unique_name(&sanitize_title(&name), &mut used));
            if std::fs::copy(&fp, &out).is_ok() {
                attachments_exported += 1;
            }
        }
    }

    Ok(ExportReport {
        notes_exported,
        attachments_exported,
        skipped,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

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
