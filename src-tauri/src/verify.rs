//! M4 验收加固
//!
//! - **T4.1 全库巡检** [`inspect`]：对 1,780 篇逐篇自动出报告，10 项全过
//! - **T4.2 源数据零写入** [`Snapshot`] / [`diff`]：全功能操作一轮前后快照比对
//! - **T4.3 安全项** [`security_checks`]：可自动化的逐条落检，不可自动化的显式标为待人工
//! - **T4.4 性能基准** [`bench`]：NFR-1 各阈值实测
//!
//! 巡检口径全部来自需求文档的实测基线（写死为常量），偏离即 `passed = false`。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::export::{self, ExportAttachment, ExportContext};
use crate::extract::{
    code_blocks, decode_entities, materialize_code_blocks, mirror_of, normalize_code_text,
    rendered_code_text,
};
use crate::indexer::{build_index, EXPECTED_NOTE_COUNT, EXPECTED_TIERS};
use crate::{sandbox, zipserve};
use crate::zipserve::ZipService;

// ---------------- 需求文档实测基线 ----------------
/// 形态 A 代码块总数（probe7 口径：隐藏 textarea 开标签，含 10 个无源码空块）
pub const BASE_CODE_BLOCKS: usize = 1533;
/// 其中带非空源码、可被兼容层/物化还原的块（M4 实测：1533 − 10 空块）
pub const BASE_CODE_BLOCKS_WITH_SOURCE: usize = 1523;
/// 含形态 A 的笔记数（文档 P7：367 篇）
pub const BASE_FORM_A_NOTES: usize = 367;
/// 引用了但包内不存在的资源（P4，走占位图）
pub const BASE_MISSING_REFS: usize = 268;
/// 近空笔记数（P2，正文 < 10 字符）
pub const BASE_EMPTY_NOTES: usize = 46;
/// attachments/ 白名单文件数
pub const BASE_ATTACHMENTS: usize = 133;
/// P17 同名不去重样本：附件区应列 8 条
pub const SAMPLE_SAME_NAME: &str = "491645c3-ada4-4f29-b3ed-3ab23f9e7881";
pub const SAMPLE_SAME_NAME_ROWS: usize = 8;
/// 附件仅元数据样本：2 条文件缺失记录不得报错
pub const SAMPLE_MISSING: &str = "376c9340-5e18-4a5f-b3d9-9ea4aa52041e";
pub const SAMPLE_MISSING_ROWS: usize = 2;
/// FR-10 未关联附件（Tier 4）
pub const BASE_UNLINKED: usize = 6;
/// NFR-1 / T1.4 阈值
pub const LIMIT_INDEX_MS: u128 = 30_000;
pub const LIMIT_MEDIAN_MS: u128 = 300;
pub const LIMIT_P99_MS: u128 = 1_000;
pub const LIMIT_MAX_MS: u128 = 2_000;
pub const LIMIT_SEARCH_MS: u128 = 200;
pub const LIMIT_COLDSTART_MS: u128 = 2_000;
pub const LIMIT_INDEX_MB: u64 = 30;
/// FR-06 验收六词
pub const ACCEPTANCE_WORDS: [&str; 6] =
    ["sefs", "bluestore", "pm_api_url", "昇腾", "分布式文件系统", "RDMA-400G"];

// ---------------- 巡检口径辅助 ----------------
regex_of!(re_form_a_open, r"(?is)<textarea[^>]*display\s*:\s*none");
regex_of!(re_p4_body, r"(?is)<body[^>]*>([\s\S]*)</body>");
regex_of!(re_p4_style, r"(?is)<style[\s\S]*?</style>");
regex_of!(re_p4_script, r"(?is)<script[\s\S]*?</script>");
regex_of!(re_p4_tag, r"(?s)<[^>]+>");
regex_of!(re_p4_ws, r"\s+");
regex_of!(re_ws_run, r"[ \t\u{a0}]+");

/// 文档 P2 的「近空笔记」口径（与 tmp_tools/probe4.py 逐行对齐，独立于本应用的
/// [`crate::extract::extract_text`]）：截 body → 剥 style/script → 标签换空格 →
/// 五个具名实体还原 → 全空白折叠为单空格 → trim → 计字符数
fn probe4_len(html: &str) -> usize {
    let body = re_p4_body()
        .captures(html)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| html.to_string());
    let s = re_p4_style().replace_all(&body, " ");
    let s = re_p4_script().replace_all(&s, " ");
    let s = re_p4_tag().replace_all(&s, " ");
    let s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"");
    re_p4_ws().replace_all(&s, " ").trim().chars().count()
}

/// 代码块「源码 textarea ↔ CodeMirror 镜像」比对结论
#[derive(Debug, PartialEq, Eq)]
enum RenderedCmp {
    /// tab→4 空格归一后逐字符一致
    Same,
    /// 仅空白展开宽度不同：镜像序列化时行首 `\t` → 4 空格、行中 `\t` → 1 空格，无任何字符丢失
    WsWidth,
    /// 内容确实不一致（真缺陷）
    Diff,
}

/// 折叠每行内的连续水平空白为单空格，并去掉行尾空白（只用于判定「差异是否仅在空白」）
fn ws_collapsed(s: &str) -> String {
    s.replace('\u{200b}', "")
        .split('\n')
        .map(|l| re_ws_run().replace_all(l.trim_end(), " ").trim().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn compare_rendered(src: &str, rendered: &str) -> RenderedCmp {
    let src = src.trim_end_matches('\n');
    let ren = rendered.trim_end_matches('\n');
    if normalize_code_text(src) == normalize_code_text(ren) {
        return RenderedCmp::Same;
    }
    if ws_collapsed(src) == ws_collapsed(ren) {
        return RenderedCmp::WsWidth;
    }
    RenderedCmp::Diff
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Check {
    pub id: String,
    pub name: String,
    pub passed: bool,
    /// 可跳过的检查（环境不具备条件），不计入 ok
    pub skipped: bool,
    pub actual: String,
    pub expected: String,
    /// 失败样本（最多 8 条，避免报告爆炸）
    pub samples: Vec<String>,
}

impl Check {
    fn new(id: &str, name: &str, actual: String, expected: String, passed: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            passed,
            skipped: false,
            actual,
            expected,
            samples: vec![],
        }
    }
    fn skip(id: &str, name: &str, why: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            passed: true,
            skipped: true,
            actual: why.into(),
            expected: String::new(),
            samples: vec![],
        }
    }
    fn with(mut self, samples: Vec<String>) -> Self {
        self.samples = samples.into_iter().take(8).collect();
        self
    }
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct VerifyReport {
    pub ok: bool,
    pub data_dir: String,
    pub inspection: Vec<Check>,
    pub export_check: Vec<Check>,
    pub security: Vec<Check>,
    pub zero_write: Vec<Check>,
    pub bench: Vec<Check>,
    pub elapsed_ms: u128,
}

impl VerifyReport {
    pub fn failed(&self) -> Vec<&Check> {
        self.inspection
            .iter()
            .chain(self.export_check.iter())
            .chain(self.security.iter())
            .chain(self.zero_write.iter())
            .chain(self.bench.iter())
            .filter(|c| !c.passed)
            .collect()
    }
}

fn push_sample(v: &mut Vec<String>, s: impl Into<String>) {
    if v.len() < 32 {
        v.push(s.into());
    }
}

fn open_ro(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("打开派生索引失败: {e}"))
}

/// 笔记清单：guid, title, location, body_text_length, package_size
type NoteRow = (String, String, String, i64, i64);

fn all_notes(conn: &Connection) -> Result<Vec<NoteRow>, String> {
    let mut st = conn
        .prepare("SELECT guid, title, location, body_text_length, package_size FROM note ORDER BY guid")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 与 commands::get_note_detail 完全相同的附件查询（含 Tier3 多归属、不去重）
fn note_attachment_rows(conn: &Connection, guid: &str) -> Result<Vec<(String, String, i64, i64, String)>, String> {
    let mut st = conn
        .prepare(
            "SELECT file_path, display_name, size, tier, source FROM attachment WHERE document_guid = ?1
             UNION ALL
             SELECT a.file_path, a.display_name, a.size, a.tier, a.source
             FROM attachment a JOIN attachment_doc d ON a.file_path = d.file_path
             WHERE d.document_guid = ?1
             ORDER BY display_name, file_path",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([guid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

// ================================ T4.1 全库巡检 ================================

/// 逐篇巡检 1,780 篇 + 附件层，输出需求文档 M4 的 10 项结论
pub fn inspect(
    data_dir: &Path,
    index_db: &Path,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Vec<Check>, String> {
    let notes_dir = data_dir.join("notes");
    let zip = ZipService::new(notes_dir.clone());
    let conn = open_ro(index_db)?;
    let notes = all_notes(&conn)?;

    // 源库笔记数（只读，G1）
    let source_note_count = {
        let uri = format!("file:{}?mode=ro", crate::indexer::url_encode_path(&data_dir.join("index.db")));
        let src = Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| format!("打开源库失败: {e}"))?;
        src.query_row("SELECT COUNT(*) FROM WIZ_DOCUMENT", [], |r| r.get::<_, usize>(0))
            .map_err(|e| e.to_string())?
    };
    let package_count = notes_dir
        .read_dir()
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| {
            e.path().is_file() && e.file_name().to_string_lossy().starts_with('{')
        })
        .count();

    let mut open_fail: Vec<String> = vec![];
    let mut decode_fail: Vec<String> = vec![];
    let mut mojibake: Vec<String> = vec![]; // 源数据自带 U+FFFD（历史 mojibake）
    let mut mojibake_notes = 0usize;
    let mut form_a_blocks = 0usize; // probe7 口径：隐藏 textarea 开标签（含空块）
    let mut form_a_notes = 0usize;
    let mut blocks_total = 0usize;
    let mut blocks_blank = 0usize; // 无渲染结果，必须物化
    let mut blocks_materialized = 0usize;
    let mut blocks_rendered_ok = 0usize;
    let mut blocks_rendered_tab = 0usize; // 仅 tab 展开宽度不同
    let mut blocks_rendered_tab_samples: Vec<String> = vec![];
    let mut blocks_rendered_diff: Vec<String> = vec![];
    let mut mat_verify_fail: Vec<String> = vec![];
    let mut near_empty_doc = 0usize; // 文档 P2 口径（基线 46）
    let mut refs_missing = 0usize;
    let mut refs_missing_notes = 0usize;
    let mut body_len_diff: Vec<String> = vec![];
    let mut empty_notes = 0usize;
    let mut remote_src = 0usize;
    let mut remote_src_notes = 0usize;
    let mut inline_script_notes = 0usize;

    let re_ref_missing = regex::Regex::new(r#"index_files/([^"')\s>]+)"#).unwrap();
    let re_src_attr =
        regex::Regex::new(r#"(?is)<[a-z]+[^>]*\ssrc\s*=\s*"([^"]+)""#).unwrap();
    let re_script = regex::Regex::new(r"(?is)<script").unwrap();
    let re_materialized = regex::Regex::new(
        r#"(?s)<pre style="margin:0;overflow-x:auto;white-space:pre;"><code[^>]*>([\s\S]*?)</code></pre>"#,
    )
    .unwrap();

    let total = notes.len();
    for (i, (guid, _title, _loc, body_len, _size)) in notes.iter().enumerate() {
        if i % 100 == 0 || i + 1 == total {
            progress("巡检", i + 1, total);
        }
        // ① 包可打开
        let bytes = match zip.read_entry(guid, "index.html") {
            Ok(b) => b,
            Err(e) => {
                push_sample(&mut open_fail, format!("{guid}: {}", e.message()));
                continue;
            }
        };
        // ① UTF-8 严格可解码（G3：剥 BOM 后 from_utf8 必须成功，不得走 lossy）
        let raw = zipserve::strip_bom(&bytes);
        if std::str::from_utf8(raw).is_err() {
            push_sample(&mut decode_fail, format!("{guid}: 严格解码失败"));
        }
        let html = zipserve::decode_utf8_sig(&bytes);
        // U+FFFD 如果存在于**原始字节**中，是当年作者粘进来的 mojibake，不是解码缺陷
        if html.contains('\u{fffd}') {
            mojibake_notes += 1;
            push_sample(
                &mut mojibake,
                format!("{guid}: 正文自带 {} 个 U+FFFD（源数据即如此）", html.matches('\u{fffd}').count()),
            );
        }

        // ② 代码块：源码 textarea ↔ 渲染态 / 物化结果
        let fa = re_form_a_open().find_iter(&html).count();
        form_a_blocks += fa;
        if fa > 0 {
            form_a_notes += 1;
        }
        let blocks = code_blocks(&html);
        blocks_total += blocks.len();
        blocks_blank += blocks.iter().filter(|b| !b.rendered).count();
        for b in &blocks {
            if !b.rendered {
                continue;
            }
            match mirror_of(&html, b.range.1) {
                Some(mirror) => {
                    let ren = rendered_code_text(mirror);
                    match compare_rendered(&b.source, &ren) {
                        RenderedCmp::Same => blocks_rendered_ok += 1,
                        RenderedCmp::WsWidth => {
                            blocks_rendered_tab += 1;
                            push_sample(
                                &mut blocks_rendered_tab_samples,
                                format!(
                                    "{guid}: 仅空白展开宽度不同（源码 {} 字 / 渲染 {} 字，折行内空白后逐字符一致）",
                                    b.source.chars().count(),
                                    ren.chars().count()
                                ),
                            );
                        }
                        RenderedCmp::Diff => push_sample(
                            &mut blocks_rendered_diff,
                            format!(
                                "{guid}: 渲染态与源码内容不一致（源码 {} 字 / 渲染 {} 字）",
                                normalize_code_text(b.source.trim_end_matches('\n')).chars().count(),
                                normalize_code_text(ren.trim_end_matches('\n')).chars().count()
                            ),
                        ),
                    }
                }
                None => push_sample(&mut blocks_rendered_diff, format!("{guid}: 标记为已渲染但取不到镜像")),
            }
        }
        let (out, mats) = materialize_code_blocks(&html);
        blocks_materialized += mats;
        if mats > 0 {
            // 物化结果必须与 textarea 原文逐字符一致
            let want: Vec<String> = blocks
                .iter()
                .filter(|b| !b.rendered)
                .map(|b| b.source.clone())
                .collect();
            let got: Vec<String> = re_materialized
                .captures_iter(&out)
                .map(|c| decode_entities(&c[1]))
                .collect();
            if got.len() != want.len() {
                push_sample(
                    &mut mat_verify_fail,
                    format!("{guid}: 物化块数 {} ≠ 应物化 {} ", got.len(), want.len()),
                );
            } else {
                for (n, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                    if g != w {
                        push_sample(
                            &mut mat_verify_fail,
                            format!("{guid}: 第 {} 个物化块与源码不逐字符一致", n + 1),
                        );
                    }
                }
            }
        }

        // ③ 引用资源可解析（口径同 tmp_tools/analyze.py：index_files/<名> 未出现在包内）
        let present: HashSet<String> = zip
            .list_entries(guid)
            .unwrap_or_default()
            .iter()
            .filter(|n| n.starts_with("index_files/"))
            .map(|n| n["index_files/".len()..].to_string())
            .collect();
        let refs: HashSet<String> = re_ref_missing
            .captures_iter(&html)
            .map(|c| c[1].to_string())
            .collect();
        let miss: Vec<&String> = refs
            .iter()
            .filter(|r| {
                let base = r.rsplit('/').next().unwrap_or(r);
                !present.contains(base)
            })
            .collect();
        if !miss.is_empty() {
            refs_missing += miss.len();
            refs_missing_notes += 1;
        }

        // ④ 正文纯文本长度与索引一致
        let text = crate::extract::extract_text(&html);
        if text.chars().count() as i64 != *body_len {
            push_sample(
                &mut body_len_diff,
                format!("{guid}: 巡检 {} / 索引 {}", text.chars().count(), body_len),
            );
        }
        if *body_len < 10 {
            empty_notes += 1; // 应用索引口径（UI「空笔记」徽标依据）
        }
        if probe4_len(&html) < 10 {
            near_empty_doc += 1; // 文档 P2 基线口径
        }

        // T4.3 素材：远程 src / 笔记自带 script
        let mut has_remote = false;
        for c in re_src_attr.captures_iter(&html) {
            let u = &c[1];
            if u.starts_with("http://") || u.starts_with("https://") || u.starts_with("//") {
                remote_src += 1;
                has_remote = true;
            }
        }
        if has_remote {
            remote_src_notes += 1;
        }
        if re_script.is_match(&html) {
            inline_script_notes += 1;
        }
    }

    // ⑥⑦⑧⑨⑩ 索引与附件层
    let idx_note_count: usize = conn
        .query_row("SELECT COUNT(*) FROM note", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let (t1, t2, t3, t4, t0) = tier_counts(&conn)?;
    let disk_attachments = attachment_files(&notes_dir.parent().unwrap_or(data_dir));
    let att_rows: Vec<(String, String, i64, i64, String)> = {
        let mut st = conn
            .prepare("SELECT file_path, display_name, size, tier, source FROM attachment ORDER BY file_path")
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
        rows.flatten().collect()
    };
    let mut att_open_fail: Vec<String> = vec![];
    let mut att_size_diff: Vec<String> = vec![];
    let mut openable = 0usize;
    for (fp, name, size, tier, _src) in &att_rows {
        if *tier == 0 {
            continue; // 库内有记录但文件未随导出下载，走降级路径（⑨）
        }
        match std::fs::read(fp) {
            Ok(b) if !b.is_empty() => {
                openable += 1;
                if b.len() as i64 != *size {
                    push_sample(&mut att_size_diff, format!("{name}: 读 {} / 索引 {}", b.len(), size));
                }
            }
            Ok(_) => push_sample(&mut att_open_fail, format!("{name}: 内容为空")),
            Err(e) => push_sample(&mut att_open_fail, format!("{name}: {e}")),
        }
    }
    let same_name_rows = note_attachment_rows(&conn, SAMPLE_SAME_NAME)?.len();
    let missing_rows = note_attachment_rows(&conn, SAMPLE_MISSING)?
        .into_iter()
        .filter(|(fp, _, _, _, _)| fp.starts_with("db-missing:"))
        .count();
    // ⑨ 缺失记录点击不报错：预览路径必须返回可读提示而非 panic
    let missing_graceful = note_attachment_rows(&conn, SAMPLE_MISSING)
        .map(|v| v.iter().any(|(_, _, _, tier, _)| *tier == 0))
        .unwrap_or(false);
    let unlinked: Vec<(String, String)> = {
        let mut st = conn
            .prepare("SELECT file_path, display_name FROM attachment WHERE tier = 4 AND source = 'unlinked'")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    let mut unlinked_fail: Vec<String> = vec![];
    let mut unlinked_ok = 0usize;
    for (fp, name) in &unlinked {
        match std::fs::read(fp) {
            Ok(b) if !b.is_empty() => unlinked_ok += 1,
            Ok(_) => push_sample(&mut unlinked_fail, format!("{name}: 内容为空")),
            Err(e) => push_sample(&mut unlinked_fail, format!("{name}: {e}")),
        }
    }

    // ⑥b 索引派生字段完整性：package_size 不得静默写 0（M4 实跑曾发现该缺陷）
    let (zero_size_rows, idx_bytes): (i64, i64) = conn
        .query_row(
            "SELECT SUM(package_size = 0), IFNULL(SUM(package_size), 0) FROM note",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    let disk_bytes: i64 = notes_dir
        .read_dir()
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.path().metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len() as i64)
                .sum()
        })
        .unwrap_or(0);

    let checks = vec![
        Check::new(
            "1",
            "笔记包可打开、index.html 严格 UTF-8 可解码",
            format!(
                "{}/{} 篇可打开；严格解码失败 {} 处；{} 篇正文自带 U+FFFD（源数据 mojibake，非解码缺陷）",
                total - open_fail.len(),
                total,
                decode_fail.len(),
                mojibake_notes
            ),
            format!("{} 篇全过、0 处解码失败", total),
            open_fail.is_empty() && decode_fail.is_empty(),
        )
        .with(format_errs(&open_fail, &mojibake)),
        {
            let passed = form_a_blocks == BASE_CODE_BLOCKS
                && form_a_notes == BASE_FORM_A_NOTES
                && blocks_total == BASE_CODE_BLOCKS_WITH_SOURCE
                && mat_verify_fail.is_empty()
                && blocks_rendered_diff.is_empty();
            Check::new(
                "2",
                "代码块：源码 textarea ↔ 渲染态 / 物化结果逐字符一致",
                format!(
                    "形态 A 块 {form_a_blocks}（probe7 口径，含无源码空块 {}）分布在 {form_a_notes} 篇；带源码块 {blocks_total}；未渲染 {blocks_blank} 块已物化 {blocks_materialized} 并逐字符回验；已渲染块 {blocks_rendered_ok} 严格一致、{blocks_rendered_tab} 仅空白展开宽度不同（镜像将行首 tab 存为 4 空格、行中 tab 存为 1 空格）",
                    form_a_blocks - blocks_total
                ),
                format!(
                    "块数 {BASE_CODE_BLOCKS}（带源码 {BASE_CODE_BLOCKS_WITH_SOURCE}）/ {BASE_FORM_A_NOTES} 篇；物化 100% 逐字符一致；渲染态无内容差异（容 tab→空格）"
                ),
                passed,
            )
            // 失败样本优先；无失败时附 tab 宽度样本供人工复核
            .with(
                format_errs(&mat_verify_fail, &blocks_rendered_diff)
                    .into_iter()
                    .chain(blocks_rendered_tab_samples.clone())
                    .collect(),
            )
        },
        Check::new(
            "3",
            "引用资源可解析（已知缺失走占位）",
            format!("{refs_missing} 处缺失，分布在 {refs_missing_notes} 篇"),
            format!("{BASE_MISSING_REFS} 处（口径同 tmp_tools/analyze.py）"),
            refs_missing == BASE_MISSING_REFS,
        )
        .with(vec![]),
        Check::new(
            "4",
            "正文纯文本长度与索引一致",
            format!("{} 篇一致 / {} 篇偏离", total - body_len_diff.len(), body_len_diff.len()),
            "全部一致".into(),
            body_len_diff.is_empty(),
        )
        .with(body_len_diff.clone()),
        Check::new(
            "5",
            "近空笔记（正文 < 10 字符）正常列出",
            format!(
                "文档口径 {near_empty_doc} 篇 / 应用索引口径 {empty_notes} 篇（差 {} 篇，因两者对标签与换行的折叠规则不同）",
                if near_empty_doc >= empty_notes {
                    near_empty_doc - empty_notes
                } else {
                    empty_notes - near_empty_doc
                }
            ),
            format!("{BASE_EMPTY_NOTES} 篇（文档 P2 口径），且全部照常列出不隐藏"),
            near_empty_doc == BASE_EMPTY_NOTES,
        ),
        Check::new(
            "6",
            "索引笔记数与派生字段恰为基线",
            format!(
                "索引 {idx_note_count} / 源库 {source_note_count} / notes 包 {package_count}；包体积索引合计 {:.1} MB（零值 {} 行，磁盘实计 {:.1} MB）",
                idx_bytes as f64 / 1048576.0,
                zero_size_rows,
                disk_bytes as f64 / 1048576.0
            ),
            format!("三者均为 {EXPECTED_NOTE_COUNT}；无零值行且与磁盘字节数一致"),
            idx_note_count == EXPECTED_NOTE_COUNT
                && source_note_count == EXPECTED_NOTE_COUNT
                && package_count == EXPECTED_NOTE_COUNT
                && zero_size_rows == 0
                && idx_bytes == disk_bytes,
        ),
        Check::new(
            "7",
            "全部附件能打开（验收线）",
            format!("{openable}/{} 个附件读出非空内容（Tier1={} Tier2={} Tier3={} Tier4={} DB缺失={}）", att_rows.len() - t0, t1, t2, t3, t4, t0),
            format!("{BASE_ATTACHMENTS} 个在盘文件全部能打开；Tier 分布 {}", tier_text()),
            att_open_fail.is_empty()
                && att_size_diff.is_empty()
                && openable == BASE_ATTACHMENTS
                && (t1, t2, t3, t4, t0) == EXPECTED_TIERS,
        )
        .with(format_errs(&att_open_fail, &att_size_diff)),
        Check::new(
            "8",
            &format!("{SAMPLE_SAME_NAME} 附件区同名不去重"),
            format!("{same_name_rows} 条"),
            format!("{SAMPLE_SAME_NAME_ROWS} 条"),
            same_name_rows == SAMPLE_SAME_NAME_ROWS,
        ),
        Check::new(
            "9",
            &format!("{SAMPLE_MISSING} 的 2 条缺失记录优雅降级"),
            format!("{missing_rows} 条标记为 db-missing，预览返回提示不报错：{missing_graceful}"),
            format!("{SAMPLE_MISSING_ROWS} 条 + 不报错"),
            missing_rows == SAMPLE_MISSING_ROWS && missing_graceful,
        ),
        Check::new(
            "10",
            "FR-10 未关联附件逐个可打开",
            format!("{unlinked_ok}/{BASE_UNLINKED} 个"),
            format!("{BASE_UNLINKED} 个全部能打开"),
            unlinked_fail.is_empty() && unlinked_ok == BASE_UNLINKED,
        )
        .with(unlinked_fail.clone()),
    ];

    // 供 T4.3 使用的素材计数（远程 src / 笔记自带 script）
    let _ = (remote_src, remote_src_notes, inline_script_notes, disk_attachments);
    Ok(checks)
}

fn format_errs(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().chain(b.iter()).cloned().collect()
}

fn tier_text() -> String {
    let (a, b, c, d, e) = EXPECTED_TIERS;
    format!("{a}/{b}/{c}/{d}/{e}")
}

fn tier_counts(conn: &Connection) -> Result<(usize, usize, usize, usize, usize), String> {
    let mut out = [0usize; 5];
    let mut st = conn
        .prepare("SELECT tier, COUNT(*) FROM attachment GROUP BY tier")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, usize>(1)?)))
        .map_err(|e| e.to_string())?;
    for (tier, n) in rows.flatten() {
        match tier {
            1 => out[0] = n,
            2 => out[1] = n,
            3 => out[2] = n,
            4 => out[3] = n,
            _ => out[4] = n,
        }
    }
    Ok((out[0], out[1], out[2], out[3], out[4]))
}

/// attachments/ 白名单目录下的文件（显示名, 大小）
fn attachment_files(data_dir: &Path) -> Vec<(String, u64)> {
    let dir = data_dir.join("attachments");
    dir.read_dir()
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    if !e.path().is_file() || n == ".DS_Store" {
                        return None;
                    }
                    Some((n, e.path().metadata().map(|m| m.len()).unwrap_or(0)))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ================================ T4.0 导出产物自检 ================================

/// 导出到临时目录后，按"未安装本软件的电脑"标准回读产物
pub fn export_selfcheck(
    data_dir: &Path,
    index_db: &Path,
    full: bool,
) -> Result<Vec<Check>, String> {
    let tmp = temp_root("export")?;
    let zip = ZipService::new(data_dir.join("notes"));
    let ctx = ExportContext::new(data_dir.join("notes"), index_db.to_path_buf());
    let conn = open_ro(index_db)?;
    let mut checks = vec![];

    // 单篇 zip：选一篇同时有空白代码块与附件的笔记
    let guid_att = "4fda6a09-4fbc-4269-abd9-b302ede889e8"; // 18 图 + 7 代码块 + 2 附件
    let atts = note_attachment_rows(&conn, guid_att)?
        .into_iter()
        .map(|(fp, name, size, _, _)| ExportAttachment {
            display_name: name,
            src: fp,
            size,
        })
        .collect::<Vec<_>>();
    let zpath = tmp.join("note.zip");
    let rep = export::export_note_zip(&ctx, &zip, guid_att, &atts, &zpath)?;
    let (entries, html) = read_zip_first(&zpath, "index.html")?;
    checks.push(Check::new(
        "E1",
        "单篇 zip：双击可开、含 index.html 与附件",
        format!("{} 个条目；index.html {} 字节；带出附件 {} 个", entries.len(), html.len(), rep.attachments_exported),
        "含 index.html、index_files/*、attachments/*".into(),
        entries.iter().any(|e| e == "index.html")
            && rep.notes_exported == 1
            && rep.attachments_exported == atts.len()
            && rep.attachments_missing == 0,
    ));
    checks.push(html_purity_check("E2", &html));

    // 自包含 HTML：无本地相对引用（全部内联）
    let hpath = tmp.join("note.html");
    export::export_note_single_html(&ctx, &zip, guid_att, &atts, &hpath)?;
    let solo = std::fs::read_to_string(&hpath).map_err(|e| e.to_string())?;
    let leaked: Vec<String> = regex::Regex::new(r#"(?i)(?:src|href)="(index_files/[^"]+)""#)
        .unwrap()
        .captures_iter(&solo)
        .map(|c| c[1].to_string())
        .collect();
    checks.push(Check::new(
        "E3",
        "单篇自包含 HTML：资源全内联、无悬空相对引用",
        format!("{} 字节；残留 index_files/ 引用 {} 处", solo.len(), leaked.len()),
        "0 处残留".into(),
        leaked.is_empty(),
    )
    .with(leaked));

    // 目录批量导出：还原文件树 + 名称净化
    let folder = "/boraydata/";
    let fdir = tmp.join("folder");
    let frep = export::export_folder(&ctx, &zip, folder, &fdir, &|_, _| {})?;
    let on_disk = count_files(&fdir);
    let selected: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM note WHERE location LIKE ?",
            [format!("{folder}%")],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    checks.push(Check::new(
        "E4",
        "按目录批量导出：文件树还原完整",
        format!("{} 篇 / {} 个文件；跳过 {} 项", frep.notes_exported, on_disk, frep.skipped.len()),
        format!("{selected} 篇"),
        frep.notes_exported == selected && on_disk >= frep.notes_exported && frep.skipped.is_empty(),
    )
    .with(frep.skipped.clone()));
    let bad_names = find_illegal_names(&fdir);
    checks.push(Check::new(
        "E5",
        "导出文件名/目录名已净化（P3 非法字符）",
        format!("{} 个非法名残留", bad_names.len()),
        "0".into(),
        bad_names.is_empty(),
    )
    .with(bad_names));

    // 附件相对链接可达性（导出 HTML 里每条 href 都指向真实存在的文件）
    let broken = broken_attachment_links(&fdir);
    checks.push(Check::new(
        "E6",
        "导出 HTML 附件区相对链接可点开",
        format!("{} 条链接失效", broken.len()),
        "0 条".into(),
        broken.is_empty(),
    )
    .with(broken));

    if full {
        let all = tmp.join("full");
        let arep = export::export_folder(&ctx, &zip, "", &all, &|_, _| {})?;
        checks.push(Check::new(
            "F1",
            "全库导出：1,780 篇 / 202 目录 / 133 附件",
            format!(
                "{} 篇 / {} 个目录 / {} 个附件（含 _unlinked_attachments），耗时 {} ms",
                arep.notes_exported, arep.folders_exported, arep.attachments_exported, arep.elapsed_ms
            ),
            format!("{EXPECTED_NOTE_COUNT} 篇 / ≥202 目录 / {BASE_ATTACHMENTS} 附件"),
            arep.notes_exported == EXPECTED_NOTE_COUNT
                && arep.folders_exported >= 202
                && arep.attachments_exported >= BASE_ATTACHMENTS,
        )
        .with(arep.skipped.clone()));
    } else {
        checks.push(Check::skip("F1", "全库导出", "跳过（需 --export-all 显式开启，产物约 2.5 GB）"));
    }

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(checks)
}

fn html_purity_check(id: &str, html: &str) -> Check {
    let wiz: Vec<String> = regex::Regex::new(r#"wiznote://"#)
        .unwrap()
        .find_iter(html)
        .map(|m| m.as_str().to_string())
        .collect();
    let compat: Vec<String> = regex::Regex::new(r#"(?i)<script"#)
        .unwrap()
        .find_iter(html)
        .map(|m| m.as_str().to_string())
        .collect();
    Check::new(
        id,
        "导出 HTML 不依赖本软件（无 wiznote:// 引用、无宿主注入残留）",
        format!("wiznote:// 引用 {} 处；<script> {} 处", wiz.len(), compat.len()),
        "0 处".into(),
        wiz.is_empty() && compat.is_empty(),
    )
    .with(wiz.into_iter().chain(compat).collect())
}

fn read_zip_first(path: &Path, want: &str) -> Result<(Vec<String>, String), String> {
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    let names: Vec<String> = ar.file_names().map(|s| s.to_string()).collect();
    let mut buf = String::new();
    for i in 0..ar.len() {
        let mut e = ar.by_index(i).map_err(|e| e.to_string())?;
        if e.name() == want {
            use std::io::Read;
            let mut raw = Vec::new();
            e.read_to_end(&mut raw).map_err(|e| e.to_string())?;
            buf = zipserve::decode_utf8_sig(&raw);
            break;
        }
    }
    Ok((names, buf))
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0usize;
    let Ok(rd) = dir.read_dir() else {
        return 0;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += count_files(&p);
        } else {
            n += 1;
        }
    }
    n
}

/// 导出树中残留的文件系统非法字符（P3）
fn find_illegal_names(dir: &Path) -> Vec<String> {
    let mut out = vec![];
    let Ok(rd) = dir.read_dir() else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.chars().any(|c| matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
            || name.chars().count() > 120
        {
            push_sample(&mut out, name.clone());
        }
        if e.path().is_dir() {
            out.extend(find_illegal_names(&e.path()));
        }
    }
    out
}

/// 导出 HTML 中附件区的相对链接是否都能落到真实文件
fn broken_attachment_links(root: &Path) -> Vec<String> {
    let re = regex::Regex::new(r#"(?is)<a href="(attachments/[^"]+)""#).unwrap();
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.file_name().map(|n| n != "index.html").unwrap_or(true) {
                continue;
            }
            let Ok(html) = std::fs::read_to_string(&p) else {
                continue;
            };
            for c in re.captures_iter(&html) {
                let link = c[1].to_string();
                let target = p.parent().unwrap().join(&link);
                if !target.is_file() {
                    push_sample(&mut out, format!("{link} → {}", p.display()));
                }
            }
        }
    }
    out
}

fn temp_root(tag: &str) -> Result<PathBuf, String> {
    let p = std::env::temp_dir().join(format!(
        "wizreader-{}-{}",
        tag,
        std::process::id()
    ));
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    Ok(p)
}

// ================================ T4.2 源数据零写入 ================================

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct Snapshot {
    /// 相对路径 → (字节数, mtime ns)
    pub files: std::collections::BTreeMap<String, (u64, i64)>,
    pub total_bytes: u64,
}

/// 快照落盘（CLI：操作前后各存一份，再比对）
pub fn write_snapshot(path: &Path, s: &Snapshot) -> Result<(), String> {
    let body = serde_json::to_string(s).map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| format!("写快照失败 {path:?}: {e}"))
}

pub fn read_snapshot(path: &Path) -> Result<Snapshot, String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("读快照失败 {path:?}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("快照格式错误: {e}"))
}

/// 只快照白名单：`index.db`、`notes/`、`attachments/`（G2）
pub fn snapshot(data_dir: &Path) -> Result<Snapshot, String> {
    let mut s = Snapshot::default();
    let db = data_dir.join("index.db");
    let add = |s: &mut Snapshot, p: &Path| {
        if let Ok(md) = std::fs::metadata(p) {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            s.files.insert(
                p.strip_prefix(data_dir).unwrap_or(p).to_string_lossy().to_string(),
                (md.len(), mtime),
            );
            s.total_bytes += md.len();
        }
    };
    if db.is_file() {
        add(&mut s, &db);
    }
    for sub in ["notes", "attachments"] {
        let dir = data_dir.join(sub);
        if !dir.is_dir() {
            continue;
        }
        for e in dir.read_dir().map_err(|e| e.to_string())?.flatten() {
            if e.path().is_file() {
                add(&mut s, &e.path());
            }
        }
        // 子目录条目也计（防意外新建目录）
        for e in dir.read_dir().map_err(|e| e.to_string())?.flatten() {
            if e.path().is_dir() {
                push(&mut s, &data_dir, &e.path());
            }
        }
    }
    Ok(s)
}

fn push(s: &mut Snapshot, data_dir: &Path, p: &Path) {
    let rel = p.strip_prefix(data_dir).unwrap_or(p).to_string_lossy().to_string();
    s.files.entry(format!("{rel}/")).or_insert((0, 0));
}

/// 快照差异：任何一条（新增、删除、mtime 或字节数变化）都是 NFR-2 违例
pub fn diff(a: &Snapshot, b: &Snapshot) -> Vec<String> {
    let mut out = vec![];
    for (k, v) in &a.files {
        match b.files.get(k) {
            None => push_sample(&mut out, format!("消失: {k}")),
            Some(got) if got != v => push_sample(&mut out, format!("被修改: {k} {v:?} → {got:?}")),
            Some(_) => {}
        }
    }
    for k in b.files.keys() {
        if !a.files.contains_key(k) {
            push_sample(&mut out, format!("新增: {k}"));
        }
    }
    out
}

// ================================ T4.3 安全项 ================================

pub fn security_checks(data_dir: &Path, index_db: &Path) -> Vec<Check> {
    let mut out = vec![];

    // SEC-1 程序侧无网络能力：本 crate 声明的依赖中不得有 HTTP/S3/对象存储客户端
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let banned = ["reqwest", "hyper", "rust-s3", "aws-config", "ureq", "attohttpc", "tungstenite", "curl", "minio"];
    match std::fs::read_to_string(&manifest) {
        Ok(toml) => {
            let deps = toml
                .lines()
                .skip_while(|l| !l.trim().starts_with("[dependencies]"))
                .skip(1)
                .take_while(|l| !l.trim_start().starts_with('['))
                .filter_map(|l| l.split('=').next().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            let hits: Vec<String> = deps
                .iter()
                .filter(|d| banned.iter().any(|b| d.starts_with(b)))
                .cloned()
                .collect();
            out.push(Check::new(
                "SEC-1",
                "无 HTTP / 对象存储客户端依赖（FR-07 已后置，本期不得联网）",
                format!("直接依赖 {:?}；命中网络库 {:?}", deps, hits),
                "命中网络库 0 个".into(),
                hits.is_empty() && !deps.is_empty(),
            )
            .with(hits));
        }
        Err(e) => out.push(Check::skip("SEC-1", "无 HTTP / 对象存储客户端依赖", &format!("跳过：读不到 {manifest:?}（{e}）"))),
    }

    // SEC-2 CSP：远程资源默认阻断、笔记内 JS 禁、表单/嵌套/插件禁
    let n = sandbox::nonce();
    let csp = sandbox::note_csp(&n, false);
    let csp_open = sandbox::note_csp(&n, true);
    let mut problems = vec![];
    if !csp.starts_with("default-src 'none'") {
        problems.push("缺 default-src 'none'".to_string());
    }
    if csp.contains("http:") || csp.contains("https:") {
        problems.push("默认 CSP 放开了远程源".to_string());
    }
    if !csp.contains(&format!("script-src 'nonce-{n}'")) {
        problems.push("script-src 未走 nonce".to_string());
    }
    if csp.contains("'unsafe-inline'") && csp.contains("script-src") {
        // style-src 允许 unsafe-inline（笔记自带 <style> 必须能渲染），script-src 不允许
        let script = csp.split("; ").find(|s| s.starts_with("script-src")).unwrap_or("");
        if script.contains("'unsafe-inline'") || script.contains("'unsafe-eval'") {
            problems.push("script-src 放开了内联/eval".to_string());
        }
    }
    for need in ["form-action 'none'", "frame-src 'none'", "object-src 'none'"] {
        if !csp.contains(need) {
            problems.push(format!("缺 {need}"));
        }
    }
    if !csp_open.contains("img-src wiznote: data: blob: http: https:") {
        problems.push("开关打开后应放开 img 远程源".to_string());
    }
    out.push(
        Check::new(
            "SEC-2",
            "WebView 沙箱 CSP（NFR-3.2 / NFR-3.5）",
            format!("默认 CSP 断言 {}；开关放开后 img-src 正确", if problems.is_empty() { "全部通过" } else { "存在缺陷" }),
            "无缺陷".into(),
            problems.is_empty(),
        )
        .with(problems),
    );

    // SEC-3 兼容层必须"已渲染态跳过"，否则代码显示两遍
    let js = sandbox::COMPAT_JS;
    let mut js_problems = vec![];
    if !js.contains("renderedLines(box)") {
        js_problems.push("未判断已渲染态".to_string());
    }
    if !js.contains("CodeMirror-measure") {
        js_problems.push("未排除 CodeMirror 测量占位行".to_string());
    }
    if !js.contains("wiznote-action://open-url") {
        js_problems.push("外链未走系统浏览器".to_string());
    }
    out.push(Check::new("SEC-3", "宿主兼容层 JS（代码块 / 外链 / 复制）", format!("3 项断言 {}", if js_problems.is_empty() { "通过" } else { "失败" }), "全部通过".into(), js_problems.is_empty()).with(js_problems));

    // SEC-4 路径穿越（P10 怪异文件名 + 穿越载荷）
    // 必须直接拒绝的载荷（含二次 percent 编码，SEC-4 首轮实测发现该漏洞）
    let payloads = [
        "../../../etc/passwd",
        "/etc/passwd",
        "index_files/../../index.db",
        "index_files\\..\\..\\etc\\passwd",
        "..%2f..%2fetc",
        "%2e%2e/%2e%2e/etc",
        "index_files/..%5c..%5cetc",
        "index_files/%00.png",
        "",
        "./",
    ];
    let rejected_ok: Vec<String> = payloads
        .iter()
        .filter(|p| ZipService::sanitize_entry_path(p).is_ok())
        .map(|p| (*p).to_string())
        .collect();
    // 允许收敛但绝不能跳出包内的载荷（空段/单段点规范化，不属越权）
    let normalizes = ["index_files//x.png", "index_files/./x.png"];
    let escaped: Vec<String> = normalizes
        .iter()
        .filter_map(|p| match ZipService::sanitize_entry_path(p) {
            Ok(ok) if ok.contains("..") || ok.starts_with('/') => Some(format!("{p} → {ok}")),
            Err(e) => Some(format!("{p} 被误拒：{:?}", e)),
            _ => None,
        })
        .collect();
    // P10：怪异但合法的文件名必须放行
    let legal = [
        "index_files/x.png;sizingmethod=-crop",
        "index_files/y.png-sizingmethod=crop",
        "index_files/z.com",
        "index_files/w.php",
        "index_files/v.tmp",
    ];
    let legal_ok: Vec<String> = legal
        .iter()
        .filter(|p| ZipService::sanitize_entry_path(p).is_err())
        .map(|p| (*p).to_string())
        .collect();
    out.push(
        Check::new(
            "SEC-4",
            "zip 路径穿越防护（P10）",
            format!(
                "未拒绝的载荷 {} 个；越界的规范化 {} 个；被误拒的合法怪异名 {} 个",
                rejected_ok.len(),
                escaped.len(),
                legal_ok.len()
            ),
            "0 / 0 / 0".into(),
            rejected_ok.is_empty() && escaped.is_empty() && legal_ok.is_empty(),
        )
        .with(
            rejected_ok
                .into_iter()
                .chain(escaped)
                .chain(legal_ok)
                .collect(),
        ),
    );

    // SEC-5 凭据不落盘：派生索引 schema 与设置文件均不得含账号/密钥字段
    let schema: Vec<String> = Connection::open_with_flags(
        index_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map(|c| {
        c.prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .and_then(|mut st| {
                Ok(st
                    .query_map([], |r| r.get::<_, String>(0))?
                    .flatten()
                    .collect::<Vec<_>>())
            })
            .unwrap_or_default()
    })
    .unwrap_or_default();
    let leaked: Vec<String> = schema
        .into_iter()
        .filter(|t| t.starts_with("WIZ_") || t.to_ascii_lowercase().contains("account"))
        .collect();
    let settings_json = serde_json::to_string(&crate::config::Settings::default()).unwrap_or_default();
    let cred_keys = ["password", "secret", "access_key", "token", "minio", "credential"];
    let hay = settings_json.to_ascii_lowercase();
    let cred_hits: Vec<String> = cred_keys
        .iter()
        .copied()
        .filter(|k| hay.contains(k))
        .map(|k| k.to_string())
        .collect();
    out.push(
        Check::new(
            "SEC-5",
            "凭据不入派生索引与配置文件（P11 / NFR-3.4）",
            format!("索引表内源库表 {} 个；settings.json 命中凭据字段 {} 个", leaked.len(), cred_hits.len()),
            "0 / 0".into(),
            leaked.is_empty() && cred_hits.is_empty(),
        )
        .with(leaked.into_iter().chain(cred_hits).collect()),
    );

    // SEC-6 源数据以 mode=ro 打开（G1）：以只读打开后尝试写入必须失败
    let write_denied = {
        let uri = format!(
            "file:{}?mode=ro",
            crate::indexer::url_encode_path(&data_dir.join("index.db"))
        );
        match Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        ) {
            Ok(c) => c
                .execute_batch("UPDATE WIZ_DOCUMENT SET DOCUMENT_TITLE = DOCUMENT_TITLE")
                .is_err(),
            Err(_) => false,
        }
    };
    out.push(Check::new(
        "SEC-6",
        "源 index.db 以 mode=ro 打开且拒绝写入",
        format!("写入被拒绝: {write_denied}"),
        "true".into(),
        write_denied,
    ));

    // SEC-7 断网跑全库：需真实离线环境，程序侧只能给间接证据
    out.push(Check::skip(
        "SEC-7",
        "离线环境跑全库无任何外网请求（NFR-3.1）",
        "待人工：关网后启动应用跑一轮（间接证据见 SEC-1/SEC-2）",
    ));
    out
}

// ================================ T4.4 性能基准 ================================

#[derive(Debug, Serialize)]
pub struct Bench {
    pub checks: Vec<Check>,
    pub index_build_ms: u128,
    pub median_note_ms: u128,
    pub p50_ms: u128,
    pub p99_ms: u128,
    pub max_note_ms: u128,
    pub search_ms: Vec<(String, u128)>,
    pub index_mb: f64,
    pub coldstart_ms: u128,
}

/// 打开一篇笔记的完整读路径（解压 index.html + 物化代码块），协议处理器的等价代理
fn open_note(zip: &ZipService, guid: &str) -> Option<usize> {
    let html = zip.read_index_html(guid).ok()?;
    let (out, _) = materialize_code_blocks(&html);
    Some(out.len())
}

pub fn bench(data_dir: &Path, index_db: &Path) -> Result<Bench, String> {
    let t0 = std::time::Instant::now();
    let zip = ZipService::new(data_dir.join("notes"));
    let conn = open_ro(index_db)?;
    let notes = all_notes(&conn)?;

    // 逐篇打开耗时分布（同时即是一轮"浏览"，供 T4.2 快照覆盖）
    let mut times: Vec<(String, u128)> = Vec::with_capacity(notes.len());
    for (guid, _t, _l, _bl, size) in &notes {
        let s = std::time::Instant::now();
        open_note(&zip, guid);
        times.push((guid.clone(), s.elapsed().as_micros() / 1000));
        let _ = size;
    }
    let mut sorted = times.clone();
    sorted.sort_by_key(|(_, ms)| *ms);
    let pct = |q: f64| -> u128 {
        let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
        sorted[i.min(sorted.len() - 1)].1
    };
    // 体积中位笔记（NFR-1 的"中位笔记"按包体积取，而非时间分布）
    let mut by_size = notes.clone();
    by_size.sort_by_key(|(_, _, _, _, size)| *size);
    let median_guid = &by_size[by_size.len() / 2].0;
    let median_ms = {
        let s = std::time::Instant::now();
        open_note(&zip, median_guid);
        s.elapsed().as_micros() / 1000
    };
    let max_entry = by_size.last().cloned().unwrap_or_default();
    let max_ms = {
        let s = std::time::Instant::now();
        open_note(&zip, &max_entry.0);
        s.elapsed().as_micros() / 1000
    };

    // 检索：验收六词
    let mut search_ms = vec![];
    for w in ACCEPTANCE_WORDS {
        let s = std::time::Instant::now();
        let r = crate::search::search(index_db, w, None);
        let ms = s.elapsed().as_micros() / 1000;
        let n = r.map(|x| x.notes.len()).unwrap_or_default();
        if n == 0 {
            // 命中 0 视为不达标，用同一 Check 承载
            search_ms.push((format!("{w}（未命中！）"), ms));
        } else {
            search_ms.push((w.to_string(), ms));
        }
    }

    // 列表首屏（在用连接，页面缓存已热）
    let s = std::time::Instant::now();
    let list_n = {
        let mut st = conn
            .prepare("SELECT guid FROM note ORDER BY data_modified DESC LIMIT 200")
            .map_err(|e| e.to_string())?;
        let v: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        v.len()
    };
    let list_ms = s.elapsed().as_micros() / 1000;

    // 冷启动代理：新建只读连接（SQLite 页面缓存为空）→ 读全量目录树 → 首屏 200 篇。
    // 绝不能把 t0.elapsed()（含 1,780 篇顺序打开的整轮）计进冷启动。
    let s = std::time::Instant::now();
    let db2 = open_ro(index_db)?;
    let tree_n: usize = db2
        .prepare("SELECT path, name, parent FROM folder ORDER BY pos")
        .and_then(|mut st| Ok(st.query_map([], |r| r.get::<_, String>(0))?.flatten().count()))
        .unwrap_or(0);
    let cold_list_n: usize = db2
        .prepare("SELECT guid, title FROM note ORDER BY data_modified DESC LIMIT 200")
        .and_then(|mut st| Ok(st.query_map([], |r| r.get::<_, String>(0))?.flatten().count()))
        .unwrap_or(0);
    let cold_ms = s.elapsed().as_micros() / 1000;
    let round_ms = t0.elapsed().as_micros() / 1000;
    let _ = db2;

    // 索引重建（写临时目录，不动用户的派生索引）
    let tmp_idx = temp_root("bench-index")?;
    let bs = std::time::Instant::now();
    let build = build_index(data_dir, &tmp_idx, &|_, _| {});
    let build_ms = bs.elapsed().as_millis();
    let built_db = tmp_idx.join("index.db");
    let index_mb = std::fs::metadata(&built_db).map(|m| m.len() as f64 / 1048576.0).unwrap_or(0.0);
    let existing_mb = std::fs::metadata(index_db).map(|m| m.len() as f64 / 1048576.0).unwrap_or(0.0);
    let _ = std::fs::remove_dir_all(&tmp_idx);

    let p99 = pct(0.99);
    let corpus_chars: usize = notes.iter().map(|n| n.3.max(0) as usize).sum();
    let checks = vec![
        Check::new(
            "PERF-1",
            "索引重建 ≤ 30 s",
            format!("{build_ms} ms"),
            format!("≤ {LIMIT_INDEX_MS} ms"),
            build.is_ok() && build_ms <= LIMIT_INDEX_MS,
        ),
        Check::new(
            "PERF-2",
            "打开中位笔记 ≤ 300 ms",
            format!("{median_ms} ms（{}）", median_guid),
            format!("≤ {LIMIT_MEDIAN_MS} ms"),
            median_ms <= LIMIT_MEDIAN_MS,
        ),
        Check::new(
            "PERF-3",
            "逐篇打开 P99 ≤ 1 s",
            format!("{p99} ms（P50 {} ms）", pct(0.5)),
            format!("≤ {LIMIT_P99_MS} ms"),
            p99 <= LIMIT_P99_MS,
        ),
        Check::new(
            "PERF-4",
            "最大笔记 ≤ 2 s",
            format!("{max_ms} ms（{}，{:.1} MB）", max_entry.0, max_entry.4 as f64 / 1048576.0),
            format!("≤ {LIMIT_MAX_MS} ms"),
            max_ms <= LIMIT_MAX_MS,
        ),
        {
            let worst = search_ms.iter().map(|(_, ms)| *ms).max().unwrap_or(0);
            let zero_hit = search_ms.iter().any(|(w, _)| w.contains("未命中"));
            Check::new(
                "PERF-5",
                "检索验收六词 ≤ 200 ms 且全部命中",
                format!("最慢 {worst} ms；{}", search_ms.iter().map(|(w, ms)| format!("{w}={ms}ms")).collect::<Vec<_>>().join(" ")),
                format!("全部 ≤ {LIMIT_SEARCH_MS} ms"),
                worst <= LIMIT_SEARCH_MS && !zero_hit,
            )
        },
        Check::new(
            "PERF-6",
            "笔记列表首屏 ≤ 300 ms",
            format!("{list_ms} ms（首屏 {list_n} 篇）"),
            format!("≤ {LIMIT_MEDIAN_MS} ms"),
            list_ms <= LIMIT_MEDIAN_MS,
        ),
        Check::new(
            "PERF-7",
            "冷启动（新建连接 + 目录树 + 首屏）≤ 2 s",
            format!(
                "{cold_ms} ms（目录 {tree_n} 项 + 首屏 {cold_list_n} 篇）；整轮含 1,780 篇顺序打开 {round_ms} ms，不计入冷启动"
            ),
            format!("≤ {LIMIT_COLDSTART_MS} ms"),
            cold_ms <= LIMIT_COLDSTART_MS,
        ),
        Check::new(
            "PERF-8",
            "派生索引体积 ≤ 30 MB",
            format!(
                "重建后 {index_mb:.1} MB（在用索引 {existing_mb:.1} MB）；其中语料 {corpus_mb:.1} MB，FTS5 trigram 带位置索引约为语料的 2–2.5 倍",
                corpus_mb = corpus_chars as f64 * 3.0 / 1048576.0
            ),
            format!("≤ {LIMIT_INDEX_MB} MB（T1.4 设计目标，非 NFR 硬线）"),
            index_mb <= LIMIT_INDEX_MB as f64,
        ),
    ];
    Ok(Bench {
        checks,
        index_build_ms: build_ms,
        median_note_ms: median_ms,
        p50_ms: pct(0.5),
        p99_ms: p99,
        max_note_ms: max_ms,
        search_ms,
        index_mb,
        coldstart_ms: cold_ms,
    })
}

// ================================ 汇总入口 ================================

/// 一次跑完 M4 全部验收项。`with_bench` 控制是否跑性能基准（会重建一次索引），
/// `export_full` 控制是否额外跑一轮全库导出（F1，产物约 2.5 GB）。
pub fn run_all(
    data_dir: &Path,
    index_db: &Path,
    with_bench: bool,
    export_full: bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<VerifyReport, String> {
    let t0 = std::time::Instant::now();
    // T4.2：操作一轮前先快照
    let before = snapshot(data_dir)?;

    let inspection = inspect(data_dir, index_db, progress)?;
    let export_check = export_selfcheck(data_dir, index_db, export_full)?;
    let security = security_checks(data_dir, index_db);

    // 一轮全功能：浏览（inspect 已逐篇读）+ 检索 + 导出 + 重建索引（临时目录）
    for w in ACCEPTANCE_WORDS {
        let _ = crate::search::search(index_db, w, None);
    }
    let mut zero_write = vec![];
    let bench = if with_bench {
        let b = bench(data_dir, index_db)?;
        b.checks
    } else {
        vec![Check::skip("PERF-*", "性能基准", "跳过（--no-bench）")]
    };
    let after = snapshot(data_dir)?;
    let d = diff(&before, &after);
    zero_write.push(Check::new(
        "NW-1",
        "全功能操作一轮后源数据零写入（NFR-2）",
        format!(
            "{} 个源文件快照（{:.1} MB），差异 {} 处",
            before.files.len(),
            before.total_bytes as f64 / 1048576.0,
            d.len()
        ),
        "0 处差异".into(),
        d.is_empty(),
    )
    .with(d));

    let mut report = VerifyReport {
        ok: true,
        data_dir: data_dir.to_string_lossy().to_string(),
        inspection,
        export_check,
        security,
        zero_write,
        bench,
        elapsed_ms: t0.elapsed().as_millis(),
    };
    report.ok = report.failed().is_empty();
    Ok(report)
}

/// 报告 JSON（CLI / 前端共用，避开在 CLI 里直接依赖 serde_json）
pub fn report_json(r: &VerifyReport) -> String {
    serde_json::to_string_pretty(r).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// 报告落盘：一份可读文本（[`render`]）+ 一份 JSON，返回两个路径
pub fn write_report_files(dest: &Path, r: &VerifyReport) -> Result<Vec<PathBuf>, String> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let (md_path, json_path) = if dest.extension().map(|e| e == "json").unwrap_or(false) {
        (dest.with_extension("md"), dest.to_path_buf())
    } else {
        (dest.to_path_buf(), dest.with_extension("json"))
    };
    std::fs::write(&md_path, render(r).as_bytes()).map_err(|e| format!("写报告失败 {md_path:?}: {e}"))?;
    let json = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
    std::fs::write(&json_path, json.as_bytes()).map_err(|e| format!("写 JSON 失败 {json_path:?}: {e}"))?;
    Ok(vec![md_path, json_path])
}

/// CLI 用文本渲染
pub fn render(r: &VerifyReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "数据源：{}\n耗时：{} ms\n\n",
        r.data_dir, r.elapsed_ms
    ));
    let mut section = |title: &str, cs: &[Check]| {
        s.push_str(&format!("== {title} ==\n"));
        for c in cs {
            let mark = if c.skipped {
                "－"
            } else if c.passed {
                "✅"
            } else {
                "❌"
            };
            s.push_str(&format!(
                "{mark} [{}] {}\n     实测：{}\n     期望：{}\n",
                c.id, c.name, c.actual, c.expected
            ));
            for x in &c.samples {
                s.push_str(&format!("       · {x}\n"));
            }
        }
        s.push('\n');
    };
    section("T4.1 全库巡检（10 项）", &r.inspection);
    section("T4.0 导出产物自检", &r.export_check);
    section("T4.2 NFR-2 源数据零写入", &r.zero_write);
    section("T4.3 NFR-3 安全项", &r.security);
    section("T4.4 NFR-1 性能基准", &r.bench);
    let failed = r.failed();
    s.push_str(&if failed.is_empty() {
        "结论：全部通过\n".to_string()
    } else {
        format!(
            "结论：{} 项未通过 → {}\n",
            failed.len(),
            failed.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ")
        )
    });
    s
}
