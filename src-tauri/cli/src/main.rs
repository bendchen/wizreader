//! 命令行工具（M1 交付物）：
//!   wiz-cli build-index <data_dir> [--index-dir <dir>]
//!   wiz-cli search <kw> [--folder <path>]
//!   wiz-cli peek <guid> [entry]                          读取笔记 zip 内文件（协议链路验证）

use std::path::PathBuf;

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
        _ => {
            eprintln!("WizReader CLI（M1）");
            eprintln!("  wiz-cli build-index <data_dir> [--index-dir <dir>]   构建派生索引并输出校验报告");
            eprintln!("  wiz-cli search <kw> [--folder <path>]                全文检索（≥3 字符走 FTS5 trigram）");
            eprintln!("  wiz-cli peek <guid> [entry] [--data-dir <path>]    读取笔记 zip 内文件（协议链路验证）");
        }
    }
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
