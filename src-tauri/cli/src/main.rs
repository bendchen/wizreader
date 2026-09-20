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
        Some("build-md-library") => cmd_build_md_library(&args),
        Some("manifest-rebuild") => cmd_manifest_rebuild(&args),
        Some("cloud-test") => cmd_cloud_test(),
        Some("cloud-init") => cmd_cloud_init(&args),
        Some("cloud-sync") => cmd_cloud_sync(&args),
        Some("trash-gc") => cmd_trash_gc(&args),
        Some("build-library-index") => cmd_build_library_index(&args),
        // 笔记库写入（FR-11 P2 / S1：先在命令行验证，再接 UI）
        Some("save-note") => cmd_save_note(&args),
        Some("rename-note") => cmd_rename_note(&args),
        Some("move-note") => cmd_move_note(&args),
        Some("delete-note") => cmd_delete_note(&args),
        Some("restore-note") => cmd_restore_note(&args),
        Some("list-trash") => cmd_list_trash(&args),
        Some("note-info") => cmd_note_info(&args),
        Some("verify-library") => cmd_verify_library(&args),
        Some("md-preview") => cmd_md_preview(&args),
        Some("md-export") => cmd_md_export(&args),
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
            eprintln!("  wiz-cli export-zips <dest_dir> [--folder <location>]         每篇一个 zip（D0：恒 native，无损）");
            eprintln!("  wiz-cli build-md-library <dest_lib_dir> [--folder <location>]  建 md 形态笔记库（§20.3：包内 note.md + index_files/，清单 export_mode=md）");
            eprintln!("  wiz-cli manifest-rebuild <dest_dir>                          重建导出目录清单 export.db（丢失后用）");
            eprintln!("  wiz-cli cloud-test                                   云端连通性自检（读取应用内已存配置与凭据）");
            eprintln!("  wiz-cli cloud-init <export|reader>                   首次初始化（导出端=全量上行；阅读端=全量下行）");
            eprintln!("  wiz-cli cloud-sync <up|down>                         手动同步（上行复用导出缓存，下行 HEAD 比对差量）");
            eprintln!("  wiz-cli trash-gc [--days 30]                         清理本地同步根 _trash/ 超期项");
            eprintln!("  wiz-cli build-library-index <library_dir> [--index-db <file>]   构建笔记库派生索引（只读 export.db）");
            eprintln!("  --- 笔记库写入（P2/S1：CLI 先验证，写前建议先冷备）---");
            eprintln!("  wiz-cli note-info <guid> [--library-dir <dir>]                 打印库内该篇的清单行/文件/条目/正文（形态自适应：md 包读 note.md）");
            eprintln!("  wiz-cli save-note <guid> --file <new.md|new.html> [--md|--html]  编辑正文（默认按包内形态自动分派；--md/--html 强制）");
            eprintln!("  wiz-cli rename-note <guid> <new-title>                        重命名（落地文件名随标题变）");
            eprintln!("  wiz-cli move-note <guid> <new-location>                       移动到目录（如 \"/My Notes/\"）");
            eprintln!("  wiz-cli delete-note <guid>                                    删除 → 移入 _trash/ + 写墓碑（含恢复载荷）");
            eprintln!("  wiz-cli restore-note <guid>                                   从回收站恢复（文件回原路径 + 清单行逐字段还原）");
            eprintln!("  wiz-cli list-trash [--library-dir <dir>]                      列出库回收站（墓碑 + 磁盘实况）");
            eprintln!("  wiz-cli verify-library [--library-dir <dir>] [--deep] [--render]   库自检（--deep 逐篇重算 MD5；--render 逐篇走阅读渲染）");
            eprintln!("  --- Markdown（§20：库内格式改 md 包）---");
            eprintln!("  wiz-cli md-preview <guid> [--zip <path>] [--library-dir <dir>] [--data-dir <dir>] [--html|--doc] [--out <file>] [--stats]");
            eprintln!("                                                       单篇「为知 HTML → Markdown」预览（默认输出 md；--html/--doc 输出渲染结果）；md 包则直取 note.md");
            eprintln!("  wiz-cli md-export <src_dir> <dest_dir> [--limit N]    批量把目录下每篇原生 zip 转成 .md（供人工抽检与回归评估）");
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
            eprintln!("用法: wiz-cli export-zips <dest_dir> [--folder <location>]");
            std::process::exit(1);
        }
    };
    let location = flag_value(args, "--folder").unwrap_or_default();
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let ctx = ExportContext::new(data_dir.join("notes"), index_db);
    eprintln!(
        "每篇一个 zip → {}（native：源 zip 字节级拷贝，无格式选项）\n范围: {}",
        dest.display(),
        if location.is_empty() { "全库".to_string() } else { location.clone() }
    );
    match wizreader_lib::export::export_folder_zips(&ctx, &location, &dest, &|done, total| {
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
            println!("   → {}", dest.display());
        }
        Err(e) => {
            eprintln!();
            eprintln!("❌ 导出失败: {e}");
            std::process::exit(1);
        }
    }
}

/// 建一个 **md 形态的笔记库**（§20.3 / M2）：`<dest_lib_dir>` 下每篇一个 md 包
/// （包内 `note.md` = 源 `index.html` 的 Markdown，其余条目如 `index_files/` **原样搬运**），
/// 并写同步清单 `export.db`（行与 meta 的 `export_mode` 都是 `md`）。
///
/// 这是「导入到我的笔记库」的 CLI 等价物（GUI 那条走同一个 `export_folder_zips_md`），
/// 用途是**在接 UI 之前先把整库跑出来验收**。与 `export-zips` 的差别只有包内正文档名：
/// `index.html`（native，D0 逃生舱）↔ `note.md`（md 库）。落地路径与文件名完全一致。
fn cmd_build_md_library(args: &[String]) {
    let dest = match args.get(1) {
        Some(d) if !d.starts_with("--") => PathBuf::from(d),
        _ => {
            eprintln!("用法: wiz-cli build-md-library <dest_lib_dir> [--folder <location>]");
            std::process::exit(1);
        }
    };
    let location = flag_value(args, "--folder").unwrap_or_default();
    let (data_dir, index_db) = match resolve_dirs(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    let ctx = ExportContext::new(data_dir.join("notes"), index_db);
    eprintln!(
        "建 md 库 → {}\n范围: {}（包内 note.md + index_files/ 原样；清单 export_mode=md）",
        dest.display(),
        if location.is_empty() { "全库".to_string() } else { location.clone() }
    );
    match wizreader_lib::export::export_folder_zips_md(&ctx, &location, &dest, &|done, total| {
        if done % 20 == 0 || done == total {
            eprint!("\r转换进度: {}/{} 篇", done, total);
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
            if let Some(md) = &r.md {
                if md.notes == 0 {
                    println!(
                        "Markdown：本轮 0 篇重写（{} 篇全部复用现有 md 包；转换统计只计本轮新转的篇目）",
                        r.notes_reused
                    );
                } else {
                    println!(
                        "Markdown：本轮新转 {} 篇，正文合计 {:.1} MB；代码围栏 {}（其中代码排版表 {}）/ 表格 {} / 摊平布局表 {} / 吞镜像 {}",
                        md.notes,
                        md.md_bytes as f64 / 1_048_576.0,
                        md.code_fences,
                        md.code_tables,
                        md.tables,
                        md.layout_tables,
                        md.mirrors
                    );
                    if r.notes_reused > 0 {
                        println!("     （另有 {} 篇复用现有 md 包，未重写）", r.notes_reused);
                    }
                }
                if md.empty_md > 0 {
                    println!("⚠ 转换后正文为空 {} 篇（源正文本身为空/无可见内容）", md.empty_md);
                    for s in &md.empty_samples {
                        println!("    · {s}");
                    }
                }
            }
            for w in r.manifest_warnings.iter().take(5) {
                eprintln!("  ⚠ 清单自检：{w}");
            }
            println!("   → {}", dest.display());
            println!("   下一步：wiz-cli build-library-index {} ", dest.display());
        }
        Err(e) => {
            eprintln!();
            eprintln!("❌ 建库失败: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_manifest_rebuild(args: &[String]) {
    let dest = match args.get(1) {
        Some(d) if !d.starts_with("--") => PathBuf::from(d),
        _ => {
            eprintln!("用法: wiz-cli manifest-rebuild <dest_dir> [--data-dir <dir>]");
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
    eprintln!("重建清单 → {}（native：以磁盘为准重建）", dest.display());
    match wizreader_lib::manifest::rebuild(&dest, &ctx) {
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

// ---------------- 云同步子命令（FR-07 阶段二） ----------------

use wizreader_lib::config::SyncSettings;
// 注：`ObjectStore` trait 在用到其方法的函数内局部引入（见 cmd_cloud_test），模块级重复引入会告警
use wizreader_lib::sync::{SyncGuard, SyncReport};

/// 从 settings.json 读云同步配置（凭据从钥匙串取，secret 绝不经命令行）
fn cloud_cfg() -> Result<SyncSettings, String> {
    let s = wizreader_lib::config::load_settings().sync;
    if !s.enabled {
        return Err("SYNC_DISABLED: 云同步未启用（先在应用内配置并保存）".into());
    }
    if s.credential_user.is_empty() {
        return Err("SYNC_NO_CREDENTIAL: 未存储凭据（先在应用内保存配置）".into());
    }
    Ok(s)
}

fn cloud_store(cfg: &SyncSettings) -> Result<wizreader_lib::store::S3Store, String> {
    // 测试/CI 逃生通道：WIZREADER_SYNC_SECRET 优先于钥匙串。
    // 仅存在于进程环境（不落盘），正式使用仍走钥匙串；GUI 场景不受影响。
    let secret = match std::env::var("WIZREADER_SYNC_SECRET") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => wizreader_lib::credential::read_secret(&cfg.credential_user)?,
    };
    wizreader_lib::sync::s3_store_of(cfg, &secret)
}

fn print_sync_report(rep: &SyncReport) {
    match rep.direction.as_str() {
        "up" => println!(
            "✅ 上行完成：上传 {} / 跳过 {} / 超限 {} / 冲突留存 {} / 清单{}，耗时 {} ms",
            rep.uploaded, rep.skipped, rep.oversized, rep.conflicts,
            if rep.manifest_uploaded { "已传" } else { "未传" },
            rep.elapsed_ms
        ),
        "down" => println!(
            "✅ 下行完成：下载 {} / 移入回收站 {} / 失败 {}，耗时 {} ms",
            rep.downloaded, rep.trashed, rep.failures.len(), rep.elapsed_ms
        ),
        "none" => println!("✅ 无变更（远端 revision {} ≤ 本地 {}）", rep.remote_revision.unwrap_or(0), rep.local_revision),
        _ => println!("{rep:?}"),
    }
    for f in rep.failures.iter().take(10) {
        eprintln!("  ❌ {}: {}（可重试：{}）", f.key, f.reason, f.retryable);
    }
}

fn progress_cb() -> wizreader_lib::sync::ProgressFn {
    std::sync::Arc::new(|phase, done, total| {
        if done % 20 == 0 || done == total {
            eprint!("\r{phase}: {done}/{total}");
        }
    })
}

/// cloud-test：读配置 + 钥匙串 → HEAD bucket/清单 + 前缀对象计数 + PUT/GET/DELETE 探测回环
fn cmd_cloud_test() {
    let run = async {
        use wizreader_lib::store::ObjectStore as _;
        let cfg = cloud_cfg()?;
        let store = cloud_store(&cfg)?;
        let t0 = std::time::Instant::now();
        let manifest_head = store.head(&cfg.cloud_key("manifest.db")).await?;
        let keys = store.list_keys(&cfg.normalized_prefix().to_string()).await?;
        let latency = t0.elapsed().as_millis();
        println!(
            "✅ 连接正常（{} ms）：bucket={} prefix={} 对象数={} 远端清单={}",
            latency,
            cfg.bucket,
            cfg.normalized_prefix(),
            keys.len(),
            match manifest_head {
                Some(h) => format!("存在（revision {}）", h.meta("revision").cloned().unwrap_or_default()),
                None => "不存在（未初始化）".into(),
            }
        );
        // 写链路探测：PUT → 校验 ETag/元数据 → GET 比对 → DELETE，全部走真实 S3 协议
        let probe_key = format!("{}/.probe-{}", cfg.normalized_prefix().trim_end_matches('/'), std::process::id());
        let payload = b"wizreader-cloud-probe".to_vec();
        let md5_hex = {
            use md5::Digest;
            let mut h = md5::Md5::new();
            h.update(&payload);
            h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let t1 = std::time::Instant::now();
        let rep = store
            .put_bytes(&probe_key, &payload, &[("probe".into(), "wizreader-cli".into())])
            .await?;
        let put_ms = t1.elapsed().as_millis();
        if rep.hex_md5 != md5_hex {
            return Err(format!("PUT 探测 ETag 不符：{} ≠ {md5_hex}", rep.hex_md5));
        }
        let got = store.get(&probe_key).await?;
        if got != payload {
            return Err("GET 探测内容不一致".into());
        }
        let head = store
            .head(&probe_key)
            .await?
            .ok_or("HEAD 探测对象不存在")?;
        if head.meta("probe") != Some(&"wizreader-cli".to_string()) {
            return Err(format!("HEAD 元数据缺失 probe（got {:?}）", head.metadata));
        }
        store.delete(&probe_key).await?;
        let gone = store.head(&probe_key).await?;
        if gone.is_some() {
            return Err("DELETE 探测后对象仍存在".into());
        }
        println!(
            "✅ 写链路探测通过：PUT({put_ms} ms, {} B) → GET 比对一致 → 元数据回读正常 → DELETE 干净",
            payload.len()
        );

        // —— 条件写探测（§7.1 纵深防御）：服务端**是否真的执行** If-Match / If-None-Match: * ——
        //
        // 为什么要单独探：`sync_up` 的提交点靠它挡住"读—改—写窗口里被第二个写入端插队"。
        // 若服务端不执行条件（有的 S3 兼容实现直接忽略，或对 If-Match 回 501），我方会自动
        // 降级为无条件 PUT —— 同步照常，但这层纵深防御就是**失效的**，必须让用户看见。
        {
            use wizreader_lib::store::{ConditionalPut, Precondition, ERR_PRECONDITION_UNSUPPORTED};
            let cond_key = format!(
                "{}/.probe-cond-{}",
                cfg.normalized_prefix().trim_end_matches('/'),
                std::process::id()
            );
            if store.head(&cond_key).await?.is_some() {
                store.delete(&cond_key).await?;
            }
            let absent = store
                .put_bytes_if(&cond_key, b"cond-v1", Precondition::Absent, &[])
                .await;
            match absent {
                Ok(ConditionalPut::Written(_)) => {
                    let again = store
                        .put_bytes_if(&cond_key, b"cond-v2", Precondition::Absent, &[])
                        .await;
                    if matches!(again, Ok(ConditionalPut::PreconditionFailed)) {
                        println!("✅ 条件写探测：`If-None-Match: *` 空位可写 / 对象已存在时被拒");
                    } else {
                        println!(
                            "⚠️ 条件写探测：`If-None-Match: *` 覆盖已存在对象**未被拒** —— \
                             该服务端不执行条件写，§7.1 纵深防御失效（同步会自动降级为无条件上传）"
                        );
                    }
                    match store.head(&cond_key).await?.and_then(|h| h.e_tag) {
                        Some(t) => {
                            let wrong = format!("\"{}\"", "0".repeat(32));
                            let bad = store
                                .put_bytes_if(&cond_key, b"cond-v3", Precondition::Match(&wrong), &[])
                                .await;
                            let good = store
                                .put_bytes_if(&cond_key, b"cond-v4", Precondition::Match(&t), &[])
                                .await;
                            let rejected = matches!(&bad, Ok(ConditionalPut::PreconditionFailed));
                            let accepted = matches!(&good, Ok(ConditionalPut::Written(_)));
                            if rejected && accepted {
                                println!(
                                    "✅ 条件写探测：错误 ETag 被 412 拒绝 / 正确 ETag 写入成功\
                                     （If-Match 语义成立 ⇒ §7.1 纵深防御可用）"
                                );
                            } else {
                                println!(
                                    "⚠️ 条件写探测：If-Match 语义不完整（错误 ETag 被拒={rejected} / \
                                     正确 ETag 成功={accepted}）—— 条件写不可依赖，\
                                     同步会自动降级为无条件上传（§7.1 纵深防御失效）"
                                );
                                // 诊断：把两次调用的原始结果打出来（真云排障用；不影响判定）
                                println!("   诊断：错误的 If-Match ⇒ {bad:?}；正确的 If-Match ⇒ {good:?}");
                            }
                        }
                        None => println!(
                            "⚠️ 条件写探测：HEAD 未返回 ETag ⇒ 无法用 If-Match（同步自动降级为无条件上传）"
                        ),
                    }
                }
                Ok(ConditionalPut::PreconditionFailed) => println!(
                    "⚠️ 条件写探测：空位 `If-None-Match: *` 竟被 412（有残留对象？）—— 已跳过 If-Match 探测"
                ),
                Err(e) if e.starts_with(ERR_PRECONDITION_UNSUPPORTED) => println!(
                    "⚠️ 条件写探测：服务端**不支持**条件 PUT（{e}）—— \
                     同步会自动降级为无条件上传（§7.1 纵深防御失效）"
                ),
                Err(e) => return Err(e),
            }
            store.delete(&cond_key).await.ok();
        }
        Ok::<(), String>(())
    };
    if let Err(e) = run_in_blocking(run) {
        eprintln!("❌ {e}");
        std::process::exit(1);
    }
}

/// cloud-init <writer|reader>（旧名 `export` 兼容映射为 `writer`，与设置迁移同口径）
fn cmd_cloud_init(args: &[String]) {
    let raw_role = match args.get(1).map(|s| s.as_str()) {
        Some("export") | Some("writer") | Some("reader") => args[1].clone(),
        _ => {
            eprintln!("用法: wiz-cli cloud-init <writer|reader>（旧名 export = writer）");
            std::process::exit(1);
        }
    };
    let run = async move {
        let cfg = cloud_cfg()?;
        let store = cloud_store(&cfg)?;
        // U1：同步根恒为库根（`cloud_cfg` 只给云参数，根从 settings 取）
        let root = wizreader_lib::config::load_settings()
            .sync_root()
            .ok_or("SYNC_CONFIG_INVALID: 未设置笔记库目录（同步根即库根）")?;
        let _guard: SyncGuard = match wizreader_lib::sync::try_acquire(&root)? {
            Some(g) => g,
            None => return Err("SYNC_BUSY: 已有同步任务在执行".into()),
        };
        let p = progress_cb();
        let role = wizreader_lib::config::canonical_role(&raw_role).to_string();
        let rep = match role.as_str() {
            wizreader_lib::config::ROLE_WRITER => {
                wizreader_lib::sync::bootstrap_export(&store, &cfg, &root, p).await?
            }
            _ => wizreader_lib::sync::bootstrap_reader(&store, &cfg, &root, p).await?,
        };
        print_sync_report(&rep);
        Ok::<(), String>(())
    };
    if let Err(e) = run_in_blocking(run) {
        eprintln!();
        eprintln!("❌ {e}");
        std::process::exit(1);
    }
}

/// cloud-sync <up|down>
fn cmd_cloud_sync(args: &[String]) {
    let direction = match args.get(1).map(|s| s.as_str()) {
        Some("up") | Some("down") => args[1].clone(),
        _ => {
            eprintln!("用法: wiz-cli cloud-sync <up|down>");
            std::process::exit(1);
        }
    };
    let run = async move {
        let cfg = cloud_cfg()?;
        let store = cloud_store(&cfg)?;
        // U4 / R7：只读端不生成本地修改 ⇒ 也没有"上行"这回事（与 `run_sync` 同判据）
        if direction == "up" && wizreader_lib::config::canonical_role(&cfg.role) == wizreader_lib::config::ROLE_READER {
            return Err(
                "READER_READONLY: 本机角色为只读端（reader），不支持上行 —— 请在写入端上行".into(),
            );
        }
        // U1：同步根恒为库根
        let root = wizreader_lib::config::load_settings()
            .sync_root()
            .ok_or("SYNC_CONFIG_INVALID: 未设置笔记库目录（同步根即库根）")?;
        let _guard: SyncGuard = match wizreader_lib::sync::try_acquire(&root)? {
            Some(g) => g,
            None => return Err("SYNC_BUSY: 已有同步任务在执行".into()),
        };
        let p = progress_cb();
        let rep = if direction == "up" {
            wizreader_lib::sync::sync_up(&store, &cfg, &root, p).await?
        } else {
            wizreader_lib::sync::sync_down(&store, &cfg, &root, p).await?
        };
        print_sync_report(&rep);
        Ok::<(), String>(())
    };
    if let Err(e) = run_in_blocking(run) {
        eprintln!();
        eprintln!("❌ {e}");
        std::process::exit(1);
    }
}

/// trash-gc [--days 30]
fn cmd_trash_gc(args: &[String]) {
    // T7 收口：GC 的根与应用内一致（`Settings::trash_root()`：有库 → 库根，否则旧同步根）
    let s = wizreader_lib::config::load_settings();
    let Some(root) = s.trash_root() else {
        eprintln!("❌ 未设置笔记库目录（也没有旧同步根），回收站不可用");
        std::process::exit(1);
    };
    let days: u64 = flag_value(args, "--days").and_then(|d| d.parse().ok()).unwrap_or(30);
    match wizreader_lib::sync::gc_trash(&root, days) {
        Ok(rep) => println!(
            "✅ 回收站清理完成（{}）：删除 {} 项（{:.1} MB），保留 {} 项（保留期 {} 天）",
            root.join("_trash").display(),
            rep.removed,
            rep.bytes as f64 / 1048576.0,
            rep.kept,
            days
        ),
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    }
}

/// build-library-index <library_dir> [--index-db <file>]
/// 构建笔记库派生索引（只读 export.db + 清单 exported_path 定位 zip，绝不拼路径）
fn cmd_build_library_index(args: &[String]) {
    use std::sync::Arc;
    let lib = args
        .get(1)
        .filter(|s| !s.starts_with("--"))
        .map(PathBuf::from)
        .or_else(|| flag_value(args, "--library-dir").map(PathBuf::from))
        .or_else(|| wizreader_lib::config::load_settings().library_dir.map(PathBuf::from))
        .unwrap_or_else(|| {
            eprintln!("未指定笔记库目录：用法 wiz-cli build-library-index <library_dir>");
            std::process::exit(1);
        });
    // 准入校验（§3.4）：非 ready 拒绝
    let status = wizreader_lib::library::validate_library(&lib);
    if status.kind != "ready" {
        let why = if status.reason.is_empty() {
            "无清单/目录为空，请先导入或重建清单".to_string()
        } else {
            status.reason.clone()
        };
        eprintln!("❌ 库不可用（{}）：{}", status.kind, why);
        std::process::exit(2);
    }
    let index_db = flag_value(args, "--index-db")
        .map(PathBuf::from)
        .unwrap_or_else(|| wizreader_lib::config::index_file_for_library(&lib));
    let resolver = match wizreader_lib::library::LibraryResolver::new(lib.clone()) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("❌ 打开库清单失败: {e}");
            std::process::exit(1);
        }
    };
    println!("笔记库: {}", lib.display());
    println!("索引文件: {}", index_db.display());
    let t0 = std::time::Instant::now();
    match wizreader_lib::indexer::build_library_index(&lib, resolver, &index_db, &|done, total| {
        eprint!("\r索引进度: {}/{} 篇", done, total);
    }) {
        Ok(r) => {
            eprintln!();
            println!("{:#?}", r);
            if !r.ok {
                eprintln!("⚠️ 库索引一致性存在偏离（见 warnings）");
                std::process::exit(2);
            }
            println!("✅ 库索引构建完成，耗时 {} ms", t0.elapsed().as_millis());
        }
        Err(e) => {
            eprintln!("❌ 构建失败: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------- 笔记库写入子命令（FR-11 P2 / S1） ----------------
//
// 排序理由（设计文档 §15.3）：写入是第一次真正改动用户数据 —— 先在命令行用**库副本**跑通
// "改完还能读、崩溃不损坏、清单自洽"，再接 UI。

fn library_dir_arg(args: &[String]) -> PathBuf {
    flag_value(args, "--library-dir")
        .map(PathBuf::from)
        .or_else(|| wizreader_lib::config::load_settings().library_dir.map(PathBuf::from))
        .unwrap_or_else(|| {
            eprintln!("未指定笔记库目录：用 --library-dir <dir>，或先在应用内「设置数据目录」");
            std::process::exit(1);
        })
}

fn library_index_db_arg(args: &[String], lib: &std::path::Path) -> PathBuf {
    flag_value(args, "--index-db")
        .map(PathBuf::from)
        .unwrap_or_else(|| wizreader_lib::config::index_file_for_library(lib))
}

/// 写操作统一出口：准入自检 → 执行 → 报告（错误码原样透传，退出码 1/2）
fn run_write<F>(lib: &std::path::Path, op: &str, f: F)
where
    F: FnOnce() -> Result<wizreader_lib::library::NoteWriteReport, String>,
{
    let status = wizreader_lib::library::validate_library(lib);
    if status.kind != "ready" {
        eprintln!("❌ 库不可用（{}）：{}", status.kind, status.reason);
        std::process::exit(2);
    }
    match f() {
        Ok(rep) => {
            // delete 后行已移入墓碑，没有行级 revision —— 显示「—」而不是 0（0 会被误读成"没写成"）
            let rev = if rep.op == "delete" {
                "—（行已入墓碑）".to_string()
            } else {
                rep.revision.to_string()
            };
            println!(
                "✅ {op} 完成：{} → {}\n   {} B / md5 {} / data_modified {} / 行 rev {} / 清单 rev {}",
                rep.guid,
                rep.exported_path,
                rep.exported_size,
                rep.exported_md5,
                rep.data_modified,
                rev,
                rep.manifest_revision
            );
            if !rep.index_updated {
                eprintln!("  ⚠ 派生索引未同步（需跑 build-library-index 重建）");
            }
            for w in &rep.warnings {
                eprintln!("  ⚠ {w}");
            }
        }
        Err(e) => {
            eprintln!("❌ {op} 失败: {e}");
            std::process::exit(1);
        }
    }
}

/// note-info <guid>：写出前后核对用的"现场快照"（清单行 + 磁盘 + zip 条目 + 正文长度）
fn cmd_note_info(args: &[String]) {
    let guid = match args.get(1) {
        Some(g) if !g.starts_with("--") => g.clone(),
        _ => {
            eprintln!("用法: wiz-cli note-info <guid> [--library-dir <dir>] [--entries]");
            std::process::exit(1);
        }
    };
    let lib = library_dir_arg(args);
    let inner = guid.trim_matches(['{', '}']).to_string();
    let uri = {
        let db = lib.join("export.db");
        format!("file:{}?mode=ro", wizreader_lib::indexer::url_encode_path(&db))
    };
    let conn = match rusqlite::Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ 打开库清单失败: {e}");
            std::process::exit(1);
        }
    };
    let row: Result<(String, String, String, String, i64, String, String), rusqlite::Error> = conn.query_row(
        "SELECT title, location, exported_path, data_modified, exported_size, exported_md5, exported_at
         FROM note WHERE guid = ?1",
        [&inner],
        |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
            ))
        },
    );
    // v2 清单无 revision 列（写路径首次写入时才迁移到 v3）→ 读不到就显示 "?"
    let row_rev: Option<i64> = conn
        .query_row("SELECT revision FROM note WHERE guid = ?1", [&inner], |r| r.get(0))
        .ok();
    let (title, location, rel, dmod, size, md5, exp_at) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // 可能在墓碑里（已删除）
            let tomb: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT last_path, removed_at, ifnull(title,'') FROM deleted WHERE guid = ?1",
                    [&inner],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok();
            match tomb {
                Some((lp, ra, t)) => {
                    println!("🗑 该篇已在墓碑（deleted）：{t}\n   last_path={lp}\n   removed_at={ra}");
                    println!("   回收站: {}", lib.join("_trash").display());
                }
                None => {
                    eprintln!("❌ 清单中无此 guid: {inner}");
                    std::process::exit(2);
                }
            }
            return;
        }
        Err(e) => {
            eprintln!("❌ 查清单失败: {e}");
            std::process::exit(1);
        }
    };
    let path = lib.join(&rel);
    println!("guid        : {inner}");
    println!("title       : {title}");
    println!("location    : {location}");
    println!("exported    : {rel}");
    println!("data_modif  : {dmod}   exported_at: {exp_at}   行 rev: {}", match row_rev {
        Some(v) => v.to_string(),
        None => "?（v2 清单未迁移，写入后自动补列）".into(),
    });
    println!("清单 size   : {size}");
    println!("清单 md5    : {md5}");
    match std::fs::metadata(&path) {
        Ok(m) => println!("磁盘 size   : {}", m.len()),
        Err(e) => println!("磁盘        : 缺失（{e}）"),
    }
    // 实际 zip 内容（条目不减的硬证据）
    let zs = wizreader_lib::zipserve::ZipService::with_resolver(std::sync::Arc::new(
        match wizreader_lib::library::LibraryResolver::new(lib.clone()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("❌ 构建解析器失败: {e}");
                std::process::exit(1);
            }
        },
    ));
    let entries = zs.list_entries(&inner).unwrap_or_default();
    println!("zip 条目数  : {}", entries.len());
    if args.iter().any(|a| a == "--entries") {
        for e in &entries {
            println!("   · {e}");
        }
    }
    // 正文：**形态自适应**（M3/§20.3）—— md 包读 `note.md`（Markdown 原文），
    // 为知原生包读 `index.html`（再抽纯文本）。读不到就是真读不到，不做静默回退。
    match zs.read_note_body(&inner) {
        Ok((fmt, text)) => {
            let (entry, chars, bom) = match fmt {
                wizreader_lib::zipserve::BodyFormat::Md => (
                    wizreader_lib::md::NOTE_MD,
                    text.chars().count(),
                    zs.read_entry(&inner, wizreader_lib::md::NOTE_MD)
                        .map(|b| b.starts_with(&[0xEF, 0xBB, 0xBF]))
                        .unwrap_or(false),
                ),
                wizreader_lib::zipserve::BodyFormat::Html => (
                    "index.html",
                    wizreader_lib::extract::extract_text(&text).chars().count(),
                    zs.read_entry(&inner, "index.html")
                        .map(|b| b.starts_with(&[0xEF, 0xBB, 0xBF]))
                        .unwrap_or(false),
                ),
            };
            println!(
                "正文形态    : {}（包内条目 {entry}）",
                match fmt {
                    wizreader_lib::zipserve::BodyFormat::Md => "md 包（Markdown 源）",
                    wizreader_lib::zipserve::BodyFormat::Html => "为知原生包（HTML 源）",
                }
            );
            println!(
                "正文        : {} 字符（源 {} 字节，BOM {}）",
                chars,
                text.len(),
                if bom { "有" } else { "无" }
            );
        }
        Err(e) => println!("正文        : 读取失败 {e:?}"),
    }
}

/// 库一致性自检。默认轻量档（只 stat 体积，秒级）；`--deep` 逐篇重算 MD5（慢，
/// 但这是唯一能发现「体积相符、内容已变」的手段 —— T9 验收口径）；
/// `--render` 逐篇走一遍**阅读渲染**（`read_note_document`，md 包要跑 pulldown-cmark），
/// 确认"库内每篇都读得出来"—— 这是 M2 遗留缺口（"可列可搜不可读"）的回归闸门。
fn cmd_verify_library(args: &[String]) {
    let lib = library_dir_arg(args);
    let deep = args.iter().any(|a| a == "--deep");
    let render = args.iter().any(|a| a == "--render");

    let status = wizreader_lib::library::validate_library(&lib);
    if status.kind != "ready" {
        eprintln!("❌ 库不可用（{}）：{}", status.kind, status.reason);
        std::process::exit(2);
    }
    // 只读打开：自检是诊断动作，不得对库产生任何副作用（连 v2→v3 迁移也不做）
    let conn = match wizreader_lib::manifest::open_readonly(&lib) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ 打开库清单失败: {e}");
            std::process::exit(1);
        }
    };
    let schema = wizreader_lib::manifest::get_meta(&conn, "schema_version")
        .ok()
        .flatten()
        .unwrap_or_else(|| "未知".into());
    println!(
        "🔍 库自检：{}（{} 篇，schema v{}，{} 档）",
        lib.display(),
        status.note_count,
        schema,
        if deep { "深度：逐篇比 MD5" } else { "轻量：只比体积" }
    );
    // 格式标识分布（1 次查询，免费）：md 包 / native 包各多少篇。
    // 注意这是**清单里的格式标识**，与包内实况的一致性由逐篇验收脚本核（§23）。
    // 注：`note` 表只存活跃行（墓碑在独立的 `deleted` 表里），故**没有** `deleted` 列可筛。
    let modes: Vec<(String, i64)> = conn
        .prepare("SELECT ifnull(export_mode,'(空)') m, count(*) FROM note GROUP BY m ORDER BY 2 DESC")
        .and_then(|mut st| {
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|it| it.flatten().collect())
        })
        .unwrap_or_else(|e| {
            eprintln!("   清单格式标识：查询失败（{e}）");
            Vec::new()
        });
    if !modes.is_empty() {
        let desc = modes
            .iter()
            .map(|(m, n)| format!("{m} {n} 篇"))
            .collect::<Vec<_>>()
            .join(" / ");
        println!("   清单格式标识：{desc}");
    }
    let t0 = std::time::Instant::now();
    let warns = if deep {
        wizreader_lib::manifest::check_invariants_deep(&conn, &lib)
    } else {
        wizreader_lib::manifest::check_invariants(&conn, &lib)
    };
    let warns = match warns {
        Ok(w) => w,
        Err(e) => {
            eprintln!("❌ 自检失败: {e}");
            std::process::exit(1);
        }
    };
    let secs = t0.elapsed().as_secs_f64();
    if warns.is_empty() {
        println!("✅ 未发现问题（耗时 {secs:.2}s）");
    } else {
        println!("⚠ 发现 {} 条问题（耗时 {secs:.2}s）：", warns.len());
        for w in &warns {
            println!("   • {w}");
        }
        if !deep {
            println!("   提示：以上不含内容比对。查「体积相符但内容已变」请加 --deep。");
        }
        std::process::exit(1);
    }
    // 阅读渲染体检（M3）：逐篇走**读路径**（源文本 + 阅读文档），确认"库内每篇都读得出来"。
    // 两个口径分开测，因为它们的失败含义不同：
    //  ① `read_note_body` —— 源文本可读（形态判定 + 是否空正文）；
    //  ② `read_note_document` —— 阅读渲染可读（md 包跑 pulldown-cmark，原生包取 index.html）。
    // **空正文只计数不判失败**：真实库里有 36/37 字节的空页（源正文本身为空），
    // 它们渲染出空内容是**正确的**。判空必须看**源文本**而不是渲染结果 ——
    // md 渲染出的文档带内联 CSS，对"整份文档"抽文本的话 CSS 也算文本，会把空页漏报。
    if render {
        let resolver = match wizreader_lib::library::LibraryResolver::new(lib.clone()) {
            Ok(r) => std::sync::Arc::new(r),
            Err(e) => {
                eprintln!("❌ 构建库解析器失败: {e}");
                std::process::exit(1);
            }
        };
        let zs = wizreader_lib::zipserve::ZipService::with_resolver(resolver);
        // 空结果绝不能静默（踩过：列名写错 → 查询报错 → 0 篇"体检通过"，看着像好消息）
        let guids: Vec<String> = match conn
            .prepare("SELECT guid FROM note ORDER BY guid")
            .and_then(|mut st| {
                st.query_map([], |r| r.get::<_, String>(0))
                    .map(|it| it.flatten().collect())
            }) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("❌ 读取清单 guid 列表失败: {e}");
                std::process::exit(1);
            }
        };
        if guids.is_empty() {
            eprintln!("❌ 清单里没有任何活跃笔记（note 表为空），渲染体检无从谈起");
            std::process::exit(1);
        }
        let t0 = std::time::Instant::now();
        let (mut ok, mut empty, mut md, mut html) = (0usize, 0usize, 0usize, 0usize);
        let mut bad: Vec<String> = Vec::new();
        // 「保真溢出」计量（只对 md 包）：渲染出的代码块数 vs 源里的围栏数。
        // 两者本应相等 —— 渲染出的**多出来**的那些是 Markdown 把「以 Tab/4 空格开头的行」
        // 当成**缩进代码块**吃掉了，正文会被显示成代码（内容没丢，但呈现错了）。
        // 这是 M1 转换器与渲染器之间唯一能被自动化抓住的口径差，故常驻体检。
        let (mut fence_total, mut rendered_total) = (0usize, 0usize);
        let mut bleed: Vec<(usize, String)> = Vec::new();
        for g in &guids {
            let (fmt, src) = match zs.read_note_body(g) {
                Ok(v) => v,
                Err(e) => {
                    bad.push(format!("{g}: 正文源读取失败 {}", e.message()));
                    continue;
                }
            };
            match fmt {
                wizreader_lib::zipserve::BodyFormat::Md => md += 1,
                wizreader_lib::zipserve::BodyFormat::Html => html += 1,
            }
            if src.trim().is_empty() {
                empty += 1; // 空页是源数据事实，不算缺陷
            }
            match zs.read_note_document(g, g) {
                Ok(doc) => {
                    ok += 1;
                    if fmt == wizreader_lib::zipserve::BodyFormat::Md {
                        let fences = src
                            .lines()
                            .filter(|l| l.trim_start().starts_with("```"))
                            .count()
                            / 2;
                        let rendered = doc.matches("<pre><code").count();
                        fence_total += fences;
                        rendered_total += rendered;
                        let extra = rendered.saturating_sub(fences);
                        if extra > 0 {
                            bleed.push((extra, g.clone()));
                        }
                    }
                }
                Err(e) => bad.push(format!("{g}: 阅读渲染失败 {}", e.message())),
            }
        }
        println!(
            "🔎 阅读链路体检：{}/{} 篇可渲染（md 包 {} / 原生包 {}；源正文为空 {} 篇），耗时 {:.2}s",
            ok,
            guids.len(),
            md,
            html,
            empty,
            t0.elapsed().as_secs_f64()
        );
        if md > 0 {
            bleed.sort_by(|a, b| b.0.cmp(&a.0));
            let notes_bleed = bleed.len();
            println!(
                "   代码块口径：源围栏 {} → 渲染 {}（多出 {}；涉及 {} 篇）",
                fence_total,
                rendered_total,
                rendered_total.saturating_sub(fence_total),
                notes_bleed
            );
            for (n, g) in bleed.iter().take(5) {
                println!("      • 多 {n} 块：{g}");
            }
        }
        if !bad.is_empty() {
            println!("⚠ 读取失败 {} 篇：", bad.len());
            for b in bad.iter().take(20) {
                println!("   • {b}");
            }
            std::process::exit(1);
        }
    }
}

fn cmd_save_note(args: &[String]) {
    let guid = match args.get(1) {
        Some(g) if !g.starts_with("--") => g.clone(),
        _ => {
            eprintln!("用法: wiz-cli save-note <guid> --file <new.md|new.html> [--md|--html] [--library-dir <dir>]");
            std::process::exit(1);
        }
    };
    let file = match flag_value(args, "--file").or_else(|| flag_value(args, "--html")) {
        Some(f) => f,
        None => {
            eprintln!("缺少 --file <new.md|new.html>（新正文文件；UTF-8）");
            std::process::exit(1);
        }
    };
    let text = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ 读取 {file} 失败: {e}");
            std::process::exit(1);
        }
    };
    // 形态：默认**按包内实况自动分派**（M3/§20.8，与 UI 同一条路径）；
    // `--md` / `--html` 强制指定，用于验收时明确打某一支（形态不符会明确报错，不静默改造）。
    let forced = if args.iter().any(|a| a == "--md") {
        Some(wizreader_lib::zipserve::BodyFormat::Md)
    } else if args.iter().any(|a| a == "--html") {
        Some(wizreader_lib::zipserve::BodyFormat::Html)
    } else {
        None
    };
    let lib = library_dir_arg(args);
    let index_db = library_index_db_arg(args, &lib);
    run_write(&lib, "save-note", || match forced {
        Some(wizreader_lib::zipserve::BodyFormat::Md) => {
            wizreader_lib::library::save_note_md(&lib, &index_db, &guid, &text)
        }
        Some(wizreader_lib::zipserve::BodyFormat::Html) => {
            wizreader_lib::library::save_note_html(&lib, &index_db, &guid, &text)
        }
        None => wizreader_lib::library::save_note_body(&lib, &index_db, &guid, &text),
    });
}

/// 取位置参数；形如 `--xxx` 的值一律拒绝。
///
/// 用途：`rename-note <guid> <new-title>` 这类命令里标题/目录是**位置参数**。若按
/// `--title "x"` 的习惯传参，旧写法会把字面量 `--title` 当标题**静默写进库**
/// （真机验收踩到过：库内文件名变成 `--title.zip`）。宁可直接报错，也不静默毁数据。
fn positional_arg(args: &[String], idx: usize, field: &str, usage: &str) -> String {
    match args.get(idx) {
        Some(v) if !v.starts_with("--") => v.clone(),
        Some(v) => {
            eprintln!("❌ {field} 收到 `{v}`：这看起来是选项名而非值（位置参数不能以 `--` 开头）");
            eprintln!("用法: {usage}");
            std::process::exit(2);
        }
        None => {
            eprintln!("用法: {usage}");
            std::process::exit(1);
        }
    }
}

fn cmd_rename_note(args: &[String]) {
    const USAGE: &str = "wiz-cli rename-note <guid> <new-title> [--library-dir <dir>]";
    let guid = positional_arg(args, 1, "guid", USAGE);
    let title = positional_arg(args, 2, "新标题", USAGE);
    let lib = library_dir_arg(args);
    let index_db = library_index_db_arg(args, &lib);
    run_write(&lib, "rename-note", || {
        wizreader_lib::library::rename_note(&lib, &index_db, &guid, &title)
    });
}

fn cmd_move_note(args: &[String]) {
    const USAGE: &str = "wiz-cli move-note <guid> \"/目标/目录/\" [--library-dir <dir>]";
    let guid = positional_arg(args, 1, "guid", USAGE);
    let loc = positional_arg(args, 2, "目标目录", USAGE);
    let lib = library_dir_arg(args);
    let index_db = library_index_db_arg(args, &lib);
    run_write(&lib, "move-note", || {
        wizreader_lib::library::move_note(&lib, &index_db, &guid, &loc)
    });
}

fn cmd_delete_note(args: &[String]) {
    const USAGE: &str = "wiz-cli delete-note <guid> [--library-dir <dir>]";
    let guid = positional_arg(args, 1, "guid", USAGE);
    let lib = library_dir_arg(args);
    let index_db = library_index_db_arg(args, &lib);
    run_write(&lib, "delete-note", || {
        wizreader_lib::library::delete_note(&lib, &index_db, &guid)
    });
    println!("   → 回收站: {}", lib.join("_trash").display());
}

/// 从回收站恢复（T7）：文件回原路径 + 按墓碑快照逐字段还原清单行
fn cmd_restore_note(args: &[String]) {
    let guid = match args.get(1) {
        Some(g) if !g.starts_with("--") => g.clone(),
        _ => {
            eprintln!("用法: wiz-cli restore-note <guid> [--library-dir <dir>]");
            std::process::exit(1);
        }
    };
    let lib = library_dir_arg(args);
    let index_db = library_index_db_arg(args, &lib);
    run_write(&lib, "restore-note", || {
        wizreader_lib::library::restore_note(&lib, &index_db, &guid)
    });
}

/// 列出库回收站（只读打开清单；不触发迁移 —— v3 库按列存在性降级为"无恢复载荷"）
fn cmd_list_trash(args: &[String]) {
    let lib = library_dir_arg(args);
    let items = match wizreader_lib::library::list_trash(&lib) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ 读取回收站失败: {e}");
            std::process::exit(1);
        }
    };
    println!("🗑  回收站：{}（{} 项）", lib.join("_trash").display(), items.len());
    for it in &items {
        println!(
            "  [{}] {}  ← {}（{} B，移除于 {}，剩 {} 天）",
            it.guid, it.title, it.last_path, it.size, it.removed_at, it.days_left
        );
        if !it.restorable {
            println!("      ⚠️ 不可恢复：{}", it.reason);
        } else if !it.has_snapshot {
            println!("      ⚠️ 无恢复载荷（v3 及更早墓碑）：只能恢复文件，不能还原清单行");
        }
        if let Some(t) = &it.trash_rel {
            println!("      文件：{t}");
        }
    }
}

/// 在独立 tokio 运行时里跑 async 任务（CLI 单命令进程，直接 block_on）
fn run_in_blocking<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时失败")
        .block_on(fut)
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn default_data_dir() -> Option<PathBuf> {
    // 从应用配置 ~/.wizreader/settings.json 读取（若已设置）
    wizreader_lib::config::load_settings().source_dir.map(PathBuf::from)
}

// ---------------- Markdown（§20：库内格式改 md 包） ----------------

/// 直接读某个 zip 文件里的 `index.html`（BOM 已剥离）。
/// 给 `md-export` 用（它的输入恒为**为知原生 zip**：整目录扫 `notes/` 下的原始包），
/// 不依赖库清单或源索引。
fn read_zip_index_html(path: &std::path::Path) -> Result<String, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("{}: {e}", path.display()))?;
    // 为知 zip 的正文条目名固定为 index.html；个别缺损篇按失败计
    let mut entry = z
        .by_name("index.html")
        .map_err(|_| format!("{}: 无 index.html 条目", path.display()))?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut buf).map_err(|e| e.to_string())?;
    Ok(wizreader_lib::zipserve::decode_utf8_sig(&buf))
}

/// 从**裸 zip 文件**读正文源文本，形态自适应（M3/§20.3）：有 `note.md` 取它，否则取
/// `index.html`。给 `md-preview --zip` 用 —— 这样"库内 md 包"与"源原生 zip"都能喂。
fn read_zip_note_source(
    path: &std::path::Path,
) -> Result<(wizreader_lib::zipserve::BodyFormat, String), String> {
    use wizreader_lib::zipserve::BodyFormat;
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("{}: {e}", path.display()))?;
    let names: Vec<String> = z.file_names().map(|s| s.to_string()).collect();
    let is_md = names.iter().any(|n| n == wizreader_lib::md::NOTE_MD);
    let entry = if is_md { wizreader_lib::md::NOTE_MD } else { "index.html" };
    let mut e = z
        .by_name(entry)
        .map_err(|_| format!("{}: 包内既无 note.md 也无 index.html", path.display()))?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut e, &mut buf).map_err(|e| e.to_string())?;
    let fmt = if is_md { BodyFormat::Md } else { BodyFormat::Html };
    Ok((fmt, wizreader_lib::zipserve::decode_utf8_sig(&buf)))
}

/// 按 `--zip` > 库 > 源 的优先级取某篇的正文**源文本**（形态自适应），
/// 返回 `(形态, 源文本, 来源描述)`。
///
/// 形态一并回传是 M3 的关键：md 包里的正文**已经**是 Markdown，再跑一遍
/// HTML→MD 转换只会把 Markdown 当 HTML 洗一遍（表格/围栏全毁）。
fn load_note_source(
    args: &[String],
    guid: &str,
) -> Result<(wizreader_lib::zipserve::BodyFormat, String, String), String> {
    use wizreader_lib::zipserve::BodyFormat;
    if let Some(z) = flag_value(args, "--zip") {
        let p = PathBuf::from(&z);
        let (fmt, text) = read_zip_note_source(&p)?;
        return Ok((fmt, text, format!("zip:{}", p.display())));
    }
    let lib = library_dir_arg(args);
    if lib.join("export.db").is_file() {
        if let Ok(r) = wizreader_lib::library::LibraryResolver::new(lib.clone()) {
            let zs = wizreader_lib::zipserve::ZipService::with_resolver(std::sync::Arc::new(r));
            if let Ok((fmt, text)) = zs.read_note_body(guid) {
                return Ok((fmt, text, format!("library:{}", lib.display())));
            }
        }
    }
    let data_dir = flag_value(args, "--data-dir")
        .map(PathBuf::from)
        .or_else(default_data_dir)
        .ok_or_else(|| "库不可读且未配置数据源目录（--library-dir / --data-dir）".to_string())?;
    let zs = wizreader_lib::zipserve::ZipService::new(data_dir.join("notes"));
    let html = zs
        .read_index_html(guid)
        .map_err(|e| format!("读源笔记失败: {e:?}"))?;
    Ok((BodyFormat::Html, html, format!("source:{}", data_dir.display())))
}

fn cmd_md_preview(args: &[String]) {
    let guid = match args.get(1).filter(|s| !s.starts_with("--")) {
        Some(g) => g.clone(),
        None => {
            eprintln!(
                "用法: wiz-cli md-preview <guid> [--zip <path>] [--library-dir <dir>] [--data-dir <dir>] [--html|--doc] [--out <file>] [--stats]"
            );
            std::process::exit(1);
        }
    };
    let (fmt, src, origin) = match load_note_source(args, &guid) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    // md 包：正文**已是** Markdown，直接采用（不转换、不计统计）；
    // 原生包：跑一遍 HTML→MD。两者的产物形态相同，故下游（--html/--doc/--out）共用一条路。
    let already_md = fmt == wizreader_lib::zipserve::BodyFormat::Md;
    let (md, stats) = if already_md {
        (src.clone(), wizreader_lib::md::MdStats::default())
    } else {
        wizreader_lib::md::html_to_md_with_stats(&src)
    };
    let text = if args.iter().any(|a| a == "--doc") {
        wizreader_lib::md::md_to_html_document(&md, &guid)
    } else if args.iter().any(|a| a == "--html") {
        wizreader_lib::md::md_to_html(&md)
    } else {
        md.clone()
    };
    match flag_value(args, "--out") {
        Some(p) => match std::fs::write(&p, text.as_bytes()) {
            Ok(_) => eprintln!("✅ 已写入 {}（{} 字节）", p, text.len()),
            Err(e) => {
                eprintln!("❌ 写入失败: {e}");
                std::process::exit(1);
            }
        },
        None => println!("{text}"),
    }
    if already_md {
        eprintln!(
            "来源 {}   形态 md 包（包内 note.md 即 Markdown，未做转换）   正文 {} 字节   md {} 字节",
            origin,
            src.len(),
            md.len()
        );
    } else {
        eprintln!(
            "来源 {}   形态 为知原生包   原文 {} 字节 / md {} 字节   代码块 {}（含代码排版表 {}）/ 吞镜像 {} / 表格 {} / 摊平布局表 {} / 丢弃内联图 {}",
            origin,
            src.len(),
            md.len(),
            stats.code_blocks,
            stats.code_tables,
            stats.mirrors,
            stats.tables,
            stats.layout_tables,
            stats.dropped_data_imgs
        );
    }
}

fn collect_zips(root: &std::path::Path, out: &mut Vec<PathBuf>) {
    let rd = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_zips(&p, out);
            continue;
        }
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // 两种形态都要收：库内是 `{标题}.zip`；为知源笔记是**无扩展名的 `{GUID}`**
        // （内容同样是 zip），M2 导入改 md 时要直接吃源目录。
        let is_zip = p.extension().map(|x| x == "zip").unwrap_or(false);
        let is_src_note =
            !name.contains('.') && wizreader_lib::zipserve::normalize_guid(name).is_some();
        if is_zip || is_src_note {
            out.push(p);
        }
    }
}

fn cmd_md_export(args: &[String]) {
    let positional =
        |i: usize| -> Option<PathBuf> { args.get(i).filter(|s| !s.starts_with("--")).map(PathBuf::from) };
    let (src, dest) = match (positional(1), positional(2)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            eprintln!("用法: wiz-cli md-export <src_dir> <dest_dir> [--limit N]");
            std::process::exit(1);
        }
    };
    let limit: usize = flag_value(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);

    let mut zips = Vec::new();
    collect_zips(&src, &mut zips);
    zips.sort();
    if zips.len() > limit {
        zips.truncate(limit);
    }
    if zips.is_empty() {
        eprintln!("❌ {} 下没有 .zip", src.display());
        std::process::exit(1);
    }

    let t0 = std::time::Instant::now();
    let (mut ok, mut failed, mut code, mut code_tbl, mut tables, mut layout, mut mirrors) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut failures: Vec<String> = Vec::new();
    for z in &zips {
        let html = match read_zip_index_html(z) {
            Ok(h) => h,
            Err(e) => {
                failed += 1;
                if failures.len() < 5 {
                    failures.push(e);
                }
                continue;
            }
        };
        let (md, st) = wizreader_lib::md::html_to_md_with_stats(&html);
        code += st.code_blocks;
        code_tbl += st.code_tables;
        tables += st.tables;
        layout += st.layout_tables;
        mirrors += st.mirrors;
        let rel = z.strip_prefix(&src).unwrap_or(z);
        let mut target = dest.join(rel);
        target.set_extension("md");
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&target, md.as_bytes()) {
            Ok(_) => ok += 1,
            Err(e) => {
                failed += 1;
                if failures.len() < 5 {
                    failures.push(format!("{}: {e}", target.display()));
                }
            }
        }
    }
    println!(
        "✅ 转换 {} 篇（失败 {}）→ {}\n   代码块 {}（其中代码排版表 {}）/ 表格 {} / 摊平布局表 {} / 吞镜像 {}   耗时 {} ms",
        ok,
        failed,
        dest.display(),
        code,
        code_tbl,
        tables,
        layout,
        mirrors,
        t0.elapsed().as_millis()
    );
    for f in &failures {
        eprintln!("   ⚠️ {f}");
    }
}
