//! 命令行工具（M1 + M4）：
//!   wiz-cli build-index <data_dir> [--index-dir <dir>]
//!   wiz-cli search <kw> [--folder <path>]
//!   wiz-cli peek <guid> [entry]                          读取笔记 zip 内文件（协议链路验证）
//!   wiz-cli verify [--export-all] [--no-bench] [--json] [--out <file>]   M4 全库验收巡检
//!   wiz-cli snapshot [--out <file>]                      源数据快照（T4.2 前置）
//!   wiz-cli snapshot-diff <a.json> <b.json>              两份快照比对（NFR-2）
//!   wiz-cli bench                                          NFR-1 性能基准
//!   wiz-cli export-note <guid> [--html <dest.html>]       单篇导出（默认 zip）
//!   wiz-cli export-folder "<location>" <dest_dir>         按目录批量导出
//!   wiz-cli export-all <dest_dir>                          全库按目录树导出

use std::path::PathBuf;

use wizreader_lib::export::{ExportContext, ExportReport};
use wizreader_lib::verify;
use wizreader_lib::zipserve::ZipService;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("build-index") => {
            let data_dir = args
                .get(1)
                .filter(|s| !s.starts_with("--"))
                .map(PathBuf::from)
                .or_else(|| flag_value(&args, "--data-dir").map(PathBuf::from))
                .or_else(default_data_dir)
                .unwrap_or_else(|| {
                    eprintln!("未指定数据源目录：用法 wiz-cli build-index <data_dir>，或先在应用内设置数据源");
                    std::process::exit(1);
                });
            let index_dir = flag_value(&args, "--index-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::var("HOME")
                        .map(|h| PathBuf::from(h).join(".wizreader"))
                        .unwrap()
                });
            println!("数据源: {}", data_dir.display());
            println!("索引目录: {}", index_dir.display());
            let t0 = std::time::Instant::now();
            match wizreader_lib::indexer::build_index(&data_dir, &index_dir, &|done, total| {
                eprint!("\r索引进度: {}/{} 篇", done, total);
            }) {
                Ok(r) => {
                    eprintln!();
                    println!("{:#?}", r);
                    if !r.ok {
                        eprintln!("⚠️ 校验报告存在偏离项（见 warnings），请核对源数据或匹配逻辑");
                        std::process::exit(2);
                    }
                    println!("✅ 索引构建完成，耗时 {} ms（目标 ≤ 30000 ms）", t0.elapsed().as_millis());
                }
                Err(e) => {
                    eprintln!("❌ 构建失败: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some("peek") => {
            let guid = match args.get(1) {
                Some(k) => k.clone(),
                None => {
                    eprintln!("用法: wiz-cli peek <guid> [entry] [--data-dir <path>]");
                    std::process::exit(1);
                }
            };
            let entry = args
                .iter()
                .skip(2)
                .find(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| "index.html".into());
            let data_dir = flag_value(&args, "--data-dir")
                .map(PathBuf::from)
                .or_else(default_data_dir)
                .unwrap_or_else(|| {
                    eprintln!("未找到数据源目录：请先在应用内设置，或用 --data-dir 指定");
                    std::process::exit(1);
                });
            let zs = wizreader_lib::zipserve::ZipService::new(data_dir.join("notes"));
            match if entry == "index.html" {
                zs.read_index_html(&guid)
            } else {
                zs.read_entry(&guid, &entry)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .map_err(wizreader_lib::zipserve::ZipError::from)
            } {
                Ok(html) => {
                    println!("✅ {} bytes\n---- 前 600 字符 ----\n{}", html.len(), &html[..html.len().min(600)]);
                }
                Err(e) => {
                    eprintln!("❌ 读取失败: {:?}", e);
                    std::process::exit(1);
                }
            }
        }
        Some("search") => {
            let kw = match args.get(1) {
                Some(k) => k.clone(),
                None => {
                    eprintln!("用法: wiz-cli search <kw> [--folder <path>]");
                    std::process::exit(1);
                }
            };
            let index_db = flag_value(&args, "--index-dir")
                .map(|d| PathBuf::from(d).join("index.db"))
                .unwrap_or_else(|| {
                    std::env::var("HOME")
                        .map(|h| PathBuf::from(h).join(".wizreader/index.db"))
                        .unwrap()
                });
            let folder = flag_value(&args, "--folder");
            match wizreader_lib::search::search(&index_db, &kw, folder.as_deref()) {
                Ok(resp) => {
                    println!(
                        "检索 \"{}\"：{} 篇笔记 / {} 个附件，耗时 {} ms（目标 ≤ 200 ms）",
                        resp.kw,
                        resp.notes.len(),
                        resp.attachments.len(),
                        resp.elapsed_ms
                    );
                    for n in resp.notes.iter().take(20) {
                        let dup = if n.dup_count > 1 {
                            format!("（另有 {} 篇相同内容）", n.dup_count - 1)
                        } else {
                            String::new()
                        };
                        println!("  [{}] {} @ {} {}", n.guid, n.title, n.location, dup);
                        let plain = n.snippet.replace("<mark>", "«").replace("</mark>", "»");
                        if !plain.is_empty() {
                            println!("      …{}…", plain.replace('\n', " "));
                        }
                    }
                    for a in &resp.attachments {
                        let tag = if a.tier == 0 {
                            "（文件未随导出下载）"
                        } else if a.document_guid.is_none() {
                            "（未关联附件）"
                        } else {
                            ""
                        };
                        println!("  [附件] {} tier={} {}", a.display_name, a.tier, tag);
                    }
                }
                Err(e) => {
                    eprintln!("❌ 检索失败: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some("verify") => cmd_verify(&args),
        Some("snapshot") => cmd_snapshot(&args),
        Some("snapshot-diff") => cmd_snapshot_diff(&args),
        Some("bench") => cmd_bench(&args),
        Some("export-note") => cmd_export_note(&args),
        Some("export-folder") => cmd_export_folder(&args, false),
        Some("export-all") => cmd_export_folder(&args, true),
        Some("export-zips") => cmd_export_zips(&args),
        Some("manifest-rebuild") => cmd_manifest_rebuild(&args),
        _ => {
            eprintln!("WizReader CLI");
            eprintln!("  wiz-cli build-index <data_dir> [--index-dir <dir>]   构建派生索引并输出校验报告");
            eprintln!("  wiz-cli search <kw> [--folder <path>]                全文检索（≥ 3 字符走 FTS5 trigram）");
            eprintln!("  wiz-cli peek <guid> [entry] [--data-dir <path>]    读取笔记 zip 内文件（协议链路验证）");
            eprintln!("  wiz-cli verify [--export-all] [--no-bench] [--json] [--out <file>]   M4 全库验收巡检");
            eprintln!("  wiz-cli snapshot [--out <file>]                      源数据快照（NFR-2 前置）");
            eprintln!("  wiz-cli snapshot-diff <a.json> <b.json>              快照比对");
            eprintln!("  wiz-cli bench                                        NFR-1 性能基准");
            eprintln!("  wiz-cli export-note <guid> [--html <dest.html>]      单篇导出 zip/自包含 HTML");
            eprintln!("  wiz-cli export-folder \"<location>\" <dest_dir>          按目录批量导出");
            eprintln!("  wiz-cli export-all <dest_dir>                        全库导出（逃生舱）");
            eprintln!("  wiz-cli export-zips <dest_dir> [--slim] [--folder <location>]  每篇一个 zip（--slim= FR-02 存储瘦身）");
            eprintln!("  wiz-cli manifest-rebuild <dest_dir> [--mode native|slim]      重建导出目录清单 export.db（丢失后用）");
        }
    }
}

// ---------------- M4 子命令 ----------------

/// (数据源目录, 派生索引)；可用 --data-dir / --index-dir 覆盖
fn resolve_dirs(args: &[String]) -> Result<(PathBuf, PathBuf), String> {
    let data_dir = flag_value(args, "--data-dir")
        .map(PathBuf::from)
        .or_else(default_data_dir)
        .ok_or("未指定数据源目录（--data-dir 或先在应用内设置）")?;
    let index_db = flag_value(args, "--index-dir")
        .map(|d| PathBuf::from(d).join("index.db"))
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".wizreader/index.db"))
                .unwrap()
        });
    if !index_db.is_file() {
        return Err(format!("派生索引不存在：{}（先跑 wiz-cli build-index）", index_db.display()));
    }
    Ok((data_dir, index_db))
}

fn print_report(args: &[String], report: &verify::VerifyReport, text: &str) {
    if args.iter().any(|a| a == "--json") {
        println!("{}", verify::report_json(report));
    } else {
        println!("{text}");
    }
    if let Some(dest) = flag_value(args, "--out") {
        match verify::write_report_files(std::path::Path::new(&dest), report) {
            Ok(paths) => {
                for p in &paths {
                    eprintln!("报告已写入 {}", p.display());
                }
            }
            Err(e) => eprintln!("❌ 写报告失败: {e}"),
        }
    }
}

fn cmd_verify(args: &[String]) {
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let with_bench = !args.iter().any(|a| a == "--no-bench");
    let export_full = args.iter().any(|a| a == "--export-all");
    eprintln!(
        "数据源: {}\n派生索引: {}\n基准: {}  全库导出: {}",
        data_dir.display(),
        index_db.display(),
        if with_bench { "跑" } else { "跳过" },
        export_full
    );
    let report = match verify::run_all(&data_dir, &index_db, with_bench, export_full, &|stage, done, total| {
        if done % 100 == 0 || done == total {
            eprint!("\r{stage}: {}/{} 篇", done, total);
        }
    }) {
        Ok(r) => {
            eprintln!();
            r
        }
        Err(e) => {
            eprintln!("❌ 巡检失败: {e}");
            std::process::exit(1);
        }
    };
    let text = verify::render(&report);
    print_report(args, &report, &text);
    if !report.ok {
        std::process::exit(2);
    }
}

fn cmd_snapshot(args: &[String]) {
    let (data_dir, _) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let snap = match verify::snapshot(&data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ 快照失败: {e}");
            std::process::exit(1);
        }
    };
    let dest = flag_value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            std::env::temp_dir().join(format!("wizreader-snapshot-{ts}.json"))
        });
    if let Err(e) = verify::write_snapshot(&dest, &snap) {
        eprintln!("❌ {e}");
        std::process::exit(1);
    }
    println!(
        "✅ 快照 {} 个条目 / {:.1} MB → {}",
        snap.files.len(),
        snap.total_bytes as f64 / 1048576.0,
        dest.display()
    );
}

fn cmd_snapshot_diff(args: &[String]) {
    let (a, b) = match (args.get(1), args.get(2)) {
        (Some(x), Some(y)) => (PathBuf::from(x), PathBuf::from(y)),
        _ => {
            eprintln!("用法: wiz-cli snapshot-diff <before.json> <after.json>");
            std::process::exit(1);
        }
    };
    let sa = match verify::read_snapshot(&a) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let sb = match verify::read_snapshot(&b) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let d = verify::diff(&sa, &sb);
    if d.is_empty() {
        println!("✅ NFR-2 通过：{} 个源文件 mtime + 字节数零变化", sa.files.len());
    } else {
        println!("❌ 源数据被改动 {} 处：", d.len());
        for x in d {
            println!("   · {x}");
        }
        std::process::exit(2);
    }
}

fn cmd_bench(args: &[String]) {
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    match verify::bench(&data_dir, &index_db) {
        Ok(b) => {
            for c in &b.checks {
                let mark = if c.passed { "✅" } else { "❌" };
                println!("{mark} [{}] {}：{}（期望 {}）", c.id, c.name, c.actual, c.expected);
            }
            println!(
                "P50={} ms P99={} ms 最大={} ms 索引重建={} ms 索引体积={:.1} MB",
                b.p50_ms, b.p99_ms, b.max_note_ms, b.index_build_ms, b.index_mb
            );
            if !b.checks.iter().all(|c| c.passed) {
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("❌ 基准跑失败: {e}");
            std::process::exit(1);
        }
    }
}

fn show_report(rep: &ExportReport) {
    println!(
        "✅ 导出完成：{} 篇 / {} 个附件（{} 个附件文件缺失）/ {} 个目录 / 物化代码块 {}，耗时 {} ms",
        rep.notes_exported,
        rep.attachments_exported,
        rep.attachments_missing,
        rep.folders_exported,
        rep.code_blocks_materialized,
        rep.elapsed_ms
    );
    if !rep.skipped.is_empty() {
        println!("⚠️ 跳过 {} 项：", rep.skipped.len());
        for s in rep.skipped.iter().take(20) {
            println!("   · {s}");
        }
    }
}

fn cmd_export_note(args: &[String]) {
    let guid = match args.get(1) {
        Some(g) => g.clone(),
        None => {
            eprintln!("用法: wiz-cli export-note <guid> [--html <dest.html>] [--zip <dest.zip>]");
            std::process::exit(1);
        }
    };
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let ctx = ExportContext::new(data_dir.join("notes"), index_db);
    let atts = match wizreader_lib::export::note_attachments(&ctx, &guid) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ 读附件清单失败: {e}");
            std::process::exit(1);
        }
    };
    let zip = ZipService::new(data_dir.join("notes"));
    let rep = if let Some(dest) = flag_value(args, "--html") {
        wizreader_lib::export::export_note_single_html(&ctx, &zip, &guid, &atts, std::path::Path::new(&dest))
    } else {
        let dest = flag_value(args, "--zip").unwrap_or_else(|| {
            let title = wizreader_lib::export::note_title(&ctx, &guid);
            let safe = wizreader_lib::extract::sanitize_title(&title);
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(format!("{safe}.zip"))
                .to_string_lossy()
                .into_owned()
        });
        wizreader_lib::export::export_note_zip(&ctx, &zip, &guid, &atts, std::path::Path::new(&dest))
    };
    match rep {
        Ok(r) => show_report(&r),
        Err(e) => {
            eprintln!("❌ 导出失败: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_export_folder(args: &[String], all: bool) {
    let location = if all {
        String::new()
    } else {
        match args.get(1) {
            Some(l) => l.clone(),
            None => {
                eprintln!("用法: wiz-cli export-folder \"/My Notes/\" <dest_dir>");
                std::process::exit(1);
            }
        }
    };
    let dest_idx = if all { 1 } else { 2 };
    let dest = match args.get(dest_idx) {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!("用法: wiz-cli {} <dest_dir>", if all { "export-all" } else { "export-folder <location>" });
            std::process::exit(1);
        }
    };
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let ctx = ExportContext::new(data_dir.join("notes"), index_db);
    let zip = ZipService::new(data_dir.join("notes"));
    match wizreader_lib::export::export_folder(&ctx, &zip, &location, &dest, &|done, total| {
        if done % 20 == 0 || done == total {
            eprint!("\r导出进度: {}/{} 篇", done, total);
        }
    }) {
        Ok(r) => {
            eprintln!();
            show_report(&r);
            println!("   → {}", dest.display());
        }
        Err(e) => {
            eprintln!();
            eprintln!("❌ 导出失败: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_export_zips(args: &[String]) {
    let dest = match args.get(1) {
        Some(d) if !d.starts_with("--") => PathBuf::from(d),
        _ => {
            eprintln!("用法: wiz-cli export-zips <dest_dir> [--slim] [--folder <location>]");
            std::process::exit(1);
        }
    };
    let location = flag_value(args, "--folder").unwrap_or_default();
    let slim = args.iter().any(|a| a == "--slim");
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let ctx = ExportContext::new(data_dir.join("notes"), index_db);
    eprintln!(
        "每篇一个 zip → {}（模式：{}）\n范围: {}",
        dest.display(),
        if slim { "FR-02 存储瘦身" } else { "为知原生格式原样复制" },
        if location.is_empty() { "全库".to_string() } else { location.clone() }
    );
    match wizreader_lib::export::export_folder_zips(&ctx, &location, &dest, slim, &|done, total| {
        if done % 20 == 0 || done == total {
            eprint!("\r导出进度: {}/{} 篇", done, total);
        }
    }) {
        Ok(r) => {
            eprintln!();
            show_report(&r.report);
            println!(
                "清单 export.db：新增 {} / 重导 {} / 复用 {} / 墓碑 {}（revision {}）",
                r.notes_added,
                r.notes_reexported,
                r.notes_reused,
                r.notes_removed,
                manifest_revision(&dest)
            );
            for w in r.manifest_warnings.iter().take(5) {
                eprintln!("  ⚠ 清单自检：{w}");
            }
            if r.slim {
                println!(
                    "瘦身：删除冗余资源 {} 个 / {:.1} MB，报告：{}",
                    r.slim_files_removed,
                    r.slim_bytes_removed as f64 / 1048576.0,
                    r.slim_report_path.as_deref().unwrap_or("")
                );
            }
            println!("   → {}", dest.display());
        }
        Err(e) => {
            eprintln!();
            eprintln!("❌ 导出失败: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_manifest_rebuild(args: &[String]) {
    let dest = match args.get(1) {
        Some(d) if !d.starts_with("--") => PathBuf::from(d),
        _ => {
            eprintln!("用法: wiz-cli manifest-rebuild <dest_dir> [--mode native|slim] [--data-dir <dir>]");
            std::process::exit(1);
        }
    };
    let mode = flag_value(args, "--mode").unwrap_or_else(|| "native".into());
    if mode != "native" && mode != "slim" {
        eprintln!("❌ --mode 只支持 native|slim");
        std::process::exit(1);
    }
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let ctx = ExportContext::new(data_dir.join("notes"), index_db);
    eprintln!("重建清单 → {}（模式：{}）", dest.display(), mode);
    match wizreader_lib::manifest::rebuild(&dest, &ctx, &mode) {
        Ok(r) => {
            println!("✅ 清单重建完成：{} 行 → {}", r.rows, dest.join("export.db").display());
            for w in &r.warnings {
                eprintln!("  ⚠ {w}");
            }
        }
        Err(e) => {
            eprintln!("❌ 重建失败: {e}");
            std::process::exit(1);
        }
    }
}

/// 读取清单 meta.revision（仅展示用）
fn manifest_revision(dest: &std::path::Path) -> String {
    rusqlite::Connection::open(dest.join("export.db"))
        .ok()
        .and_then(|c| {
            c.query_row("SELECT value FROM meta WHERE key='revision'", [], |r| r.get::<_, String>(0))
                .ok()
        })
        .unwrap_or_else(|| "?".into())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn default_data_dir() -> Option<PathBuf> {
    // 从应用配置 ~/.wizreader/settings.json 读取（若已设置）
    wizreader_lib::config::load_settings().data_dir.map(PathBuf::from)
}
