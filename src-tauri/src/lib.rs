//! WizReader —— 为知笔记存量数据只读阅读器
//!
//! 架构（需求文档 §7.1）：
//! - 源数据层只读接入（G1/G2）
//! - 派生索引层 ~/.wizreader/index.db（可删除重建）
//! - 核心服务：zip 流式读取 / 检索 / 导出（Rust）
//! - UI：Vue3 三栏布局，iframe 加载 wiznote:// 协议

/// 定义一个「编译一次、进程内复用」的 Regex 访问器。
/// 正则编译开销很大（Unicode 大小写折叠尤其），绝不能放在逐篇循环里。
macro_rules! regex_of {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static ::regex::Regex {
            static RE: ::std::sync::OnceLock<::regex::Regex> = ::std::sync::OnceLock::new();
            RE.get_or_init(|| ::regex::Regex::new($pat).expect("非法正则"))
        }
    };
}

pub mod commands;
pub mod config;
pub mod credential;
pub mod export;
pub mod extract;
pub mod indexer;
pub mod library;
pub mod manifest;
pub mod md;
pub mod sandbox;
pub mod search;
pub mod store;
pub mod sync;
pub mod verify;
pub mod zipserve;

use std::sync::{Arc, Mutex};

use percent_encoding::percent_decode_str;
use tauri::http::{header, Response, StatusCode};
use tauri::menu::{Menu, MenuItem, Submenu};
use tauri::{Emitter, Manager};

use commands::{AppState, ViewContext};
use zipserve::{content_type, placeholder_svg, NotePathResolver, ZipError, ZipService};

/// 解析 wiznote:// URI → (guid, path)
/// 兼容 macOS（wiznote://{guid}/{path}）与 Windows 归一化（http://wiznote.localhost/）变体
///
/// 查询串（`?a=b`）**不在 path 里返回**：编辑抽屉的预览用 `?draft=<token>` 区分
/// "要看草稿"还是"要看库内实况"，这个参数由调用方另取（见 [`query_of`](fn@query_of)），
/// 否则 `index.html?draft=x` 会被当成一个不存在的条目名。
fn parse_wiznote_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri
        .strip_prefix("wiznote://")
        .or_else(|| uri.strip_prefix("http://wiznote.localhost/"))?;
    let rest = rest.strip_prefix("localhost/").unwrap_or(rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (guid, path) = rest.split_once('/')?;
    Some((guid.to_string(), path.to_string()))
}

/// 取 URI 查询串里某个键的值（未命中 → None）
fn query_of(uri: &str, key: &str) -> Option<String> {
    let q = uri.split_once('?')?.1;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

fn html_response(bytes: Vec<u8>, nonce: &str, allow_remote: bool) -> tauri::http::Response<std::borrow::Cow<'static, [u8]>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_SECURITY_POLICY, sandbox::note_csp(nonce, allow_remote))
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(std::borrow::Cow::Owned(bytes))
        .unwrap()
}

fn err_response(status: StatusCode, msg: &str) -> tauri::http::Response<std::borrow::Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(std::borrow::Cow::Owned(msg.as_bytes().to_vec()))
        .unwrap()
}

/// 项目 GitHub 主页（「关于」菜单入口）
const GITHUB_URL: &str = "https://github.com/bendchen/wizreader";

/// 启动分支判定（§6.4）——「打开什么」的**唯一决策点**，单列以便直测：
/// - `dir = None`（未设置数据目录）→ `(None, None)`：不打开任何笔记，也无需报错；
/// - 清单（export.db）可读 → `(Some(resolver), None)`：打开笔记库；
/// - 清单不可读 → `(None, Some(原因))`：不打开任何笔记，原因进 sync.log + 空态页。
///
/// 注意：**没有**「回退为知源视图」这一支 —— 源视图只能由用户显式进入。
fn probe_library(
    dir: Option<&std::path::Path>,
) -> (Option<library::LibraryResolver>, Option<String>) {
    match dir {
        None => (None, None),
        Some(d) => match library::LibraryResolver::new(d.to_path_buf()) {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(format!("{}：{e}", d.display()))),
        },
    }
}

pub fn run() {
    let settings = config::load_settings();
    // 启动上下文（§6.4）——只有两条路：
    //   ① library_dir 的清单（export.db）**可读** → Library：打开笔记库；
    //   ② 未设置 library_dir，或清单不可读    → **None：不打开任何笔记**。
    // ②不再回退为知源视图：源视图只能由用户显式进入（「读取为知笔记 ▸ 浏览/检索」），
    // 否则用户极易把为知原始数据误当自有笔记（此前是静默回退，界面上无任何提示）。
    let library_dir = settings.library_dir.as_ref().map(std::path::PathBuf::from);
    let (lib_resolver, lib_probe_error) = probe_library(library_dir.as_deref());
    if let Some(e) = &lib_probe_error {
        // §6.3 硬约束：启动失败不弹框，只记 sync.log；界面由空态页如实告知
        commands::append_sync_log(&format!(
            "启动：数据目录清单不可读，不打开任何笔记 —— {e}"
        ));
    }
    let (zip, initial_ctx, resolver_handle) = if let Some(r) = lib_resolver {
        let r = Arc::new(r);
        (
            ZipService::with_resolver(r.clone() as Arc<dyn NotePathResolver>),
            ViewContext::Library,
            // 同一实例留给写路径（reload + 清缓存）：句柄与 ZipService 内必须是**同一个** Arc
            Some(r),
        )
    } else {
        // 不打开任何笔记：空解析器，任何 guid 都读不到数据
        (ZipService::none(), ViewContext::None, None)
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            zip,
            settings: Mutex::new(settings),
            context: Mutex::new(initial_ctx),
            library_resolver: Mutex::new(resolver_handle),
            draft: Mutex::new(None),
        })
        .register_uri_scheme_protocol("wiznote", |ctx, request| {
            let app = ctx.app_handle();
            let uri = request.uri().to_string();
            let allow_remote = {
                let state0 = app.state::<AppState>();
                let s = state0.settings.lock().unwrap();
                s.allow_remote
            };
            let Some((guid, raw_path)) = parse_wiznote_uri(&uri) else {
                return err_response(StatusCode::BAD_REQUEST, "URI 格式错误");
            };
            let path = percent_decode_str(&raw_path).decode_utf8_lossy().to_string();

            // 宿主兼容层脚本
            if guid == "_compat" && path.ends_with(".js") {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/javascript; charset=utf-8")
                    .header(header::CACHE_CONTROL, "no-store")
                    .body(std::borrow::Cow::Owned(sandbox::COMPAT_JS.as_bytes().to_vec()))
                    .unwrap();
            }

            let state = app.state::<AppState>();
            if guid != "_compat" && path == "index.html" {
                // 主文档：物化注入 + CSP 沙箱（每篇一个 nonce，见 sandbox.rs）
                //
                // **地址不随库内形态变**（M3/§20.8）：`wiznote://{guid}/index.html` 是协议的
                // "主文档地址"，md 包在这里由 [`ZipService::read_note_document`] 把 `note.md`
                // 渲染成 HTML 后返回，native 包照旧返回 `index.html`。这样做的两个理由：
                // ① 前端与 URL 一行不改，两种库共用同一条阅读链路；
                // ② 相对资源（`index_files/…`）在两种形态下都按同目录解析，图不会裂。
                // 编辑抽屉预览：带 `?draft=<token>` 时以草稿为正文（token 与 guid 双命中才生效，
                // 且草稿按包内形态渲染），其余请求一律走 zip 实况。
                let draft = query_of(&uri, "draft").and_then(|t| state.draft_html_for(&guid, &t));
                let html = match draft {
                    Some(h) => Ok(h),
                    None => state.zip.read_note_document(&guid, &guid),
                };
                match html {
                    Ok(html) => {
                        let n = sandbox::nonce();
                        let injected =
                            extract::inject_before_body_close(&html, &sandbox::compat_script_tag(&n));
                        return html_response(injected.into_bytes(), &n, allow_remote);
                    }
                    Err(e) => return err_response(StatusCode::NOT_FOUND, &e.message()),
                }
            }

            match state.zip.read_entry(&guid, &path) {
                Ok(bytes) => {
                    let ct = content_type(&path);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, ct)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(std::borrow::Cow::Owned(bytes))
                        .unwrap()
                }
                Err(ZipError::NoSuchEntry(_)) => {
                    // P4：缺失资源返回占位图而非报错
                    if content_type(&path).starts_with("image/") {
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "image/svg+xml")
                            .body(std::borrow::Cow::Owned(placeholder_svg(&path)))
                            .unwrap()
                    } else {
                        err_response(StatusCode::NOT_FOUND, "条目缺失")
                    }
                }
                Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, &e.message()),
            }
        })
        .register_uri_scheme_protocol("wiznote-action", |_ctx, request| {
            // 动作协议：open-url?url=<外链> → 系统浏览器（NFR-3.3）
            let uri = request.uri().to_string();
            let decoded = percent_decode_str(&uri).decode_utf8_lossy().to_string();
            if let Some(idx) = decoded.find("url=") {
                let url = &decoded[idx + 4..];
                if url.starts_with("http://") || url.starts_with("https://") {
                    let _ = tauri_plugin_opener::open_url(url, None::<&str>);
                }
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/plain")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(std::borrow::Cow::Owned(b"ok".to_vec()))
                .unwrap()
        })
        .setup(|app| {
            // 「关于」菜单：版本信息 + GitHub 项目主页
            let menu = Menu::default(app.handle())?;
            let version = MenuItem::with_id(
                app,
                "about-version",
                format!("WizReader v{}", app.package_info().version),
                false,
                None::<&str>,
            )?;
            let github = MenuItem::with_id(app, "about-github", "GitHub 项目主页", true, None::<&str>)?;
            let about = Submenu::with_items(app, "关于", true, &[&version, &github])?;
            menu.append(&about)?;
            app.set_menu(menu)?;

            // 启动任务（设计稿 §6.3，用户问题 3）：挂 setup 之后的独立后台任务。
            // 硬约束：绝不阻塞首屏、失败只写 sync.log + 状态行，绝不弹框；
            // 幂等（进程内 AtomicBool + .sync.lock），断网/未配置时静默退出。
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let progress: crate::sync::ProgressFn = std::sync::Arc::new(|phase, done, total| {
                    // 启动期窗口可能尚未监听事件，emit 失败静默
                    let _ = (phase, done, total);
                });
                match crate::sync::startup_tasks(progress).await {
                    Ok(summary) => {
                        crate::commands::append_sync_log(&format!("启动任务完成: {summary}"));
                        let _ = handle.emit("sync-status", serde_json::json!({
                            "phase": "startup", "level": "info", "message": summary
                        }));
                    }
                    Err(e) => {
                        // 离线/未配置是常态而非故障：记日志 + 状态行 warn，不弹框
                        crate::commands::append_sync_log(&format!("启动任务跳过/失败: {e}"));
                        let _ = handle.emit("sync-status", serde_json::json!({
                            "phase": "startup", "level": "warn", "message": e
                        }));
                    }
                }
            });

            // 库索引自检（§6.4）：库上下文下，索引缺失或篇数≠清单 → 后台自动重建。
            // 硬约束：不阻塞首屏，失败仅记 sync.log，绝不弹框。
            let handle2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let (ctx, lib) = {
                    let state = handle2.state::<AppState>();
                    (state.view_context(), state.library_dir())
                };
                if ctx != ViewContext::Library {
                    return;
                }
                let Some(lib) = lib else { return };
                let index_db = config::index_file_for_library(&lib);
                let resolver = match library::LibraryResolver::new(lib.clone()) {
                    Ok(r) => r,
                    Err(_) => return,
                };
                let manifest_notes = resolver.note_count() as i64;
                let need_rebuild = if index_db.exists() {
                    let idx_notes = rusqlite::Connection::open_with_flags(
                        &index_db,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )
                    .and_then(|c| c.query_row("SELECT count(*) FROM note", [], |r| r.get::<_, i64>(0)))
                    .unwrap_or(-1);
                    idx_notes != manifest_notes
                } else {
                    true
                };
                if !need_rebuild {
                    return;
                }
                let h = handle2.clone();
                let _ = tauri::async_runtime::spawn_blocking(move || {
                    let resolver = std::sync::Arc::new(resolver);
                    match indexer::build_library_index(&lib, resolver, &index_db, &|done, total| {
                        let _ = h.emit("index-progress", serde_json::json!({"done": done, "total": total}));
                    }) {
                        Ok(rep) => commands::append_sync_log(&format!(
                            "库索引自检重建完成: {} 篇, ok={}",
                            rep.note_count, rep.ok
                        )),
                        Err(e) => commands::append_sync_log(&format!("库索引自检重建失败: {e}")),
                    }
                })
                .await;
            });
            Ok(())
        })
        .on_menu_event(|_app, event| {
            if event.id() == "about-github" {
                let _ = tauri_plugin_opener::open_url(GITHUB_URL, None::<&str>);
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::update_settings,
            commands::check_index,
            commands::build_index_cmd,
            commands::get_tree,
            commands::list_notes,
            commands::get_note_detail,
            commands::preview_attachment,
            commands::open_attachment_external,
            commands::reveal_in_finder,
            commands::save_attachment_as,
            commands::open_external_url,
            commands::search_cmd,
            commands::get_search_history,
            commands::clear_search_history,
            commands::get_unlinked,
            commands::export_note_zip_cmd,
            commands::export_note_html_cmd,
            commands::export_folder_cmd,
            commands::export_folder_zips_cmd,
            commands::run_verify_cmd,
            commands::save_verify_report,
            commands::pick_default_data_dir,
            commands::detect_wiznote_dir,
            // 笔记库（FR-11 P0/P1）
            commands::pick_library_dir,
            commands::build_library_index_cmd,
            commands::import_to_library,
            commands::get_library_status,
            commands::set_view_context,
            // 笔记库写入（FR-11 P2 / S1）
            commands::create_note_cmd,
            commands::rename_note_cmd,
            commands::move_note_cmd,
            commands::delete_note_cmd,
            // 笔记库编辑（FR-11 P2 / S2）：读正文源码 + 预览草稿
            commands::get_note_source,
            commands::get_note_file_path,
            commands::set_note_draft,
            commands::clear_note_draft,
            // 正文写（§4.3）：按包内形态自动分派（md 包 → note.md；native → index.html）
            commands::save_note_source_cmd,
            // 云同步（阶段二 FR-07）
            commands::get_sync_config,
            commands::test_cloud_connection,
            commands::save_sync_config,
            commands::clear_cloud_credentials,
            // U1：`pick_sync_root` 已删（同步根恒为库根，唯一可选的是 `pick_library_dir`）
            commands::init_cloud_sync,
            commands::run_sync,
            commands::get_sync_status,
            commands::run_startup_tasks,
            commands::list_trash,
            commands::trash_stats,
            commands::restore_trash,
            commands::purge_trash,
            commands::open_trash_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_uri_shapes() {
        assert_eq!(
            parse_wiznote_uri("wiznote://{0002c9c7-e874-436f-be3d-941734660f15}/index.html"),
            Some((
                "{0002c9c7-e874-436f-be3d-941734660f15}".into(),
                "index.html".into()
            ))
        );
        // Windows 下 Tauri 会把自定义协议归一化为 http://wiznote.localhost/
        assert!(parse_wiznote_uri("http://wiznote.localhost/{guid}/index_files/a.png").is_some());
        assert!(parse_wiznote_uri("https://example.com/x").is_none());
    }

    /// 查询串不得混进 path（否则 `index.html?draft=x` 会被当成不存在的条目名）
    #[test]
    fn strips_query_from_uri() {
        let (g, p) = parse_wiznote_uri("wiznote://{g1}/index.html?draft=abc").unwrap();
        assert_eq!((g.as_str(), p.as_str()), ("{g1}", "index.html"));
        assert_eq!(query_of("wiznote://{g1}/index.html?draft=abc", "draft").as_deref(), Some("abc"));
        assert_eq!(query_of("wiznote://{g1}/index.html", "draft"), None);
        // 相对资源带查询串同样不受影响
        let (_, p2) = parse_wiznote_uri("wiznote://{g1}/index_files/a.png?v=2").unwrap();
        assert_eq!(p2, "index_files/a.png");
    }

    /// §6.4 启动分支三态：清单**可读才打开**，否则不打开任何笔记（无「回退为知源」这一支）
    #[test]
    fn test_startup_branch_three_cases() {
        // ① 未设置数据目录 → 不打开任何笔记，且无需报错（静默）
        let (r, err) = probe_library(None);
        assert!(r.is_none() && err.is_none(), "未设置目录：静默不打开");

        let d = std::env::temp_dir().join(format!("wiz-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        // ② 目录在但没有清单 → 不打开任何笔记，并给出原因（供 sync.log / 空态页）
        let (r, err) = probe_library(Some(&d));
        assert!(r.is_none(), "无 export.db 不得打开任何笔记");
        assert!(err.is_some(), "无 export.db 应给出不可读原因");

        // ③ 清单可读 → 打开笔记库
        manifest::open_or_create(&d).unwrap();
        let (r, err) = probe_library(Some(&d));
        assert!(r.is_some(), "清单可读应打开笔记库");
        assert!(err.is_none());
        assert_eq!(r.unwrap().note_count(), 0);

        std::fs::remove_dir_all(&d).unwrap();
    }
}
