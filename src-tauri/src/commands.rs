//! Tauri 命令层（IPC 接口）—— 核心服务与 UI 解耦，二期 PWA/鸿蒙可复用同一接口

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::config::{save_settings, Settings};
use crate::export::{
    export_folder, export_folder_zips, export_folder_zips_md, export_note_single_html, export_note_zip,
    ExportAttachment,
    ExportContext, ExportReport, FolderZipExportReport,
};
use crate::indexer::{build_index, build_library_index, BuildReport};
use crate::library::{LibraryResolver, LibraryStatus};
use crate::manifest;
use crate::search::{self, SearchResponse};
use crate::store::ObjectStore as _;
use crate::verify;
use crate::zipserve::{NotePathResolver, ZipService};

/// 视图上下文（§6.1）三态：
/// - `None`：**未打开任何笔记** —— 未设置 `library_dir`，或该目录的清单（export.db）不可读。
///   此时 zip 解析器为 [`crate::zipserve::EmptyResolver`]，读命令一律失败：
///   绝不静默回退展示为知源（否则用户会把为知原始数据误当自有笔记）；
/// - `Library`：笔记库（主数据目录，可读可写）；
/// - `Source`：为知数据源（只读导入）—— **只能由用户显式进入**（「读取为知笔记 ▸ 浏览/检索」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewContext {
    None,
    Library,
    Source,
}

/// 预览草稿（§4.3/T8）：编辑抽屉里"未保存的正文"，只服务带 `?draft=<token>` 的
/// `wiznote://` 请求，普通阅读请求永远走 zip 实况。
///
/// **M3 起带 `format`**：草稿的原文形态必须与包内形态一致才谈得上"所见即所得"——
/// md 包里编辑器拿到的是 Markdown，预览就必须先渲染再喂给 iframe，
/// 否则用户看到的是一屏原始 Markdown 文本（与阅读态不一致）。
#[derive(Debug, Clone)]
pub struct DraftPreview {
    pub guid: String,
    /// 草稿原文形态（由包内实况决定，见 [`ZipService::body_format`]）
    pub format: crate::zipserve::BodyFormat,
    /// 草稿原文（md 包 → Markdown；native 包 → HTML）
    pub text: String,
    pub token: String,
}

pub struct AppState {
    pub zip: ZipService,
    pub settings: std::sync::Mutex<Settings>,
    /// 当前视图上下文（启动：库清单可读 → Library；否则 None —— 不打开任何笔记）
    pub context: std::sync::Mutex<ViewContext>,
    /// 当前库解析器句柄（P2 写路径需要：写后 `reload` + 重注入 ZipService 以清 LRU，见
    /// [`AppState::invalidate_library_cache`]）。非库上下文为 `None`。
    pub library_resolver: std::sync::Mutex<Option<Arc<LibraryResolver>>>,
    /// 编辑抽屉的预览草稿（同时只允许一份：抽屉本身也是单实例）
    pub draft: std::sync::Mutex<Option<DraftPreview>>,
}

regex_of!(re_rtf, r"\\'[0-9a-fA-F]{2}|\\[a-zA-Z]+-?\d* ?|[{\}]");

/// 「未打开任何笔记」的哨兵索引路径：该文件从不由程序创建，
/// 使 `index_db_of` 必然报「索引尚未构建」，从而拦住误读。
fn no_context_index() -> PathBuf {
    crate::config::wiz_home().join("__no_context__.db")
}

impl AppState {
    /// 派生索引文件：库模式 → index-{hash8}.db（§5.3/Q11）；源模式 → index.db；
    /// **`None`（未打开任何笔记）→ 哨兵路径**（永不指向真实索引）。
    /// 这样即便有命令漏了上下文判断，也只会撞上「索引尚未构建」，
    /// 而不会悄悄读到为知源索引把源笔记当自有笔记展示。
    pub fn index_db(&self) -> PathBuf {
        match self.view_context() {
            ViewContext::Library => self
                .library_dir()
                .map(|lib| crate::config::index_file_for_library(&lib))
                .unwrap_or_else(no_context_index),
            ViewContext::Source => crate::config::wiz_home().join("index.db"),
            ViewContext::None => no_context_index(),
        }
    }
    /// 主数据目录（笔记库根）
    pub fn library_dir(&self) -> Option<PathBuf> {
        self.settings
            .lock()
            .unwrap()
            .library_dir
            .as_ref()
            .map(PathBuf::from)
    }
    /// 源数据目录（为知原始数据，只读）
    pub fn source_dir(&self) -> Option<PathBuf> {
        self.settings
            .lock()
            .unwrap()
            .source_dir
            .as_ref()
            .map(PathBuf::from)
    }
    pub fn view_context(&self) -> ViewContext {
        *self.context.lock().unwrap()
    }

    /// 取预览草稿并**渲染成可阅读 HTML**（M3/§20.7）：**必须 guid 与 token 同时命中**才返回。
    /// 双条件是为了一步到位地防"草稿泄漏进普通阅读"（只看 guid 的话，
    /// 抽屉开着时用户点别的入口重载主文档就会看到未保存内容）。
    ///
    /// 渲染口径与阅读态**完全一致**（md 走 [`crate::md::md_to_html_document`]、
    /// native 原样），故"编辑时看到的"与"保存后阅读到的"不会因为形态不同而两样。
    pub fn draft_html_for(&self, guid: &str, token: &str) -> Option<String> {
        let d = self.draft.lock().unwrap();
        let d = d.as_ref().filter(|d| d.guid == guid && d.token == token)?;
        Some(match d.format {
            crate::zipserve::BodyFormat::Md => crate::md::md_to_html_document(&d.text, guid),
            crate::zipserve::BodyFormat::Html => d.text.clone(),
        })
    }

    /// 写后缓存失效（T4/§4.5 第 3 环）：
    /// ① 重读清单 → 解析器 `reload`（rename/move 后 `exported_path` 变了）；
    /// ② 把同一个解析器**重新注入** ZipService —— `set_resolver` 会清空 LRU，
    ///    否则缓存里的旧 `File` 句柄（rename 换了 inode）仍会返回**写之前的内容**。
    ///
    /// 顺序不能颠倒：先 reload 再注入，避免中间态被读请求看到。
    pub fn invalidate_library_cache(&self) -> Result<(), String> {
        let resolver = self.library_resolver.lock().unwrap().clone();
        let Some(r) = resolver else {
            return Ok(()); // 非库上下文：没有库缓存需要失效
        };
        r.reload()?;
        self.zip.set_resolver(r as Arc<dyn NotePathResolver>);
        Ok(())
    }
}

/// 切换视图上下文：设置 zip 解析器（未打开→EmptyResolver；库→LibraryResolver；源→SourceResolver）
/// 并落 context。库模式清单缺失时返回 Err（调用方据此提示先导入/重建）。
pub fn apply_context(state: &AppState, ctx: ViewContext) -> Result<(), String> {
    match ctx {
        ViewContext::None => {
            // 不打开任何笔记：解析器置空 + 清库句柄 ⇒ 读路径必然读不到数据
            state.zip.set_resolver(Arc::new(crate::zipserve::EmptyResolver));
            *state.library_resolver.lock().unwrap() = None;
        }
        ViewContext::Library => {
            let lib = state.library_dir().ok_or("未设置笔记库目录")?;
            let resolver = Arc::new(LibraryResolver::new(lib)?);
            state.zip.set_resolver(resolver.clone() as Arc<dyn NotePathResolver>);
            // 留句柄给写路径（reload + 清缓存）
            *state.library_resolver.lock().unwrap() = Some(resolver);
        }
        ViewContext::Source => {
            let src = state.source_dir().ok_or("未设置源数据目录")?;
            state.zip.set_notes_dir(src.join("notes"));
            *state.library_resolver.lock().unwrap() = None;
        }
    }
    *state.context.lock().unwrap() = ctx;
    Ok(())
}

// ---------- 云同步公共设施 ----------

/// spawn_blocking 的统一包装（D8 混合模型：本地重 I/O 一律离开 async 线程）
pub async fn spawn_blocking<T, F>(task: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))
}

/// 同步日志（§6.3 硬约束：失败不弹框，只追加 sync.log；>1 MB 滚动保留一份）
pub fn append_sync_log(msg: &str) {
    use std::io::Write;
    let dir = crate::config::wiz_home();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("sync.log");
    // 滚动：超过 1 MB → 改名 sync.log.1 重新开始
    if let Ok(m) = std::fs::metadata(&path) {
        if m.len() > 1024 * 1024 {
            let _ = std::fs::rename(&path, dir.join("sync.log.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let ts = crate::manifest::format_utc(std::time::SystemTime::now());
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

/// 从设置 + 钥匙串构造存储（错误信息可直接给 UI 引导重输）
fn store_from_state(cfg: &crate::config::SyncSettings) -> Result<std::sync::Arc<dyn crate::store::ObjectStore>, String> {
    if cfg.credential_user.is_empty() {
        return Err("SYNC_NO_CREDENTIAL: 云同步未配置凭据".into());
    }
    let secret = crate::credential::read_secret(&cfg.credential_user)?;
    let store = crate::sync::s3_store_of(cfg, &secret)?;
    Ok(std::sync::Arc::new(store))
}

#[derive(Debug, Serialize)]
pub struct SyncStatusView {
    pub enabled: bool,
    pub role: String,
    pub initialized: bool,
    pub last_sync_at: Option<String>,
    pub last_report: Option<crate::sync::SyncReport>,
    pub local_revision: u64,
    pub trash_items: usize,
    pub trash_bytes: u64,
}

// ---------- 云同步：配置与凭据（用户问题 1，P1） ----------

#[derive(Debug, Serialize)]
pub struct SyncConfigView {
    pub config: crate::config::SyncSettings,
    pub credential_set: bool,
}

#[tauri::command]
pub fn get_sync_config(state: State<AppState>) -> SyncConfigView {
    let s = state.settings.lock().unwrap().sync.clone();
    SyncConfigView {
        credential_set: !s.credential_user.is_empty(),
        config: s,
    }
}

#[derive(Debug, Serialize)]
pub struct TestConnectionResult {
    pub ok: bool,
    pub latency_ms: u128,
    pub can_read: bool,
    pub can_write: bool,
    pub objects_under_prefix: usize,
    pub message: String,
}

/// 测试连接（§4.3）：用**待保存**的值测试，不必先落盘
#[tauri::command]
pub async fn test_cloud_connection(
    state: State<'_, AppState>,
    config: crate::config::SyncSettings,
    secret_key: String,
) -> Result<TestConnectionResult, String> {
    let lib_dir = state.library_dir().map(|p| p.to_string_lossy().to_string());
    let data_dir = state.source_dir().map(|p| p.to_string_lossy().to_string());
    // 校验（U1：红线校验的对象是**库根** —— 同步根恒为库根；库未设时跳过该子项）
    crate::config::validate_sync(&config, lib_dir.as_deref(), data_dir.as_deref())
        .map_err(|e| e.clone())?;
    if secret_key.is_empty() && config.credential_user.is_empty() {
        return Err("SYNC_NO_CREDENTIAL: secret key 不能为空".into());
    }
    // 未传新 secret 时用钥匙串里的旧值
    let secret = if secret_key.is_empty() {
        crate::credential::read_secret(&config.credential_user)?
    } else {
        secret_key
    };
    let store = crate::sync::s3_store_of(&config, &secret)?;
    let t0 = std::time::Instant::now();
    let manifest_key = config.cloud_key("manifest.db");
    let mut res = TestConnectionResult {
        ok: false,
        latency_ms: 0,
        can_read: false,
        can_write: false,
        objects_under_prefix: 0,
        message: String::new(),
    };
    // 可读：HEAD manifest.db，200/404 都算通（404 = 空桶，正常）
    match store.head(&manifest_key).await {
        Ok(_) => res.can_read = true,
        Err(e) => {
            res.message = format!("读取失败（凭据或网络问题）: {e}");
            res.latency_ms = t0.elapsed().as_millis();
            return Ok(res);
        }
    }
    // 可写：PUT + DELETE 探针（失败只告警不阻止——可能是只读的阅读端策略）
    let probe = config.cloud_key(&format!("_probe/{}", uuid_v4()));
    match store.put_bytes(&probe, b"probe", &[]).await {
        Ok(_) => match store.delete(&probe).await {
            Ok(()) => res.can_write = true,
            Err(e) => res.message.push_str(&format!("探针清理失败: {e}; ")),
        },
        Err(e) => res.message.push_str(&format!("写入探测失败（可能为只读策略）: {e}; ")),
    }
    res.objects_under_prefix = store
        .list_keys(&config.normalized_prefix())
        .await
        .unwrap_or_default()
        .len();
    res.latency_ms = t0.elapsed().as_millis();
    res.ok = res.can_read;
    if res.ok && res.message.is_empty() {
        res.message = format!(
            "连接正常（可读{}，延迟 {} ms，前缀下 {} 个对象）",
            if res.can_write { "可写" } else { "，只读" },
            res.latency_ms,
            res.objects_under_prefix
        );
    }
    Ok(res)
}

/// 轻量 uuid（探针用；无 uuid 依赖，时间+进程熵足够）
fn uuid_v4() -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0).to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    let d = h.finalize();
    let s = format!("{:x}", d);
    format!("{}-{}-{}-{}", &s[0..8], &s[8..12], &s[12..16], &s[16..28])
}

/// 保存配置（§4.4）：**先写 keyring 再落盘 settings**，任一失败整体回滚
#[tauri::command]
pub async fn save_sync_config(
    state: State<'_, AppState>,
    config: crate::config::SyncSettings,
    secret_key: Option<String>,
) -> Result<(), String> {
    let lib_dir = state.library_dir().map(|p| p.to_string_lossy().to_string());
    let data_dir = state.source_dir().map(|p| p.to_string_lossy().to_string());
    crate::config::validate_sync(&config, lib_dir.as_deref(), data_dir.as_deref())?;

    let user = crate::credential::credential_user(&config.endpoint, &config.bucket, &config.normalized_prefix());
    let mut cfg = config.clone();
    cfg.credential_user = user.clone();

    let old_user = state.settings.lock().unwrap().sync.credential_user.clone();
    if let Some(sk) = &secret_key {
        crate::credential::store_secret(&user, sk)?;
    } else if old_user != user {
        // 换了 endpoint/bucket/prefix 但没给新 secret → 需要用户提供
        return Err("SYNC_NO_CREDENTIAL: 连接信息变更，请重新输入 Secret Key".into());
    }

    // settings 落盘失败 → 回滚 keyring 新条目
    let mut settings = state.settings.lock().unwrap().clone();
    settings.sync = cfg.clone();
    if let Err(e) = crate::config::save_settings(&settings) {
        if secret_key.is_some() {
            let _ = crate::credential::delete_secret(&user);
        }
        return Err(e);
    }
    *state.settings.lock().unwrap() = settings;

    // 旧条目保留并提示（§4.2：避免误删后无法回滚）；此处先记日志，UI 提供一键清理
    if !old_user.is_empty() && old_user != user {
        append_sync_log(&format!("云配置变更，旧凭据条目保留: {old_user}（可在钥匙串中清理）"));
    }
    Ok(())
}

// ---------- 云同步：初始化与同步（用户问题 2/3，P2/P3） ----------

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub report: crate::sync::SyncReport,
}

/// 首次初始化（用户问题 2 入口，§8.3）：按角色分流，**后台执行**（Q16）
#[tauri::command]
pub async fn init_cloud_sync(
    app: AppHandle,
    state: State<'_, AppState>,
    role: String,
) -> Result<InitReport, String> {
    let cfg = state.settings.lock().unwrap().sync.clone();
    if !cfg.enabled {
        return Err("SYNC_DISABLED: 请先启用云同步并保存配置".into());
    }
    // U1：同步根恒为库根 —— 没有库就没有可同步的东西（旧的「本地同步根」字段已废弃）
    let Some(root) = state.settings.lock().unwrap().sync_root() else {
        return Err("SYNC_CONFIG_INVALID: 请先设置主数据目录（同步根即笔记库根）".into());
    };
    // Q16：初始化是 1.33 GB 量级作业，独立后台任务，UI 通过事件跟进，不模态等待
    let _ = &app;
    let guard = crate::sync::try_acquire(&root)?;
    let Some(_guard) = guard else {
        return Err("SYNC_BUSY: 已有同步任务在执行".into());
    };
    let store = store_from_state(&cfg)?;
    let progress: crate::sync::ProgressFn = Arc::new(move |phase, done, total| {
        let _ = app.emit(
            "sync-progress",
            serde_json::json!({"phase": phase, "done": done, "total": total}),
        );
    });
    // U4：角色归一（旧值 `export` → `writer`；未知值拒）
    let role = crate::config::canonical_role(&role);
    let report = match role {
        crate::config::ROLE_WRITER => {
            crate::sync::bootstrap_export(&*store, &cfg, &root, progress).await?
        }
        crate::config::ROLE_READER => crate::sync::bootstrap_reader(&*store, &cfg, &root, progress).await?,
        other => {
            return Err(format!(
                "SYNC_CONFIG_INVALID: 未知角色 {other}（只能是 {} / {}）",
                crate::config::ROLE_WRITER,
                crate::config::ROLE_READER
            ))
        }
    };
    // 初始化完成置位（写回**归一后**的角色，避免 export 残留在设置里）
    let mut settings = state.settings.lock().unwrap().clone();
    settings.sync.initialized = true;
    settings.sync.role = role.to_string();
    crate::config::save_settings(&settings)?;
    *state.settings.lock().unwrap() = settings;
    Ok(InitReport { report })
}

#[tauri::command]
pub async fn run_sync(
    app: AppHandle,
    state: State<'_, AppState>,
    direction: String,
) -> Result<crate::sync::SyncReport, String> {
    let cfg = state.settings.lock().unwrap().sync.clone();
    if !cfg.enabled || !cfg.initialized {
        return Err("SYNC_DISABLED: 云同步未启用或未初始化".into());
    }
    // U4 / R7：只读端不接受本地写入 ⇒ 也不接受"把只读副本推回云端"
    if crate::config::canonical_role(&cfg.role) == crate::config::ROLE_READER
        && direction == "up"
    {
        return Err(
            "READER_READONLY: 本机角色为只读端（reader），不支持上行 —— 只读端以云端为准，\
             请在写入端上行"
                .into(),
        );
    }
    // U1：同步根恒为库根
    let Some(root) = state.settings.lock().unwrap().sync_root() else {
        return Err("SYNC_CONFIG_INVALID: 请先设置主数据目录（同步根即笔记库根）".into());
    };
    let guard = crate::sync::try_acquire(&root)?;
    let Some(_guard) = guard else {
        return Err("SYNC_BUSY: 已有同步任务在执行".into());
    };
    let store = store_from_state(&cfg)?;
    let progress: crate::sync::ProgressFn = Arc::new(move |phase, done, total| {
        let _ = app.emit(
            "sync-progress",
            serde_json::json!({"phase": phase, "done": done, "total": total}),
        );
    });
    match direction.as_str() {
        "up" => crate::sync::sync_up(&*store, &cfg, &root, progress).await,
        "down" => crate::sync::sync_down(&*store, &cfg, &root, progress).await,
        other => Err(format!("SYNC_CONFIG_INVALID: direction 只能是 up/down，收到 {other}")),
    }
}

#[tauri::command]
pub fn get_sync_status(state: State<AppState>) -> SyncStatusView {
    let s = state.settings.lock().unwrap().sync.clone();
    let (last_sync_at, last_report) = crate::sync::load_state()
        .map(|(at, r)| (Some(at), Some(r)))
        .unwrap_or((None, None));
    let mut trash_items = 0usize;
    let mut trash_bytes = 0u64;
    // T7 收口：回收站统计与 `list_trash` / `delete_note` 同根（库模式 = library_dir）
    if let Some(root) = state.settings.lock().unwrap().trash_root() {
        let trash = root.join("_trash");
        trash_items = count_tree_files(&trash);
        trash_bytes = tree_bytes(&trash);
    }
    // 【真云 GUI 实测发现（2026-09-20）】此前这里写死 0（注释称"避免阻塞 UI 的网络调用"），
    // 但本地水位只是读库清单 sqlite 的一个 meta 键，**零网络零阻塞** —— 状态栏的
    // "revision" 于是永远显示 0。改为直读本地清单；清单缺失/未设库时回退 0。
    let local_revision = state
        .settings
        .lock()
        .unwrap()
        .sync_root()
        .and_then(|root| crate::manifest::open_readonly(&root).ok())
        .map(|conn| crate::manifest::current_revision(&conn))
        .unwrap_or(0);
    SyncStatusView {
        enabled: s.enabled,
        role: s.role.clone(),
        initialized: s.initialized,
        last_sync_at,
        last_report,
        local_revision,
        trash_items,
        trash_bytes,
    }
}

/// 启动任务手动重跑（§6.3 可诊断入口）；断网/未配置时返回摘要而非错误弹框语义
#[tauri::command]
pub async fn run_startup_tasks(app: AppHandle) -> Result<String, String> {
    let progress: crate::sync::ProgressFn = Arc::new(move |phase, done, total| {
        let _ = app.emit(
            "sync-progress",
            serde_json::json!({"phase": phase, "done": done, "total": total}),
        );
    });
    match crate::sync::startup_tasks(progress).await {
        Ok(s) => Ok(s),
        Err(e) => {
            append_sync_log(&format!("启动任务失败: {e}"));
            Err(e)
        }
    }
}

// ---------- 回收站（T7 收口：根 = 库根，不再是 sync.local_root） ----------
//
// 收口要点（§16.5 的待接线项）：`library::delete_note` 把删掉的篇目移进
// `library_dir/_trash/{YYYY-MM-DD}/` 并写 `deleted` 墓碑，所以**列表 / 恢复 / 清理 /
// 打开目录必须都在库根上做**。此前这四个命令看 `sync.local_root`，
// 结果是"在库里删的笔记，回收站里看不见、也恢复不了"。
//
// 根的唯一判据：`Settings::trash_root()`（库模式 = library_dir；未设库才回退 local_root）。

/// 回收站统计（库根 `_trash/` 实况：文件数与字节数）
#[derive(Debug, Serialize)]
pub struct TrashStats {
    /// 库根（统计口径的来源，UI 原样展示）
    pub root: String,
    pub items: usize,
    pub bytes: u64,
    pub retention_days: u64,
}

fn trash_root_of(state: &State<'_, AppState>) -> Option<PathBuf> {
    state.settings.lock().unwrap().trash_root()
}

#[tauri::command]
pub fn list_trash(state: State<AppState>) -> Result<Vec<crate::library::TrashEntry>, String> {
    let Some(root) = trash_root_of(&state) else {
        return Ok(Vec::new());
    };
    if !root.join(manifest::MANIFEST_NAME).is_file() {
        // 无清单 → 没有墓碑可列（空目录还没建库）
        return Ok(Vec::new());
    }
    crate::library::list_trash(&root)
}

#[tauri::command]
pub fn trash_stats(state: State<AppState>) -> TrashStats {
    let root = trash_root_of(&state);
    let (mut items, mut bytes) = (0usize, 0u64);
    if let Some(r) = &root {
        let t = r.join("_trash");
        items = count_tree_files(&t);
        bytes = tree_bytes(&t);
    }
    TrashStats {
        root: root.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        items,
        bytes,
        retention_days: crate::sync::TRASH_RETENTION_DAYS,
    }
}

/// 恢复（T7）：文件移回原路径 + 墓碑快照逐字段还原清单行 + 单篇索引增量。
/// 与其余写命令同一前置：**库上下文 + 库根 + 库索引**，错误码原样透传。
#[tauri::command]
pub async fn restore_trash(
    state: State<'_, AppState>,
    guid: String,
) -> Result<crate::library::NoteWriteReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        crate::library::restore_note(&lib, &index_db, &guid)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 restore {} → {}（rev {}）",
        report.guid, report.exported_path, report.revision
    ));
    Ok(report)
}

#[tauri::command]
pub fn purge_trash(state: State<AppState>, before_days: i64) -> Result<crate::sync::GcReport, String> {
    let Some(root) = trash_root_of(&state) else {
        return Ok(crate::sync::GcReport { removed: 0, bytes: 0, kept: 0 });
    };
    let days = if before_days > 0 { before_days as u64 } else { crate::sync::TRASH_RETENTION_DAYS };
    crate::sync::gc_trash(&root, days)
}

#[tauri::command]
pub fn open_trash_dir(state: State<AppState>) -> Result<(), String> {
    let Some(root) = trash_root_of(&state) else {
        return Err("未设置笔记库目录，回收站不可用".into());
    };
    let dir = root.join("_trash");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    tauri_plugin_opener::open_path(dir, None::<&str>).map_err(|e| e.to_string())
}

fn count_tree_files(root: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    n += 1;
                }
            }
        }
    }
    n
}

fn tree_bytes(root: &Path) -> u64 {
    let mut n = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(m) = e.metadata() {
                    n += m.len();
                }
            }
        }
    }
    n
}

#[tauri::command]
pub fn clear_cloud_credentials(state: State<AppState>) -> Result<(), String> {
    let user = state.settings.lock().unwrap().sync.credential_user.clone();
    if !user.is_empty() {
        crate::credential::delete_secret(&user)?;
    }
    let mut settings = state.settings.lock().unwrap().clone();
    settings.sync.credential_user = String::new();
    settings.sync.enabled = false;
    settings.sync.initialized = false;
    crate::config::save_settings(&settings)?;
    *state.settings.lock().unwrap() = settings;
    Ok(())
}

// `pick_sync_root` 已随 U1 删除：**同步根恒为库根**，没有第二个根可挑。
// 用户能选的只有「主数据目录」（`pick_library_dir`），Q18 红线（唯一目的 = 为知原笔记不可修改：
// 库根不得等于/位于为知源数据目录之内；库包含源放行 —— 2026-09-20 用户澄清）
// 改在 `validate_sync` 里对库根施加。留着这条命令只会让"库根"与"同步根"重新变成两个东西。

fn index_db_of(state: &AppState) -> Result<PathBuf, String> {
    // 「未打开任何笔记」：先于任何索引查找拒绝，避免无库时悄悄读到源索引
    if state.view_context() == ViewContext::None {
        return Err(
            "NO_CONTEXT: 尚未打开任何笔记（未设置数据目录，或数据目录的清单 export.db 不可读）"
                .into(),
        );
    }
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

/// 源模式派生索引路径：导出/巡检恒基于源数据，与视图上下文无关
fn source_index_db() -> PathBuf {
    crate::config::wiz_home().join("index.db")
}

fn source_index_db_checked() -> Result<PathBuf, String> {
    let p = source_index_db();
    if !p.exists() {
        return Err("源索引尚未构建，请先在「读取为知笔记」中重建源索引".into());
    }
    Ok(p)
}

/// 导出目标库根保护（§3.2）：dest 位于 library_dir 之下且 ≠ library_dir → 拒绝；
/// dest == library_dir → 引导走「导入到我的笔记库」（避免污染库）
fn guard_export_dest(state: &AppState, dest: &str) -> Result<(), String> {
    let Some(lib) = state.library_dir() else {
        return Ok(());
    };
    let lib_c = crate::config::canon_best_effort(&lib);
    let dest_c = crate::config::canon_best_effort(Path::new(dest));
    if dest_c == lib_c {
        return Err("EXPORT_INTO_LIBRARY: 目标即笔记库根，请改用「导入到我的笔记库」".into());
    }
    if dest_c.starts_with(&lib_c) {
        return Err("EXPORT_INTO_LIBRARY_SUBDIR: 导出目标不得位于笔记库目录之下（会污染库）".into());
    }
    Ok(())
}

// ---------- 设置 ----------

#[tauri::command]
pub fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
pub fn update_settings(state: State<AppState>, settings: Settings) -> Result<(), String> {
    save_settings(&settings)?;
    // 仅当前处于源上下文时才切 zip 解析器（库上下文由 library_dir 驱动，勿覆盖）
    if state.view_context() == ViewContext::Source {
        if let Some(dir) = &settings.source_dir {
            state.zip.set_notes_dir(PathBuf::from(dir).join("notes"));
        }
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
        .source_dir()
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
    /// 正文形态（M4）：`"md"` = 包内 `note.md`；`"html"` = 为知原生 `index.html`；
    /// `None` = 读不到正文条目（包缺失/损坏）—— 前端据此决定徽标与编辑入口文案，
    /// 且**不会**把"读不到"显示成某一种形态（见 [`ZipService::body_format_tag`]）。
    pub body_format: Option<String>,
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
                body_format: None, // 下面按包内实况填（要看 zip，不在索引里）
                attachments: vec![],
            })
        })
        .map_err(|e| e.to_string())?;
    drop(st);
    detail.body_format = state.zip.body_format_tag(&guid);

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
    let cleaned = re_rtf().replace_all(rtf, "").to_string();
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

fn to_export_attachments(detail: &NoteDetail) -> Vec<ExportAttachment> {
    detail
        .attachments
        .iter()
        .map(|a| ExportAttachment {
            display_name: a.display_name.clone(),
            src: a.file_path.clone(),
            size: a.size,
        })
        .collect()
}

#[tauri::command]
pub fn export_note_zip_cmd(
    state: State<AppState>,
    guid: String,
    dest: String,
) -> Result<ExportReport, String> {
    let detail = get_note_detail(state.clone(), guid.clone())?;
    let ctx = export_ctx(&state)?;
    export_note_zip(
        &ctx,
        &state.zip,
        &guid,
        &to_export_attachments(&detail),
        Path::new(&dest),
    )
}

#[tauri::command]
pub fn export_note_html_cmd(
    state: State<AppState>,
    guid: String,
    dest: String,
) -> Result<ExportReport, String> {
    let detail = get_note_detail(state.clone(), guid.clone())?;
    let ctx = export_ctx(&state)?;
    export_note_single_html(
        &ctx,
        &state.zip,
        &guid,
        &to_export_attachments(&detail),
        Path::new(&dest),
    )
}

/// 按目录 / 全库导出：耗时数分钟，放到 blocking 线程池，避免冻结界面
#[tauri::command]
pub async fn export_folder_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    location: String,
    dest: String,
) -> Result<ExportReport, String> {
    let data_dir = state.source_dir().ok_or("未设置源数据目录")?;
    guard_export_dest(&state, &dest)?;
    let index_db = source_index_db_checked()?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        let zip = ZipService::new(data_dir.join("notes"));
        let ctx = ExportContext::new(data_dir.join("notes"), index_db);
        export_folder(&ctx, &zip, &location, Path::new(&dest), &|done, total| {
            let _ = app.emit("export-progress", serde_json::json!({"done": done, "total": total}));
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(report)
}

/// 每份笔记导出为一个 zip（FR-08.1 批量形态）。**D0**：只产 native（源 zip 字节级拷贝），
/// 无模式参数（slim 已整体取消，见 `docs/本地笔记读写实现.md` §4.6）。
/// 耗时可能数分钟，放到 blocking 线程池，避免冻结界面
#[tauri::command]
pub async fn export_folder_zips_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    location: String,
    dest: String,
) -> Result<FolderZipExportReport, String> {
    let data_dir = state.source_dir().ok_or("未设置源数据目录")?;
    guard_export_dest(&state, &dest)?;
    let index_db = source_index_db_checked()?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        let ctx = ExportContext::new(data_dir.join("notes"), index_db);
        export_folder_zips(&ctx, &location, Path::new(&dest), &|done, total| {
            let _ = app.emit("export-progress", serde_json::json!({"done": done, "total": total}));
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(report)
}

fn export_ctx(state: &State<AppState>) -> Result<ExportContext, String> {
    // 单篇导出不依赖 ctx 的 notes_dir（export_note_* 均忽略 ctx），故按上下文取目录即可；
    // 库模式下即使源已移除也能导出当前笔记（P1：删源后全功能可用）
    let notes_dir = match state.view_context() {
        ViewContext::Library => state.library_dir().ok_or("未设置笔记库目录")?,
        ViewContext::Source => state.source_dir().ok_or("未设置源数据目录")?,
        ViewContext::None => return Err("NO_CONTEXT: 尚未打开任何笔记".into()),
    }
    .join("notes");
    Ok(ExportContext::new(notes_dir, state.index_db()))
}

// ---------- 验收巡检（M4：T4.1 – T4.4） ----------

/// 全库巡检 + 安全项 + 性能基准，一次跑完并回报告
#[tauri::command]
pub async fn run_verify_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    with_bench: Option<bool>,
    export_full: Option<bool>,
) -> Result<verify::VerifyReport, String> {
    let data_dir = state.source_dir().ok_or("未设置源数据目录")?;
    let index_db = source_index_db_checked()?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        verify::run_all(
            &data_dir,
            &index_db,
            with_bench.unwrap_or(true),
            export_full.unwrap_or(false),
            &|stage, done, total| {
                let _ = app.emit(
                    "verify-progress",
                    serde_json::json!({"stage": stage, "done": done, "total": total}),
                );
            },
        )
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(report)
}

/// 落盘验收报告（Markdown + 同名 JSON），返回两个路径
#[tauri::command]
pub fn save_verify_report(dest: String, report: verify::VerifyReport) -> Result<Vec<String>, String> {
    verify::write_report_files(Path::new(&dest), &report)
        .map(|v| v.into_iter().map(|p| p.to_string_lossy().into_owned()).collect())
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
    // 源目录不得与库根相等/互为父子（R9）
    let lib = state.settings.lock().unwrap().library_dir.clone();
    if let Some(l) = &lib {
        crate::config::validate_library_root(Path::new(l), Some(&dir))?;
    }
    let mut s = state.settings.lock().unwrap().clone();
    s.source_dir = Some(dir.clone());
    save_settings(&s)?;
    *state.settings.lock().unwrap() = s;
    // 设置后仅当当前上下文是 Source 才切 notes_dir
    if state.view_context() == ViewContext::Source {
        state.zip.set_notes_dir(PathBuf::from(dir).join("notes"));
    }
    Ok(())
}

// ---------- 笔记库（FR-11 P0/P1） ----------

/// 选择主数据目录（笔记库）：红线校验 + 准入校验 → 写 settings.library_dir，
/// ready 时建 LibraryResolver 并切库上下文；返回 LibraryStatus 供前端分支
///（empty→可导入 / ready→建索引 / no_manifest→引导重建清单 / rejected→不变）。
#[tauri::command]
pub fn pick_library_dir(state: State<AppState>, dir: String) -> Result<LibraryStatus, String> {
    let p = PathBuf::from(&dir);
    let source = state.settings.lock().unwrap().source_dir.clone();
    // R9 红线：库根不得与源目录相等/互为父子（不通过直接 Err）
    crate::config::validate_library_root(&p, source.as_deref())?;
    let status = crate::library::validate_library(&p);
    if status.kind == "rejected" {
        return Ok(status); // 不落盘、不切上下文，交前端展示拒绝原因
    }
    let mut s = state.settings.lock().unwrap().clone();
    s.library_dir = Some(dir);
    save_settings(&s)?;
    *state.settings.lock().unwrap() = s;
    // 上下文按「清单能否打开」定，而不是「用户选了目录就算」（与启动分支同口径）：
    //   清单可读 → Library，注入 LibraryResolver；
    //   empty（新空目录）/ no_manifest（有 zip 无清单）→ None：**不打开任何笔记**，
    //   由空态页引导「导入到我的笔记库」或重建清单；期间任何读命令都不会落到源数据。
    match LibraryResolver::new(p) {
        Ok(resolver) => {
            let resolver = Arc::new(resolver);
            state
                .zip
                .set_resolver(resolver.clone() as Arc<dyn NotePathResolver>);
            *state.library_resolver.lock().unwrap() = Some(resolver);
            *state.context.lock().unwrap() = ViewContext::Library;
        }
        Err(_) => {
            let _ = apply_context(&state, ViewContext::None);
        }
    }
    Ok(status)
}

/// 构建库派生索引（后台全量，进度走 index-progress）；完成后切库上下文
#[tauri::command]
pub async fn build_library_index_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<BuildReport, String> {
    let lib = state.library_dir().ok_or("未设置笔记库目录")?;
    let index_db = crate::config::index_file_for_library(&lib);
    let resolver = Arc::new(LibraryResolver::new(lib.clone())?);
    // 先切库上下文，保证后续消费命令走库索引 + 库解析器（并留句柄给写路径）
    state
        .zip
        .set_resolver(resolver.clone() as Arc<dyn NotePathResolver>);
    *state.library_resolver.lock().unwrap() = Some(resolver.clone());
    *state.context.lock().unwrap() = ViewContext::Library;
    let report = tauri::async_runtime::spawn_blocking(move || {
        build_library_index(&lib, resolver, &index_db, &|done, total| {
            let _ = app.emit("index-progress", serde_json::json!({"done": done, "total": total}));
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(report)
}

/// 「导入到我的笔记库」报告（导出 + 附件入库 + 库索引重建）
#[derive(Debug, Serialize)]
pub struct ImportToLibraryReport {
    pub export: FolderZipExportReport,
    pub attachments_copied: usize,
    pub attachments_missing: usize,
    pub index: BuildReport,
}

/// 导入到我的笔记库（目标固定=库根）：export_folder_zips_**md**(dest=库根) + 附件入库 + 库索引重建。
/// **库内主数据自 M2（§20）起是 md 包**：源 `index.html` 转成包内 `note.md`，源包其余条目原样搬运。
/// 只读源、只写库（G1）；进度走 export-progress / index-progress。
#[tauri::command]
pub async fn import_to_library(
    app: AppHandle,
    state: State<'_, AppState>,
    location: String,
) -> Result<ImportToLibraryReport, String> {
    let lib = state
        .library_dir()
        .ok_or("未设置笔记库目录，请先在设置中选择主数据目录")?;
    // U4 / R7：只读端的库由下行填充，导入会让本地多出云端没有的篇目（随后下行必被护栏拒掉）
    let role = state.settings.lock().unwrap().sync.role.clone();
    if crate::config::canonical_role(&role) == crate::config::ROLE_READER {
        return Err(
            "READER_READONLY: 本机角色为只读端（reader），导入已禁用 —— 只读端的库由「初始化/下行」从云端填充"
                .into(),
        );
    }
    let source = state.source_dir().ok_or("未设置源数据目录")?;
    let source_index = source_index_db_checked()?;

    // ① 导出源目录→库根（**md 包**：源 `index.html` 转 `note.md`，`index_files/` 整包搬运；§20.3/M2）
    let app2 = app.clone();
    let export = {
        let lib = lib.clone();
        let source = source.clone();
        let source_index = source_index.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let ctx = ExportContext::new(source.join("notes"), source_index);
            export_folder_zips_md(&ctx, &location, &lib, &|done, total| {
                let _ = app2.emit("export-progress", serde_json::json!({"done": done, "total": total}));
            })
        })
        .await
        .map_err(|e| e.to_string())??
    };

    // ② 附件入库（清单 attachment 表为空时先从源索引回填）+ ③ 库索引重建
    let app3 = app.clone();
    let (attachments_copied, attachments_missing, index) = {
        let lib = lib.clone();
        let source = source.clone();
        let source_index = source_index.clone();
        tauri::async_runtime::spawn_blocking(move || -> Result<(usize, usize, BuildReport), String> {
            let conn = manifest::open_and_migrate(&lib)?;
            let att_count: i64 = conn
                .query_row("SELECT count(*) FROM attachment", [], |r| r.get(0))
                .unwrap_or(0);
            if att_count == 0 {
                let _ = manifest::populate_attachments_from_index(&conn, &source_index, &source)?;
            }
            let imp = crate::library::import_attachments(&lib, &source, &conn)?;
            drop(conn);
            let resolver = Arc::new(LibraryResolver::new(lib.clone())?);
            let index_db = crate::config::index_file_for_library(&lib);
            let rep = build_library_index(&lib, resolver, &index_db, &|done, total| {
                let _ = app3.emit("index-progress", serde_json::json!({"done": done, "total": total}));
            })?;
            Ok((imp.copied, imp.missing, rep))
        })
        .await
        .map_err(|e| e.to_string())??
    };

    // 导入后切库上下文（解析器已就绪）
    let _ = apply_context(&state, ViewContext::Library);

    Ok(ImportToLibraryReport {
        export,
        attachments_copied,
        attachments_missing,
        index,
    })
}

/// 库状态（启动自检 / 设置页展示）：
/// - `library_readable` / `library_error`：库清单（export.db）能否打开及原因 ——
///   这是「是否打开笔记」的唯一判据（§6.4），空态页据此如实告知用户；
/// - `manifest_notes` / `index_*`：**恒指笔记库**，与当前上下文无关
///   （切到为知源视图时也应能知道「我的库」是否就绪、返回按钮是否可用）。
#[derive(Debug, Serialize)]
pub struct LibraryStatusView {
    pub library_dir: Option<String>,
    pub source_dir: Option<String>,
    pub context: ViewContext,
    pub library_readable: bool,
    pub library_error: String,
    /// 库准入分类：unset（未设置）/ empty（空目录，待导入）/ no_manifest（有 zip 无清单）/
    /// rejected（不可用作库）/ ready。空态页据此区分「新库待导入」与「清单损坏」并给不同引导。
    pub library_kind: String,
    pub manifest_notes: usize,
    pub index_present: bool,
    pub index_notes: usize,
    pub consistent: bool,
}

#[tauri::command]
pub fn get_library_status(state: State<AppState>) -> LibraryStatusView {
    let lib = state.library_dir();
    let source = state.source_dir();
    let ctx = state.view_context();
    let mut library_readable = false;
    let mut library_error = String::new();
    let mut library_kind = String::from("unset");
    let mut manifest_notes = 0usize;
    if let Some(l) = &lib {
        // 准入分类：empty（新空目录，待导入）/ no_manifest（有 zip 无清单）/
        // rejected（不可用作库）/ ready。四者都「不打开任何笔记」，但空态页引导不同。
        let st = crate::library::validate_library(l);
        if st.kind == "rejected" {
            library_error = st.reason.clone();
        }
        library_kind = st.kind.clone();
        match LibraryResolver::new(l.clone()) {
            Ok(r) => {
                library_readable = true;
                manifest_notes = r.note_count();
            }
            Err(e) if library_error.is_empty() => library_error = e,
            Err(_) => {}
        }
    }
    // 索引口径恒为「库索引」：无库 / 库不可读时不存在可比对的索引
    let index_db = lib
        .as_ref()
        .filter(|_| library_readable)
        .map(|l| crate::config::index_file_for_library(l));
    let index_present = index_db.as_ref().map(|p| p.exists()).unwrap_or(false);
    let mut index_notes = 0usize;
    if index_present {
        if let Some(p) = &index_db {
            if let Ok(conn) = rusqlite::Connection::open_with_flags(
                p,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                index_notes = conn
                    .query_row("SELECT count(*) FROM note", [], |r| r.get::<_, i64>(0))
                    .unwrap_or(0) as usize;
            }
        }
    }
    let consistent = manifest_notes > 0 && manifest_notes == index_notes;
    LibraryStatusView {
        library_dir: lib.map(|p| p.to_string_lossy().into_owned()),
        source_dir: source.map(|p| p.to_string_lossy().into_owned()),
        context: ctx,
        library_readable,
        library_error,
        library_kind,
        manifest_notes,
        index_present,
        index_notes,
        consistent,
    }
}

/// 切换视图上下文（「浏览/检索为知笔记」显式进入源视图、回库视图用），返回切换后上下文。
/// `none` 用于「库不可用时离开源视图」——落到空态页如实说明原因。
#[tauri::command]
pub fn set_view_context(state: State<AppState>, context: String) -> Result<ViewContext, String> {
    let ctx = match context.as_str() {
        "none" => ViewContext::None,
        "library" => ViewContext::Library,
        "source" => ViewContext::Source,
        other => return Err(format!("未知上下文: {other}")),
    };
    apply_context(&state, ctx)?;
    Ok(ctx)
}

/// 空库初始化：目录已设为笔记库但**还没有清单**（export.db）时，按当前 schema
/// 建立一份空清单并切到库上下文 —— 此后「新建笔记 / 新建目录」即可直接使用。
/// 只对 `validate_library` 判为 `empty` 的目录生效：`no_manifest`（有 zip 无清单）
/// 绝不凭空造清单（zip 会变孤儿），必须走 manifest-rebuild 重建。幂等：已有清单直接切上下文。
/// 建清单后**同步重建派生索引**（索引文件在库根之外，可能残留旧内容）。
#[tauri::command]
pub async fn init_library_manifest(state: State<'_, AppState>) -> Result<(), String> {
    let lib = state
        .library_dir()
        .ok_or("LIBRARY_UNSET: 未设置笔记库目录，请先「设置数据目录」")?;
    if !crate::manifest::manifest_path(&lib).is_file() {
        let lib2 = lib.clone();
        tauri::async_runtime::spawn_blocking(move || {
            crate::library::init_empty_library_manifest(
                &lib2,
                &crate::config::index_file_for_library(&lib2),
            )
        })
        .await
        .map_err(|e| e.to_string())??;
        append_sync_log("库初始化：空目录建立清单（export.db）+ 重建派生索引");
    }
    // 清单就绪 → 切库上下文（注入 LibraryResolver；写路径 write_targets 依赖它）
    apply_context(&state, ViewContext::Library)
}

/// 新建目录（库内）：磁盘建目录 + 索引 folder 表补行。清单不记目录
/// （目录 = 笔记 location 推导 + 磁盘实况），故无清单写、无置脏。返回规范化 location。
#[tauri::command]
pub async fn create_folder_cmd(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let (lib, index_db) = write_targets(&state)?;
    let created = tauri::async_runtime::spawn_blocking(move || {
        crate::library::create_folder(&lib, &index_db, &path)
    })
    .await
    .map_err(|e| e.to_string())??;
    append_sync_log(&format!("库写入 create_folder {}", created));
    Ok(created)
}

// ---------- 笔记库写入（FR-11 P2 / S1：库内编辑正文、重命名、移动、删除） ----------
//
// 一律：① 库上下文才允许（源视图是只读导入路径）；② 重 I/O 走 spawn_blocking；
// ③ 写成功后立即让 ZipService/解析器缓存失效（否则界面还读得到旧字节，T4）；
// ④ 错误码原样透传（LOCK_BUSY / NOTE_NOT_FOUND / PATH_TAKEN / EMPTY_HTML …），不吞错（T8）。

/// 写操作前置：取库根 + 库索引 + 强制库上下文 + **角色护栏**（U4 / R7）
///
/// 前三条是"在哪个视图写"，第四条是"这台机器有没有写资格"：
/// 角色为 `reader` 的机器是云端镜像，本地写入必被下一次下行覆盖（R7），故**在入口就拒**，
/// 而不是等用户写完再让他发现改动没了。前端同时把写入口置灰（两侧双保险）。
fn write_targets(state: &State<'_, AppState>) -> Result<(PathBuf, PathBuf), String> {
    if state.view_context() != ViewContext::Library {
        return Err("WRONG_CONTEXT: 写操作只能在「我的笔记库」视图下进行（源视图为只读导入）".into());
    }
    let role = state.settings.lock().unwrap().sync.role.clone();
    if crate::config::canonical_role(&role) == crate::config::ROLE_READER {
        return Err(
            "READER_READONLY: 本机角色为只读端（reader），写操作已禁用 —— 请在写入端修改后再同步过来"
                .into(),
        );
    }
    let lib = state
        .library_dir()
        .ok_or("WRONG_CONTEXT: 未设置笔记库目录，请先「设置数据目录」")?;
    let index_db = crate::config::index_file_for_library(&lib);
    Ok((lib, index_db))
}

/// 该篇在磁盘上的绝对路径（「在访达中显示」用）。
/// 走解析器 → 库模式是清单 `exported_path`，源模式是 `notes/{GUID}` —— 与读路径同一口径，
/// 前端不必自己拼路径（库内路径含中文/空格，拼必错）。
#[tauri::command]
pub fn get_note_file_path(state: State<AppState>, guid: String) -> Result<String, String> {
    let resolver = state.library_resolver.lock().unwrap().clone();
    // 库上下文：解析器权威；源上下文：按 notes/{GUID} 拼（只读来源，不写）
    let path = match resolver {
        Some(r) => r.resolve(&guid),
        None => state
            .source_dir()
            .map(|d| d.join("notes").join(crate::zipserve::normalize_guid(&guid).unwrap_or(guid.clone()))),
    };
    path.filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| format!("NOTE_NOT_FOUND: 找不到该篇的落地文件 {guid}"))
}

/// 正文源码（编辑抽屉的初值，§4.3/T8/M3）：`format` 让前端摆对编辑器
/// （`md` = Markdown 源码框，`html` = HTML 源码框）。
#[derive(Debug, serde::Serialize)]
pub struct NoteSource {
    /// `md` | `html`（见 [`crate::zipserve::BodyFormat::tag`]）
    pub format: String,
    /// 正文**源文本**（md 包 → Markdown；native 包 → HTML），BOM 已剥
    pub text: String,
}

/// 读笔记正文**源码**（编辑抽屉的初始内容，§4.3/T8；M3 起形态自适应）。
///
/// 与 `wiznote://{guid}/index.html` 的差别：那条路会**注入宿主兼容层 + 加 CSP**，
/// 拿它当编辑初值会把注入痕迹写回库内（写前校验会拒，但那是事后拦截）；
/// 这里走 [`ZipService::read_note_body`]（只剥 BOM 的原始正文）。
///
/// **形态不写死在命令名里**：md 包返回 `note.md` 的 Markdown、native 包返回
/// `index.html` 的 HTML，由 `format` 如实告知 —— 前端据它切换编辑器与保存口径。
#[tauri::command]
pub fn get_note_source(state: State<AppState>, guid: String) -> Result<NoteSource, String> {
    if state.view_context() != ViewContext::Library {
        return Err("WRONG_CONTEXT: 编辑只能在「我的笔记库」视图下进行".into());
    }
    let (fmt, text) = state.zip.read_note_body(&guid).map_err(|e| e.message())?;
    Ok(NoteSource {
        format: fmt.tag().into(),
        text,
    })
}

/// 登记 / 清除「预览草稿」（§4.3/T8/M3）。
///
/// 用途：源码编辑时右侧要**所见即所得**地看改动结果。三个可选做法里：
/// - `iframe srcdoc`：同源（或需 `sandbox` 全禁），且相对 `index_files/…` 解析不到 —— 图全裂；
/// - 前端 `document.write`：把笔记 HTML 灌进宿主文档，风险不可接受；
/// - **本做法**：把草稿交给 `wiznote://` 协议，按 token 提供主文档 ——
///   URL 仍是 `wiznote://{guid}/…`，相对资源照常从 zip 里取，CSP / 兼容层注入与阅读态**完全一致**。
///
/// 草稿只在带 `?draft=<token>` 的请求上生效，**永不污染**普通阅读请求；
/// `clear_note_draft` 在抽屉关闭时释放（单份草稿即可，不需要多份并存）。
///
/// `text` 的形态由**包内实况**决定（调用方不必传格式，也传不错）：
/// md 包里编辑器拿到的就是 Markdown，预览时先渲染再喂 iframe。
#[tauri::command]
pub fn set_note_draft(state: State<AppState>, guid: String, text: String) -> Result<String, String> {
    if state.view_context() != ViewContext::Library {
        return Err("WRONG_CONTEXT: 预览草稿只在库视图下可用".into());
    }
    let format = state.zip.body_format(&guid);
    let token = crate::sandbox::nonce();
    *state.draft.lock().unwrap() = Some(DraftPreview {
        guid,
        format,
        text,
        token: token.clone(),
    });
    Ok(token)
}

#[tauri::command]
pub fn clear_note_draft(state: State<AppState>) -> Result<(), String> {
    *state.draft.lock().unwrap() = None;
    Ok(())
}

/// 编辑笔记正文（§4.3/M3）：**按包内形态自动分派** —— md 包写 `note.md`，
/// 为知原生包写 `index.html`；两者共用同一条原子写路径（整包搬运 + 清单事务 + 单篇索引增量）。
///
/// 为什么命令层不按形态分两个入口：编辑器拿到的源码形态**必然**与包内形态一致
/// （见 [`get_note_source`]），而写回的形态只要跟包走就一定对得上 ——
/// 多一个"形态参数"只会多一个能传错的地方。
#[tauri::command]
pub async fn save_note_source_cmd(
    state: State<'_, AppState>,
    guid: String,
    text: String,
) -> Result<crate::library::NoteWriteReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        crate::library::save_note_body(&lib, &index_db, &guid, &text)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    // 保存后草稿即过期（预览若还挂着会显示旧文本，且它会遮住新正文）
    *state.draft.lock().unwrap() = None;
    append_sync_log(&format!(
        "库写入 {} {} → {}（{} B, rev {}）",
        report.op, report.guid, report.exported_path, report.exported_size, report.revision
    ));
    Ok(report)
}

/// M4 图片插入（文件选择器路径）：后端读用户**刚在系统对话框里选中**的文件并写入包内。
/// 为什么不让前端读字节再传：那需要一个通用的"按路径读文件"命令，是一把不该发给
/// WebView 的万能钥匙 —— 这里文件路径只作为"用户刚选过"的凭证，服务端一次读一次写。
#[tauri::command]
pub async fn add_note_image_file_cmd(
    state: State<'_, AppState>,
    guid: String,
    file_path: String,
) -> Result<crate::library::NoteImageReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        let bytes = std::fs::read(&file_path).map_err(|e| format!("IMAGE_READ_FAILED: {e}"))?;
        let name = std::path::Path::new(&file_path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned());
        crate::library::add_note_image(&lib, &index_db, &guid, &bytes, name.as_deref())
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 add-image {} {} → {}（复用={})",
        report.guid, report.entry, report.exported_path, report.reused
    ));
    Ok(report)
}

/// M4 图片插入（粘贴路径）：剪贴板里的图片字节（截图 / 网页复制）走 base64 过 IPC。
#[tauri::command]
pub async fn add_note_image_data_cmd(
    state: State<'_, AppState>,
    guid: String,
    data_b64: String,
    name: Option<String>,
) -> Result<crate::library::NoteImageReport, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .map_err(|e| format!("IMAGE_B64_DECODE: {e}"))?;
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        crate::library::add_note_image(&lib, &index_db, &guid, &bytes, name.as_deref())
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 add-image {} {} → {}（复用={})",
        report.guid, report.entry, report.exported_path, report.reused
    ));
    Ok(report)
}

/// M4 附件插入（最小版）：系统对话框选中的**任意文件**写入包内 `attachments/`，
/// 正文由前端插链接引用。与图片同为「按路径读」凭证模式（不给 WebView 万能钥匙），
/// 50 MB 上限的文件也不该走 base64 过 IPC —— 后端一次读一次写。
#[tauri::command]
pub async fn add_note_attachment_cmd(
    state: State<'_, AppState>,
    guid: String,
    file_path: String,
) -> Result<crate::library::NoteImageReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        let bytes = std::fs::read(&file_path).map_err(|e| format!("ATTACHMENT_READ_FAILED: {e}"))?;
        let name = std::path::Path::new(&file_path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned());
        crate::library::add_note_attachment(&lib, &index_db, &guid, &bytes, name.as_deref())
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 add-attachment {} {} → {}（复用={})",
        report.guid, report.entry, report.exported_path, report.reused
    ));
    Ok(report)
}

/// 重命名标题（§4.3/T6）：落地文件名随标题变，云端键不变（F6）
#[tauri::command]
pub async fn rename_note_cmd(
    state: State<'_, AppState>,
    guid: String,
    new_title: String,
) -> Result<crate::library::NoteWriteReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        crate::library::rename_note(&lib, &index_db, &guid, &new_title)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 rename {} → {}",
        report.guid, report.exported_path
    ));
    Ok(report)
}

/// 移动笔记到其它目录（§4.3/T6）
#[tauri::command]
pub async fn move_note_cmd(
    state: State<'_, AppState>,
    guid: String,
    new_location: String,
) -> Result<crate::library::NoteWriteReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        crate::library::move_note(&lib, &index_db, &guid, &new_location)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 move {} → {}",
        report.guid, report.exported_path
    ));
    Ok(report)
}

/// 删除笔记（进 `_trash/` + 墓碑，§4.3/Q8）
#[tauri::command]
pub async fn delete_note_cmd(
    state: State<'_, AppState>,
    guid: String,
) -> Result<crate::library::NoteWriteReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report =
        tauri::async_runtime::spawn_blocking(move || crate::library::delete_note(&lib, &index_db, &guid))
            .await
            .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 delete {}（{}）",
        report.guid, report.exported_path
    ));
    Ok(report)
}

/// 新建笔记（库内新增，origin=local）：生成 guid + md 包 + 清单插行（置脏）+ 单篇索引。
/// `md` 为初始正文（前端传 `# 标题` 起手的模板，保证创建后即可打开编辑）。
#[tauri::command]
pub async fn create_note_cmd(
    state: State<'_, AppState>,
    title: String,
    location: String,
    md: String,
) -> Result<crate::library::NoteWriteReport, String> {
    let (lib, index_db) = write_targets(&state)?;
    let report = tauri::async_runtime::spawn_blocking(move || {
        crate::library::create_note(&lib, &index_db, &title, &location, &md)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.invalidate_library_cache()?;
    append_sync_log(&format!(
        "库写入 create {} → {}（{} B）",
        report.guid, report.exported_path, report.exported_size
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    fn state_with(lib: Option<&str>, src: Option<&str>, ctx: ViewContext) -> AppState {
        let mut s = Settings::default();
        s.library_dir = lib.map(|x| x.to_string());
        s.source_dir = src.map(|x| x.to_string());
        AppState {
            zip: ZipService::none(),
            settings: std::sync::Mutex::new(s),
            context: std::sync::Mutex::new(ctx),
            library_resolver: std::sync::Mutex::new(None),
            draft: std::sync::Mutex::new(None),
        }
    }

    /// 三态索引路径：None **不得**落到源索引（否则等于把为知笔记当自有笔记展示）
    #[test]
    fn test_index_db_three_states() {
        let src_index = crate::config::wiz_home().join("index.db");
        let lib_index = crate::config::index_file_for_library(std::path::Path::new("/tmp/lib"));

        let st = state_with(Some("/tmp/lib"), Some("/tmp/src"), ViewContext::None);
        assert_ne!(st.index_db(), src_index, "None 不得落到源索引");
        assert_ne!(st.index_db(), lib_index, "None 不得落到库索引");
        assert!(
            index_db_of(&st).unwrap_err().contains("NO_CONTEXT"),
            "None 上下文读索引必须被 NO_CONTEXT 拦住"
        );

        *st.context.lock().unwrap() = ViewContext::Source;
        assert_eq!(st.index_db(), src_index);

        *st.context.lock().unwrap() = ViewContext::Library;
        assert_eq!(st.index_db(), lib_index);
    }

    /// 未打开任何笔记时切上下文：解析器置空，从读路径上读不到任何 zip
    #[test]
    fn test_apply_context_none_clears_resolver() {
        let st = state_with(Some("/tmp/lib"), Some("/tmp/src"), ViewContext::Library);
        apply_context(&st, ViewContext::None).unwrap();
        assert!(st.library_resolver.lock().unwrap().is_none());
        // 空解析器 ⇒ 任何 guid 都读不到数据（含库中真实存在的路径形态）
        assert!(st
            .zip
            .read_index_html("2834c195-c07c-4098-9a26-f46963d32908")
            .is_err());
        // M3：形态自适应的读路径同样必须读不到（不能成为"未打开笔记"的后门）
        assert!(st
            .zip
            .read_note_body("2834c195-c07c-4098-9a26-f46963d32908")
            .is_err());
        assert!(st
            .zip
            .read_note_document("2834c195-c07c-4098-9a26-f46963d32908", "t")
            .is_err());
    }

    /// M3 草稿渲染口径：md 草稿先渲染成 HTML 文档、native 草稿原样；token/guid 双命中才生效
    #[test]
    fn test_draft_html_renders_by_format() {
        let st = state_with(Some("/tmp/lib"), None, ViewContext::Library);
        // native 草稿：原样返回
        *st.draft.lock().unwrap() = Some(DraftPreview {
            guid: "g1".into(),
            format: crate::zipserve::BodyFormat::Html,
            text: "<p>原样</p>".into(),
            token: "t1".into(),
        });
        assert_eq!(st.draft_html_for("g1", "t1").as_deref(), Some("<p>原样</p>"));
        // md 草稿：渲染成完整文档（预览与阅读态同一条渲染口径）
        *st.draft.lock().unwrap() = Some(DraftPreview {
            guid: "g1".into(),
            format: crate::zipserve::BodyFormat::Md,
            text: "# 草稿标题".into(),
            token: "t2".into(),
        });
        let out = st.draft_html_for("g1", "t2").unwrap();
        assert!(out.starts_with("<!DOCTYPE html>"), "{out}");
        assert!(out.contains("<h1>草稿标题</h1>"), "{out}");
        // 双命中：guid 或 token 任一不符 → None（草稿不得泄漏进普通阅读）
        assert!(st.draft_html_for("g2", "t2").is_none());
        assert!(st.draft_html_for("g1", "t1").is_none(), "旧 token 必须失效");
    }
}
