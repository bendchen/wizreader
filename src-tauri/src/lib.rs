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
pub mod export;
pub mod extract;
pub mod indexer;
pub mod manifest;
pub mod sandbox;
pub mod search;
pub mod verify;
pub mod zipserve;

use std::sync::Mutex;

use percent_encoding::percent_decode_str;
use tauri::http::{header, Response, StatusCode};
use tauri::menu::{Menu, MenuItem, Submenu};
use tauri::Manager;

use commands::AppState;
use zipserve::{content_type, placeholder_svg, ZipError, ZipService};

/// 解析 wiznote:// URI → (guid, path)
/// 兼容 macOS（wiznote://{guid}/{path}）与 Windows 归一化（http://wiznote.localhost/）变体
fn parse_wiznote_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri
        .strip_prefix("wiznote://")
        .or_else(|| uri.strip_prefix("http://wiznote.localhost/"))?;
    let rest = rest.strip_prefix("localhost/").unwrap_or(rest);
    let (guid, path) = rest.split_once('/')?;
    Some((guid.to_string(), path.to_string()))
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

pub fn run() {
    let settings = config::load_settings();
    let notes_dir = settings
        .data_dir
        .as_ref()
        .map(|d| std::path::PathBuf::from(d).join("notes"))
        .unwrap_or_default();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            zip: ZipService::new(notes_dir),
            settings: Mutex::new(settings),
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
                match state.zip.read_index_html(&guid) {
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
}
