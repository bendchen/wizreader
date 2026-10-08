//! 应用设置（FR-09）持久化于 ~/.wizreader/settings.json

use std::path::{Path, PathBuf};

/// 角色取值（§7.2）：**写入端** —— 以本地库为准，上行覆盖云端
pub const ROLE_WRITER: &str = "writer";
/// 角色取值（§7.2）：**只读端** —— 以云端为准，下行覆盖本地（不接受本地写入，R7）
pub const ROLE_READER: &str = "reader";

/// 规范化角色：P3 之前的旧值 `export` → [`ROLE_WRITER`]（§7.2「更名（沿用旧值需兼容映射）」）。
/// 其余值原样返回（含空串 = 尚未选择角色）。
pub fn canonical_role(role: &str) -> &str {
    match role {
        "export" => ROLE_WRITER,
        other => other,
    }
}

/// 云同步配置（设计稿 §4.1）。整体 `serde(default)`，旧 settings.json 原样可读。
/// **secret key 绝不出现于此文件**（D2/D3）——只存 keyring 条目的 user 名（endpoint|bucket|prefix 哈希）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncSettings {
    /// 总开关；false 时启动任务只做本地 GC（NFR-3.6 默认离线）
    #[serde(default)]
    pub enabled: bool,
    /// [`ROLE_WRITER`] | [`ROLE_READER`]（D5 显式选择；旧值 `export` 读入时迁移为 `writer`）
    #[serde(default)]
    pub role: String,
    /// **已废弃（D/U1）**：同步根恒为库根 `library_dir`，本字段不再参与任何路径推导。
    /// 仅保留用于反序列化兼容 + 一次性迁移（`load_settings` 里搬到 `library_dir` 并记日志），
    /// 迁移后即被清空。新代码一律用 [`Settings::sync_root`]。
    #[serde(default)]
    pub local_root: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub bucket: String,
    /// 规范化后无首尾 '/'
    #[serde(default)]
    pub prefix: String,
    #[serde(default = "default_sync_region")]
    pub region: String,
    /// MinIO 建议 true
    #[serde(default = "default_true")]
    pub path_style: bool,
    /// 非敏感，可落盘
    #[serde(default)]
    pub access_key_id: String,
    /// keyring entry 的 user 名（= endpoint|bucket|prefix 哈希），支持多套配置并存
    #[serde(default)]
    pub credential_user: String,
    /// 上行并发 1–16（D6）
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    /// FR-09「自动检查同步」默认关（D7）
    #[serde(default)]
    pub auto_check_on_start: bool,
    /// 首次初始化是否已完成（用户问题 2 的判定位）
    #[serde(default)]
    pub initialized: bool,
}

fn default_sync_region() -> String {
    "us-east-1".into()
}
fn default_true() -> bool {
    true
}
fn default_concurrency() -> u32 {
    4
}

// 手写 Default 而非 derive：derive 会把数值/布尔字段置 0/false，
// 绕过 serde 的 default_* 函数（concurrency=0 会被校验拒绝，D6 要求默认 4）
impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            role: String::new(),
            local_root: String::new(),
            endpoint: String::new(),
            bucket: String::new(),
            prefix: String::new(),
            region: default_sync_region(),
            path_style: true,
            access_key_id: String::new(),
            credential_user: String::new(),
            concurrency: default_concurrency(),
            auto_check_on_start: false,
            initialized: false,
        }
    }
}

impl SyncSettings {
    /// 规范化 prefix：去首尾 '/'（空串允许）
    pub fn normalized_prefix(&self) -> String {
        self.prefix.trim_matches('/').to_string()
    }
    /// 云端对象键 = `{prefix}/{rest}`（prefix 为空时无前缀段）
    pub fn cloud_key(&self, rest: &str) -> String {
        let p = self.normalized_prefix();
        if p.is_empty() {
            rest.to_string()
        } else {
            format!("{p}/{rest}")
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// 主数据目录（笔记库根，FR-11）：本软件唯一事实源，含 export.db + 目录树 + 每篇 zip。
    /// 未设置时主界面为空态引导（docs/本地笔记读写实现.md §6.4）
    #[serde(default)]
    pub library_dir: Option<String>,
    /// 源数据目录（为知笔记原始数据）：**只读**、可选、可弃，仅用于导入与导出逃生舱（§8.3）。
    /// 旧字段名 `data_dir` 经 serde alias 兼容读取（§8.1）
    #[serde(default, alias = "data_dir")]
    pub source_dir: Option<String>,
    #[serde(default = "default_font_size")]
    pub font_size: u32,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_read_width")]
    pub read_width: u32,
    #[serde(default)]
    pub allow_remote: bool,
    /// 云同步（阶段二）；serde(default) 保证旧文件可读
    #[serde(default)]
    pub sync: SyncSettings,
}

fn default_font_size() -> u32 {
    16
}
fn default_theme() -> String {
    "system".into()
}
fn default_read_width() -> u32 {
    860
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            library_dir: None,
            source_dir: None,
            font_size: default_font_size(),
            theme: default_theme(),
            read_width: default_read_width(),
            allow_remote: false,
            sync: SyncSettings::default(),
        }
    }
}

/// 同步根（D/U1，§7.1）：**恒为库根 `library_dir`**。
///
/// 库就是唯一事实源，同步只是"把这个库推到/拉到云端"，因此不存在第二个根。
/// `sync.local_root` 已废弃：只在**库未设置**时作过渡回退（旧设置下回收站仍可见），
/// 并且 `load_settings` 会把它搬到 `library_dir` 后清空（一次性迁移 + 迁移日志）。
///
/// 回收站根与同步根是**同一个根**，见 [`Settings::trash_root`]。
impl Settings {
    pub fn sync_root(&self) -> Option<PathBuf> {
        if let Some(d) = self.library_dir.as_deref().filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(d));
        }
        let lr = self.sync.local_root.as_str();
        (!lr.is_empty()).then(|| PathBuf::from(lr))
    }

    /// 回收站根（T7 收口，docs/本地笔记读写实现.md §4.3/§16.5）。
    ///
    /// **库模式下恒为 `library_dir`**：回收站属于**库**，不属于云同步 ——
    /// `library::delete_note` 把文件移进 `library_dir/_trash/{YYYY-MM-DD}/`，
    /// 因此 `list_trash` / `restore_trash` / `purge_trash` / `open_trash_dir` 与启动 GC
    /// 必须看同一个根，否则「删进去的找不回来、GC 也清不掉」。
    ///
    /// U1 之后它与 [`Settings::sync_root`] **同源**（都是库根）—— 保留两个名字是为了让
    /// "回收站根"与"同步根"两个概念在调用点仍然说得清，而不是让读者去猜。
    pub fn trash_root(&self) -> Option<PathBuf> {
        self.sync_root()
    }
}

pub fn wiz_home() -> PathBuf {
    // 测试/调试覆盖：设置 WIZREADER_HOME 后派生数据全部重定向，避免污染真实 ~/.wizreader
    if let Ok(h) = std::env::var("WIZREADER_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".wizreader")
}

pub fn settings_path() -> PathBuf {
    wiz_home().join("settings.json")
}

pub fn load_settings() -> Settings {
    // 旧 settings.json 里的 `sync.mode` 字段已随 D0 移除（格式不再是选项），
    // serde 默认忽略未知字段 → 无需任何迁移代码
    let raw = std::fs::read_to_string(settings_path()).ok();
    let parsed: Settings = raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<Settings>(s).ok())
        .unwrap_or_default();
    let (s, migrated) = migrate_settings(parsed, raw.as_deref());
    if !migrated.is_empty() && save_settings(&s).is_ok() {
        crate::commands::append_sync_log(&format!("设置迁移：{}", migrated.join("；")));
    }
    s
}

/// settings 迁移（**纯函数**）：`settings + 原始文本 → 迁移后的 settings + 迁移说明`。
///
/// 与 `load_settings` 拆开的唯一理由是**可直测**：这里不碰磁盘，单测不必去改进程级的
/// `WIZREADER_HOME`（那会让并行跑的其它测试互相污染，见 §10 的教训）。
///
/// 两条迁移：
/// - **U1**：`sync.local_root` 废弃 → 同步根恒为库根（§7.1）。库未设时把旧值搬成库根
///   （旧模型里它就是"导出根"，与新库根同义）；库已设且不同则忽略旧值并留痕；随后清空字段
///   （否则每次加载都要重复判断）。沿用 `data_dir → source_dir` 那套做法（§8.1）。
/// - **U4**：角色更名 `export` → `writer`（§7.2）。
pub fn migrate_settings(mut s: Settings, raw: Option<&str>) -> (Settings, Vec<String>) {
    let mut migrated: Vec<String> = Vec::new();

    if let Some(r) = raw {
        if r.contains("\"data_dir\"") && s.source_dir.is_some() {
            migrated.push(format!(
                "data_dir → source_dir（{}）",
                s.source_dir.as_deref().unwrap_or_default()
            ));
        }
    }

    if !s.sync.local_root.trim().is_empty() {
        let legacy = s.sync.local_root.clone();
        let lib = s.library_dir.clone().unwrap_or_default();
        if lib.trim().is_empty() {
            s.library_dir = Some(legacy.clone());
            migrated.push(format!("sync.local_root → library_dir（{legacy}）"));
        } else if canon_best_effort(Path::new(&legacy)) != canon_best_effort(Path::new(lib.trim())) {
            migrated.push(format!(
                "已忽略 sync.local_root（{legacy}）—— 同步根恒为库根 {lib}"
            ));
        }
        s.sync.local_root.clear();
    }

    let canon = canonical_role(&s.sync.role).to_string();
    if canon != s.sync.role {
        migrated.push(format!("sync.role {} → {}", s.sync.role, canon));
        s.sync.role = canon;
    }

    (s, migrated)
}

pub fn save_settings(s: &Settings) -> Result<(), String> {
    let dir = wiz_home();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(settings_path(), json).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 校验（设计稿 §4.3，Q18 定案）

fn is_local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// 明文 HTTP 允许的主机：本机回环 + 内网私有地址（RFC1918：10/8、172.16/12、192.168/16）。
/// 公网地址必须走 HTTPS（NFR-3.3）。内网 S3（如局域网 Ceph/MinIO）明文可接受，
/// 因为流量不出内网；仅支持 IP 形式，内网域名请用 HTTPS 或改用 IP。
pub fn is_local_or_private_host(host: &str) -> bool {
    if is_local_host(host) {
        return true;
    }
    match host.parse::<std::net::Ipv4Addr>() {
        Ok(ip) => ip.is_private() || ip.is_loopback(),
        Err(_) => false,
    }
}

/// 保存与「测试连接」共用的校验。`source_dir` 用于 Q18 的同步根红线校验（可传 None 跳过）。
///
/// **U1 起校验的是库根**：同步根恒为 `library_dir`（§7.1），故第一个参数是库根而不是
/// 已废弃的 `sync.local_root`。库尚未设置时跳过该子项（允许先测连接、再选库）。
pub fn validate_sync(
    sync: &SyncSettings,
    library_dir: Option<&str>,
    source_dir: Option<&str>,
) -> Result<(), String> {
    // endpoint：必须含 scheme；明文 http 仅限本机 / 内网私有 IP
    let ep = sync.endpoint.trim().trim_end_matches('/');
    if ep.is_empty() {
        return Err("SYNC_CONFIG_INVALID: endpoint 不能为空".into());
    }
    if !(ep.starts_with("http://") || ep.starts_with("https://")) {
        return Err("SYNC_CONFIG_INVALID: endpoint 必须含 http:// 或 https://".into());
    }
    if ep.starts_with("http://") {
        let host = ep
            .trim_start_matches("http://")
            .split(['/', ':'])
            .next()
            .unwrap_or("");
        if !is_local_or_private_host(host) {
            return Err(format!(
                "SYNC_INSECURE_ENDPOINT: 明文 HTTP 仅允许本机或内网私有 IP，公网地址必须 HTTPS（{host}）"
            ));
        }
    }

    // bucket：3–63 字符，a-z0-9.-，不以 . 或 - 开头结尾
    let b = sync.bucket.as_str();
    let b_ok = (3..=63).contains(&b.len())
        && b.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
        && !b.starts_with(['.', '-'])
        && !b.ends_with(['.', '-']);
    if !b_ok {
        return Err("SYNC_CONFIG_INVALID: bucket 须为 3–63 位小写字母/数字/./-，且不以 . 或 - 开头结尾".into());
    }

    // prefix：拒绝含 `..` 的段
    for seg in sync.normalized_prefix().split('/') {
        if seg == ".." {
            return Err("SYNC_CONFIG_INVALID: prefix 段不得包含 `..`".into());
        }
    }

    // concurrency 1–16（D6）
    if !(1..=16).contains(&sync.concurrency) {
        return Err("SYNC_CONFIG_INVALID: concurrency 取值 1–16".into());
    }

    // role（§7.2）：接受 `writer` / `reader`；旧值 `export` 经 canonical_role 归一为 `writer`
    let role = canonical_role(sync.role.as_str());
    if !role.is_empty() && role != ROLE_WRITER && role != ROLE_READER {
        return Err(format!(
            "SYNC_CONFIG_INVALID: role 只能是 {ROLE_WRITER} 或 {ROLE_READER}"
        ));
    }

    // Q18（U1 后）：**同步根即库根**，故红线校验的对象从 `local_root` 换成 `library_dir`。
    // 库未设置 → 跳过（允许"先测连接、再选库"的顺序），由 sync 流程自己再要求库就绪。
    if let Some(lib) = library_dir.map(str::trim).filter(|s| !s.is_empty()) {
        validate_sync_root(Path::new(lib), source_dir)?;
    }
    Ok(())
}

/// Q18（2026-09-20 用户澄清口径）：红线**唯一目的 = 为知原笔记不可修改**。
/// `root`（= 库根）不得等于 / 位于 `data_dir` 之下；**库包含源（祖先方向）放行** ——
/// 库内写入均为结构化子路径（notes/…、_attachments/、_trash/、_conflicts/、export.db），
/// 不会触碰嵌套在库里的源目录，而库内数据用户有完全控制权（旧版第 3 条「祖先也拒」废止）。
/// 两侧先 `canonicalize`（规避符号链接绕过与 macOS `/var → /private/var` 之类链接）；
/// 不存在的路径对**最深存在祖先**做 canonicalize 后再拼回剩余段，保证与已存在路径可比。
pub fn validate_sync_root(root: &Path, data_dir: Option<&str>) -> Result<(), String> {
    let r = canon_best_effort(root);
    if let Some(dd) = data_dir {
        if dd.trim().is_empty() {
            return Ok(());
        }
        let d = canon_best_effort(Path::new(dd));
        if r == d {
            return Err("SYNC_ROOT_UNDER_SOURCE: 同步根（=笔记库根）不得与为知源数据目录相同（G1：为知原笔记只读）".into());
        }
        if r.starts_with(&d) {
            return Err("SYNC_ROOT_UNDER_SOURCE: 同步根（=笔记库根）不得位于为知源数据目录之内（G1：为知原笔记只读）".into());
        }
    }
    // 可写性：存在则探测写权限，不存在则尝试创建
    if !r.exists() {
        std::fs::create_dir_all(&r).map_err(|e| format!("SYNC_ROOT_UNWRITABLE: 无法创建同步根（{e}）"))?;
    }
    let probe = r.join(".wizreader-probe");
    std::fs::write(&probe, b"ok").map_err(|e| format!("SYNC_ROOT_UNWRITABLE: 同步根不可写（{e}）"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// 尽力 canonicalize：路径本身不存在时，向上找最深**已存在**祖先做 canonicalize，
/// 再拼回剩余段（否则与已存在的真实路径比较会因符号链接前缀错位而失效）
pub fn canon_best_effort(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut anc = p.to_path_buf();
    let mut tails: Vec<std::ffi::OsString> = Vec::new();
    while !anc.exists() {
        match anc.parent() {
            Some(par) if par != anc => {
                if let Some(f) = anc.file_name() {
                    tails.push(f.to_os_string());
                }
                anc = par.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = anc.canonicalize().unwrap_or(anc);
    for t in tails.iter().rev() {
        out.push(t);
    }
    out
}

// ---------------------------------------------------------------- 库根校验（FR-11，R9 红线）

/// 库根红线（§6.2 / R9，2026-09-20 与 Q18 同步澄清）：`library_dir` 不得与为知源数据目录
/// 相等 / 位于其之内（红线唯一目的 = 为知原笔记不可修改）；**库包含源放行**（库内数据用户完全控制，
/// 库内写入均为结构化子路径，不会触碰嵌套的源目录）。复用 Q18 的 `canon_best_effort` 思路防符号链接绕过。
/// 目录不存在时顺带创建 + 可写探测（与 `validate_sync_root` 同构）。
pub fn validate_library_root(library: &Path, source_dir: Option<&str>) -> Result<(), String> {
    let l = canon_best_effort(library);
    if let Some(sd) = source_dir {
        if !sd.trim().is_empty() {
            let d = canon_best_effort(Path::new(sd));
            if l == d {
                return Err("LIBRARY_ROOT_UNDER_SOURCE: 库根不得与为知源数据目录相同（G1：为知原笔记只读）".into());
            }
            if l.starts_with(&d) {
                return Err("LIBRARY_ROOT_UNDER_SOURCE: 库根不得位于为知源数据目录之内（G1：为知原笔记只读）".into());
            }
        }
    }
    if !l.exists() {
        std::fs::create_dir_all(&l).map_err(|e| format!("LIBRARY_ROOT_UNWRITABLE: 无法创建库根（{e}）"))?;
    }
    let probe = l.join(".wizreader-probe");
    std::fs::write(&probe, b"ok").map_err(|e| format!("LIBRARY_ROOT_UNWRITABLE: 库根不可写（{e}）"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// 路径哈希（FNV-1a 低 32 位，8 位十六进制）：先 canonicalize 再算，
/// 因此 `/tmp/x` 与 macOS 的 `/private/tmp/x` 得到同一个值（否则同一目录会算出两个身份）。
///
/// 两个用途共用它，理由相同 —— **按库隔离派生数据**：
/// ① 库索引文件名 `index-{hash8}.db`（§5.3/Q11）；
/// ② 同步暂存目录（`sync_down` 读远端清单用的临时落点）。
pub fn path_hash8(p: &Path) -> String {
    let canon = canon_best_effort(p);
    let key = canon.to_string_lossy();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", h & 0xffff_ffff)
}

/// 库模式的派生索引文件名（§5.3 / Q11）：`index-{hash8}.db`，hash8 = 库根规范化路径的
/// FNV-1a 十六进制前 8 位。索引永不入库（R8：库要上云），故仍放 `~/.wizreader/`；
/// 哈希后缀为未来多库并挂预留（Q9）。
pub fn index_file_for_library(library_dir: &Path) -> PathBuf {
    wiz_home().join(format!("index-{}.db", path_hash8(library_dir)))
}

/// 同步暂存目录（`sync_down` 的护栏与差集都需要"把远端清单当库读一遍"）。
///
/// `~/.wizreader/remote-manifest/{pid}-{库根hash8}/`：**按库 + 按进程**唯一。
/// 共用一条固定路径会让同进程内两个库的并发下行互相读到对方的清单 —— 症状是护栏把
/// "本地新增"判错（另一库的清单里当然没有这篇），并行测试立刻踩到。
pub fn sync_stash_dir(root: &Path) -> PathBuf {
    wiz_home()
        .join("remote-manifest")
        .join(format!("{}-{}", std::process::id(), path_hash8(root)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SyncSettings {
        SyncSettings {
            endpoint: "https://minio.example.com:9000".into(),
            bucket: "wiznotes".into(),
            prefix: "wiznotes".into(),
            region: "us-east-1".into(),
            path_style: true,
            access_key_id: "ak".into(),
            role: ROLE_WRITER.into(),
            // U1 起 `local_root` 不参与任何推导 → 测试基准里**故意不设**
            ..Default::default()
        }
    }

    #[test]
    fn test_validate_ok() {
        assert!(validate_sync(&base(), None, None).is_ok());
    }

    #[test]
    fn test_validate_insecure_endpoint() {
        let mut s = base();
        s.endpoint = "http://minio.example.com:9000".into();
        assert!(validate_sync(&s, None, None).unwrap_err().contains("SYNC_INSECURE_ENDPOINT"));
        s.endpoint = "http://127.0.0.1:9000".into();
        assert!(validate_sync(&s, None, None).is_ok(), "本机 http 放行");
        s.endpoint = "http://192.168.1.10:7480".into();
        assert!(validate_sync(&s, None, None).is_ok(), "内网私有 IP http 放行（如局域网 Ceph）");
        s.endpoint = "http://10.0.0.5:9000".into();
        assert!(validate_sync(&s, None, None).is_ok(), "10/8 内网放行");
        s.endpoint = "http://8.8.8.8:9000".into();
        assert!(validate_sync(&s, None, None).unwrap_err().contains("SYNC_INSECURE_ENDPOINT"), "公网 IP 仍拒绝");
        s.endpoint = "minio.example.com".into();
        assert!(validate_sync(&s, None, None).is_err(), "必须含 scheme");
    }

    #[test]
    fn test_validate_bucket_and_prefix() {
        let mut s = base();
        s.bucket = "ab".into();
        assert!(validate_sync(&s, None, None).is_err());
        s.bucket = "Wiz".into();
        assert!(validate_sync(&s, None, None).is_err());
        s.bucket = "-wiz".into();
        assert!(validate_sync(&s, None, None).is_err());
        s.bucket = "wiznotes".into();
        s.prefix = "a/../b".into();
        assert!(validate_sync(&s, None, None).is_err());
        s.prefix = "/wiz/notes/".into();
        assert_eq!(s.normalized_prefix(), "wiz/notes");
    }

    #[test]
    fn test_validate_concurrency_and_role() {
        let mut s = base();
        s.concurrency = 0;
        assert!(validate_sync(&s, None, None).is_err());
        s.concurrency = 17;
        assert!(validate_sync(&s, None, None).is_err());
        s.concurrency = 16;
        assert!(validate_sync(&s, None, None).is_ok());
        s.role = "master".into();
        assert!(validate_sync(&s, None, None).is_err());
    }

    #[test]
    fn test_q18_sync_root_rules() {
        let dd = std::env::temp_dir().join(format!("wiz-q18-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dd);
        std::fs::create_dir_all(dd.join("notes")).unwrap();
        let dd_s = dd.to_string_lossy().to_string();

        // 等于 → 拒
        let err = validate_sync_root(&dd, Some(&dd_s)).unwrap_err();
        assert!(err.contains("SYNC_ROOT_UNDER_SOURCE"), "{err}");
        // 之下 → 拒
        let err = validate_sync_root(&dd.join("sync"), Some(&dd_s)).unwrap_err();
        assert!(err.contains("SYNC_ROOT_UNDER_SOURCE"));
        // 祖先（库包含源目录）→ **放行**（2026-09-20 用户澄清：库内数据用户完全控制，
        // 库内写入均为结构化子路径，不会触碰嵌套的源目录；旧版此处拒）
        let parent = dd.parent().unwrap().to_path_buf();
        validate_sync_root(&parent, Some(&dd_s)).unwrap();
        // 兄弟目录 → 放行
        let sibling = dd.parent().unwrap().join(format!("wiz-q18-ok-{}", std::process::id()));
        validate_sync_root(&sibling, Some(&dd_s)).unwrap();
        assert!(sibling.exists(), "校验时可顺带创建同步根");
        let _ = std::fs::remove_dir_all(&sibling);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn test_q18_via_canonicalize_symlink() {
        // 符号链接绕过也必须被拒（验收 P1 ⑥）
        let dd = std::env::temp_dir().join(format!("wiz-q18-src2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dd);
        std::fs::create_dir_all(&dd).unwrap();
        let link = std::env::temp_dir().join(format!("wiz-q18-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dd, &link).unwrap();
        let dd_s = dd.to_string_lossy().to_string();
        let err = validate_sync_root(&link, Some(&dd_s)).unwrap_err();
        assert!(err.contains("SYNC_ROOT_UNDER_SOURCE"), "{err}");
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn test_cloud_key() {
        let mut s = base();
        assert_eq!(s.cloud_key("manifest.db"), "wiznotes/manifest.db");
        s.prefix = "".into();
        assert_eq!(s.cloud_key("manifest.db"), "manifest.db");
    }

    // ---------------------------------------------------------------- FR-11（P0）

    /// §8.1 兼容读取：旧 settings.json 的 `data_dir` 经 alias 读入 `source_dir`；
    /// 新字段名与 `library_dir` 正常往返
    #[test]
    fn test_settings_alias_data_dir() {
        let old = r#"{"data_dir":"/Users/me/.wiznote/x/data","font_size":16}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.source_dir.as_deref(), Some("/Users/me/.wiznote/x/data"));
        assert_eq!(s.library_dir, None);

        let new = r#"{"library_dir":"/Users/me/WizLibrary","source_dir":"/src","sync":{"local_root":"/tmp/x"}}"#;
        let s2: Settings = serde_json::from_str(new).unwrap();
        assert_eq!(s2.library_dir.as_deref(), Some("/Users/me/WizLibrary"));
        assert_eq!(s2.source_dir.as_deref(), Some("/src"));
        assert_eq!(s2.sync.local_root, "/tmp/x");
        // 序列化用新字段名，不再出现 data_dir
        let out = serde_json::to_string(&s2).unwrap();
        assert!(out.contains("source_dir") && !out.contains("data_dir"));
    }

    /// R9 红线（2026-09-20 澄清后两态）：相等 / 之下拒，库包含源放行，兄弟目录放行（与 Q18 同构，含创建）
    #[test]
    fn test_library_root_red_lines() {
        let sd = std::env::temp_dir().join(format!("wiz-lib-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sd);
        std::fs::create_dir_all(sd.join("notes")).unwrap();
        let sd_s = sd.to_string_lossy().to_string();

        let err = validate_library_root(&sd, Some(&sd_s)).unwrap_err();
        assert!(err.contains("LIBRARY_ROOT_UNDER_SOURCE"), "{err}");
        let err = validate_library_root(&sd.join("lib"), Some(&sd_s)).unwrap_err();
        assert!(err.contains("LIBRARY_ROOT_UNDER_SOURCE"));
        // 祖先（库包含源目录）→ **放行**（2026-09-20 用户澄清，同 Q18）
        let parent = sd.parent().unwrap().to_path_buf();
        validate_library_root(&parent, Some(&sd_s)).unwrap();

        let ok_dir = std::env::temp_dir().join(format!("wiz-lib-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ok_dir);
        validate_library_root(&ok_dir, Some(&sd_s)).unwrap();
        assert!(ok_dir.exists(), "校验时可顺带创建库根");
        // 无源目录（源可弃，§8.3）时不做红线比较
        validate_library_root(&ok_dir, None).unwrap();
        let _ = std::fs::remove_dir_all(&ok_dir);
        let _ = std::fs::remove_dir_all(&sd);
    }

    /// §5.3：库索引文件名稳定（同路径同哈希）且不同于源模式 index.db
    #[test]
    fn test_index_file_for_library() {
        let a = index_file_for_library(Path::new("/tmp/wiz-lib-a"));
        let b = index_file_for_library(Path::new("/tmp/wiz-lib-a"));
        let c = index_file_for_library(Path::new("/tmp/wiz-lib-b"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("index-"));
        assert!(a.file_name().unwrap().to_string_lossy().ends_with(".db"));
    }

    /// `path_hash8` 是**派生数据身份**的唯一来源（索引文件名 + 同步暂存目录），
    /// 三条性质都要钉住：① 同路径同值；② 异路径异值；③ **canonicalize 后再算**
    /// （macOS 上 `/tmp/x` 与 `/private/tmp/x` 是同一目录，必须同值 —— 否则同一库会
    /// 算出两个身份，索引与暂存目录各认一个）。
    ///
    /// 末条带一个**磁盘实测锚点**：真实库 `/private/tmp/wizlib-md-bak-smoke` 的索引文件名
    /// 就是 `index-48aa7cca.db`（见 `.workbuddy/memory/MEMORY.md`）。锚点值变了 =
    /// 身份算法被改动，旧索引会全部变成孤儿 —— 这条断言就是防它的。
    #[test]
    fn test_path_hash8_identity() {
        assert_eq!(path_hash8(Path::new("/tmp/wiz-lib-a")), path_hash8(Path::new("/tmp/wiz-lib-a")));
        assert_ne!(path_hash8(Path::new("/tmp/wiz-lib-a")), path_hash8(Path::new("/tmp/wiz-lib-b")));
        if Path::new("/private/tmp").is_dir() {
            assert_eq!(
                path_hash8(Path::new("/tmp/wiz-lib-a")),
                path_hash8(Path::new("/private/tmp/wiz-lib-a")),
                "macOS 的 /tmp 是 /private/tmp 的链接，必须是同一个身份"
            );
            assert_eq!(path_hash8(Path::new("/private/tmp/wizlib-md-bak-smoke")), "48aa7cca");
        }
        assert_eq!(path_hash8(Path::new("x")).len(), 8, "恒为 8 位十六进制");
    }

    /// 同步暂存目录：**按库 + 按进程**唯一 —— 两个库并发下行不得共用一条路径
    /// （共用会让护栏读到对方的远端清单，把"本地新增"判错）
    #[test]
    fn test_sync_stash_dir_is_per_library() {
        let a = sync_stash_dir(Path::new("/tmp/wiz-lib-a"));
        let b = sync_stash_dir(Path::new("/tmp/wiz-lib-b"));
        assert_ne!(a, b, "不同库不得共用暂存目录");
        assert_eq!(a, sync_stash_dir(Path::new("/tmp/wiz-lib-a")));
        assert!(a.to_string_lossy().contains(&std::process::id().to_string()), "含 pid：{a:?}");
        assert!(a.starts_with(wiz_home()), "暂存只在 ~/.wizreader 下: {a:?}");
    }

    // ---------------------------------------------------------------- D 轮（U1 / U4）

    /// U4：旧角色值 `export` 归一为 `writer`，其余原样（含空串 = 未选角色）
    #[test]
    fn test_canonical_role() {
        assert_eq!(canonical_role("export"), ROLE_WRITER);
        assert_eq!(canonical_role("writer"), ROLE_WRITER);
        assert_eq!(canonical_role("reader"), ROLE_READER);
        assert_eq!(canonical_role(""), "");
        assert_eq!(canonical_role("master"), "master");
        // 校验层：旧值合法（归一后即 writer），未知值仍拒
        let mut s = base();
        s.role = "export".into();
        assert!(validate_sync(&s, None, None).is_ok());
        s.role = "master".into();
        assert!(validate_sync(&s, None, None).unwrap_err().contains("role"));
    }

    /// U1 + U4 迁移（纯函数，不落盘）：
    /// - 库未设 → 旧 `sync.local_root` 搬成 `library_dir` 并清空字段
    /// - 库已设且不同 → 忽略旧值并留痕
    /// - 旧角色 `export` → `writer`
    #[test]
    fn test_migrate_settings_local_root_and_role() {
        // ① 只有旧 local_root：搬成库根
        let raw = r#"{"sync":{"local_root":"/Users/me/WizLib","role":"export","enabled":true}}"#;
        let s: Settings = serde_json::from_str(raw).unwrap();
        let (s, log) = migrate_settings(s, Some(raw));
        assert_eq!(s.library_dir.as_deref(), Some("/Users/me/WizLib"));
        assert_eq!(s.sync.local_root, "", "迁移后清空，避免每次加载重复判断");
        assert_eq!(s.sync.role, ROLE_WRITER, "export → writer");
        assert!(s.sync.enabled, "其它字段不受影响");
        assert!(log.iter().any(|l| l.contains("sync.local_root → library_dir")), "{log:?}");
        assert!(log.iter().any(|l| l.contains("sync.role export → writer")), "{log:?}");
        // 迁移后的数值口径：同步根 = 库根
        assert_eq!(s.sync_root(), Some(PathBuf::from("/Users/me/WizLib")));

        // ② 库已设且与旧值不同：忽略旧值 + 留痕，库根不动
        let raw2 = r#"{"library_dir":"/Users/me/RealLib","sync":{"local_root":"/tmp/old","role":"reader"}}"#;
        let s2: Settings = serde_json::from_str(raw2).unwrap();
        let (s2, log2) = migrate_settings(s2, Some(raw2));
        assert_eq!(s2.library_dir.as_deref(), Some("/Users/me/RealLib"));
        assert_eq!(s2.sync.local_root, "");
        assert_eq!(s2.sync.role, ROLE_READER, "reader 不变");
        assert!(log2.iter().any(|l| l.contains("已忽略 sync.local_root")), "{log2:?}");

        // ③ 幂等：已迁移过的设置再跑一遍没有新日志
        let (s3, log3) = migrate_settings(s2.clone(), None);
        assert!(log3.is_empty(), "已清空 local_root / 角色已规范化 → 无新迁移: {log3:?}");
        assert_eq!(s3.library_dir, s2.library_dir);

        // ④ 空库根 + 空 local_root → 同步根为 None（而不是空 PathBuf）
        assert_eq!(Settings::default().sync_root(), None);
    }

    /// U1：红线校验的对象是**库根**（不再是已废弃的 local_root）
    #[test]
    fn test_validate_sync_redline_uses_library_root() {
        let sd = std::env::temp_dir().join(format!("wiz-vs-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sd);
        std::fs::create_dir_all(sd.join("notes")).unwrap();
        let sd_s = sd.to_string_lossy().to_string();
        let s = base();
        // 库根 = 源目录 → 拒（即便 local_root 为空）
        let err = validate_sync(&s, Some(&sd_s), Some(&sd_s)).unwrap_err();
        assert!(err.contains("SYNC_ROOT_UNDER_SOURCE"), "{err}");
        // 库未设 → 跳过该子项（允许"先测连接、再选库"）
        assert!(validate_sync(&s, None, Some(&sd_s)).is_ok());
        assert!(validate_sync(&s, Some("  "), Some(&sd_s)).is_ok(), "空白库根视同未设");
        let _ = std::fs::remove_dir_all(&sd);
    }
}
