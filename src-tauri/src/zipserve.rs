//! zip 流式读取服务（T0.2/T2.1）
//!
//! - 按需读取 zip 内条目，绝不落盘解压
//! - LRU 缓存已打开的 zip 句柄（容量 32）
//! - 路径穿越防护：请求路径规范化后必须能在该 zip 条目列表中精确命中
//! - Content-Type 按扩展名推断
//! - guid → zip 路径经 [`NotePathResolver`] 解析（FR-11 §4.1 方案 A）：
//!   源模式 [`SourceResolver`] 拼 `notes/{GUID}`；库模式走清单 `exported_path`（library.rs）
//! - **正文形态感知**（M3/§20.3）：包内正文可能是 `note.md`（M2 起的 md 包）或
//!   `index.html`（为知原生包），见 [`BodyFormat`]。读正文一律走
//!   [`ZipService::read_note_body`] / [`ZipService::read_note_document`]，
//!   不要再直接拼 `index.html` —— 那正是 M2 之后「md 库可列可搜不可读」的根因。

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use std::num::NonZeroUsize;
use zip::ZipArchive;

const CACHE_CAP: usize = 32;

/// 库内**正文形态**（§20.3）。
///
/// 两种形态都是**本程序的合法产物**，故读侧必须都能读 —— 这不是"历史兼容分支"：
/// - [`BodyFormat::Md`]：M2 起的库内形态，正文条目 `note.md`（Markdown 源）；
/// - [`BodyFormat::Html`]：为知原生形态，正文条目 `index.html`（HTML 源）。
///
/// 判定**只看包内条目**，不看清单 `export_mode`：清单是格式标识，包才是事实；
/// 且判定必须对得上写路径（`library::package_body_format` 同口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFormat {
    /// md 包：正文是 Markdown 源（`note.md`）
    Md,
    /// 为知原生包：正文是 HTML 源（`index.html`）
    Html,
}

impl BodyFormat {
    /// 包内正文档名
    pub fn entry(self) -> &'static str {
        match self {
            BodyFormat::Md => crate::md::NOTE_MD,
            BodyFormat::Html => "index.html",
        }
    }

    /// 短标识（前端 / CLI / 报告用）
    pub fn tag(self) -> &'static str {
        match self {
            BodyFormat::Md => "md",
            BodyFormat::Html => "html",
        }
    }
}

/// guid → zip 绝对路径解析器（读路径唯一入口，§13.1：库模式绝不拼路径）
pub trait NotePathResolver: Send + Sync {
    /// guid 兼容 `{uuid}` 与 `uuid` 两种形态；无法解析时返回 None
    fn resolve(&self, guid: &str) -> Option<PathBuf>;
    /// 缓存键命名空间（源/库切换后同 guid 不串缓存）
    fn mode_tag(&self) -> &'static str;
}

/// guid 规范化：剥花括号后必须是纯十六进制+连字符（防注入），返回 `{uuid}` 形态
pub fn normalize_guid(guid: &str) -> Option<String> {
    let inner = guid
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(guid);
    let valid = !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-');
    valid.then(|| format!("{{{}}}", inner))
}

/// 源数据模式解析器：`{notes_dir}/{GUID}`（磁盘上无 .zip 扩展名，继承为知命名）
pub struct SourceResolver {
    pub notes_dir: PathBuf,
}

impl NotePathResolver for SourceResolver {
    fn resolve(&self, guid: &str) -> Option<PathBuf> {
        let name = normalize_guid(guid)?;
        Some(self.notes_dir.join(name))
    }
    fn mode_tag(&self) -> &'static str {
        "src"
    }
}

/// 空解析器：「未打开任何笔记」态（`ViewContext::None`）专用。
///
/// `resolve` 恒 `None` ⇒ 任何 guid 都读不到 zip（`ZipError::NoSuchNote`），
/// 从**读路径**上杜绝「未设置 / 清单不可读的数据目录」被静默当成数据来源展示。
pub struct EmptyResolver;

impl NotePathResolver for EmptyResolver {
    fn resolve(&self, _guid: &str) -> Option<PathBuf> {
        None
    }
    fn mode_tag(&self) -> &'static str {
        "none"
    }
}

pub struct ZipService {
    /// guid → zip 路径解析器（源模式 / 库模式可切换，切换时清空缓存）
    resolver: Mutex<Arc<dyn NotePathResolver>>,
    cache: Mutex<LruCache<String, std::sync::Arc<Mutex<ZipArchive<File>>>>>,
}

#[derive(Debug)]
pub enum ZipError {
    /// zip 包不存在 / 打不开
    NoSuchNote,
    /// 路径穿越或非法
    IllegalPath(String),
    /// 条目不存在（268 处已知缺失走占位）
    NoSuchEntry(String),
    Io(String),
}

impl ZipError {
    pub fn message(&self) -> String {
        match self {
            ZipError::NoSuchNote => "笔记包不存在".into(),
            ZipError::IllegalPath(p) => format!("非法路径: {}", p),
            ZipError::NoSuchEntry(p) => format!("条目缺失: {}", p),
            ZipError::Io(e) => format!("IO 错误: {}", e),
        }
    }
}

impl From<ZipError> for String {
    fn from(e: ZipError) -> String {
        e.message()
    }
}

impl ZipService {
    pub fn new(notes_dir: PathBuf) -> Self {
        Self {
            resolver: Mutex::new(Arc::new(SourceResolver { notes_dir })),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_CAP).unwrap())),
        }
    }

    /// 以指定解析器构建（库模式建索引用：注入 LibraryResolver）
    pub fn with_resolver(resolver: Arc<dyn NotePathResolver>) -> Self {
        Self {
            resolver: Mutex::new(resolver),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_CAP).unwrap())),
        }
    }

    /// 空服务：不指向任何库 / 源（启动时无可用库 → `ViewContext::None`）。
    /// 所有 `read_*` 均返回 `NoSuchNote`，界面读不到任何笔记。
    pub fn none() -> Self {
        Self::with_resolver(Arc::new(EmptyResolver))
    }

    pub fn set_notes_dir(&self, dir: PathBuf) {
        self.set_resolver(Arc::new(SourceResolver { notes_dir: dir }));
    }

    /// 注入解析器（§4.1 方案 A）：源/库模式切换的唯一入口，切换即清缓存
    pub fn set_resolver(&self, resolver: Arc<dyn NotePathResolver>) {
        *self.resolver.lock().unwrap() = resolver;
        self.cache.lock().unwrap().clear();
    }

    /// 路径穿越防护（P10）：
    /// - 拒绝绝对路径、反斜杠、URI 冒号、NUL、`..`
    /// - percent 编码**逐轮解码后复验**：`..%2f..%2fetc` 这类二次编码不得绕过（SEC-4 实测发现）
    /// - `.` 与空段（`a//b`）按规范化收敛，结果只可能是包内相对路径
    pub fn sanitize_entry_path(raw: &str) -> Result<String, ZipError> {
        let mut cur = raw.to_string();
        for _ in 0..3 {
            Self::reject_escape(&cur)?;
            let next = percent_encoding::percent_decode_str(&cur)
                .decode_utf8_lossy()
                .into_owned();
            if next == cur {
                break;
            }
            cur = next;
        }
        let mut parts: Vec<&str> = Vec::new();
        for seg in cur.split('/') {
            match seg {
                "" => continue,
                "." => continue,
                s => parts.push(s),
            }
        }
        if parts.is_empty() {
            return Err(ZipError::IllegalPath(raw.into()));
        }
        Ok(parts.join("/"))
    }

    /// 单层形态检查（原始串与每一轮解码结果都要过一遍）
    fn reject_escape(s: &str) -> Result<(), ZipError> {
        if s.is_empty()
            || s.contains('\\')
            || s.starts_with('/')
            || s.contains(':')
            || s.contains('\0')
            || s.split('/').any(|seg| seg == "..")
        {
            return Err(ZipError::IllegalPath(s.into()));
        }
        Ok(())
    }

    fn open_archive(&self, guid: &str) -> Result<std::sync::Arc<Mutex<ZipArchive<File>>>, ZipError> {
        // guid 兼容两种形态：{uuid}（文件系统名）与 uuid（WIZ_DOCUMENT.DOCUMENT_GUID），
        // 统一规范化为 {uuid} 后再校验，防注入
        let Some(note_name) = normalize_guid(guid) else {
            return Err(ZipError::IllegalPath(guid.into()));
        };
        let resolver = self.resolver.lock().unwrap().clone();
        let cache_key = format!("{}:{}", resolver.mode_tag(), note_name);
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(a) = cache.get(&cache_key) {
                return Ok(a.clone());
            }
        }
        // 路径解析全部交给解析器：源模式拼 {GUID}，库模式读清单 exported_path（§13.1）
        let path = resolver
            .resolve(&note_name)
            .filter(|p| p.is_file())
            .ok_or(ZipError::NoSuchNote)?;
        let file = File::open(&path).map_err(|e| ZipError::Io(e.to_string()))?;
        let archive = ZipArchive::new(file).map_err(|e| ZipError::Io(e.to_string()))?;
        let arc = std::sync::Arc::new(Mutex::new(archive));
        self.cache.lock().unwrap().put(cache_key, arc.clone());
        Ok(arc)
    }

    /// 读取 zip 内条目，返回字节流
    pub fn read_entry(&self, guid: &str, raw_path: &str) -> Result<Vec<u8>, ZipError> {
        let entry_path = Self::sanitize_entry_path(raw_path)?;
        let archive = self.open_archive(guid)?;
        let mut ar = archive.lock().unwrap();
        // 先精确命中；再尝试对每个段做 percent 解码后的命中（URL 已解码则直接命中）
        let names: Vec<String> = ar.file_names().map(|s| s.to_string()).collect();
        let hit = names.iter().find(|n| n.as_str() == entry_path);
        let name = match hit {
            Some(n) => n.clone(),
            None => {
                // 防御：zip 内条目名可能是已编码形态
                let decoded = percent_encoding::percent_decode_str(entry_path.as_str())
                    .decode_utf8_lossy()
                    .to_string();
                if decoded != entry_path {
                    names
                        .iter()
                        .find(|n| n.as_str() == decoded)
                        .cloned()
                        .ok_or_else(|| ZipError::NoSuchEntry(entry_path.clone()))?
                } else {
                    return Err(ZipError::NoSuchEntry(entry_path));
                }
            }
        };
        let mut f = ar.by_name(&name).map_err(|e| ZipError::Io(e.to_string()))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| ZipError::Io(e.to_string()))?;
        Ok(buf)
    }

    /// 是否存在某条目
    pub fn has_entry(&self, guid: &str, entry: &str) -> bool {
        let Ok(archive) = self.open_archive(guid) else {
            return false;
        };
        let ar = archive.lock().unwrap();
        let hit = ar.file_names().any(|n| n == entry);
        hit
    }

    /// 列出全部条目名（导出用）
    pub fn list_entries(&self, guid: &str) -> Result<Vec<String>, ZipError> {
        let archive = self.open_archive(guid)?;
        let ar = archive.lock().unwrap();
        Ok(ar.file_names().map(|s| s.to_string()).collect())
    }

    /// 读取 `index.html` 并按 utf-8-sig 解码（G3：忽略 <meta charset>）。
    ///
    /// **只服务为知原生包**（源数据目录 / 导出 / 巡检）：那三处的正文条目恒为
    /// `index.html`。读**库内**笔记请用 [`Self::read_note_body`]（形态自适应）。
    pub fn read_index_html(&self, guid: &str) -> Result<String, ZipError> {
        let bytes = self.read_entry(guid, "index.html")?;
        Ok(decode_utf8_sig(&bytes))
    }

    /// 该篇的正文形态（§20.3）：包内有 `note.md` → md 包，否则为知原生包。
    ///
    /// md 优先是**结构性事实**而非偏好：M2 的 `write_md_package` 产 md 包时
    /// `index.html` 与 `note.md` 是**一对一替换**，md 包里不会同时存在两者。
    pub fn body_format(&self, guid: &str) -> BodyFormat {
        if self.has_entry(guid, crate::md::NOTE_MD) {
            BodyFormat::Md
        } else {
            BodyFormat::Html
        }
    }

    /// 正文形态的**可信**短标识（M4，阅读态形态徽标用）：`Some("md"/"html")`
    /// 仅当该形态的正文条目**确实存在**；包缺失/已删/无正文 → `None`。
    ///
    /// 为什么要多这一层：`body_format` 在"读不到包"时会落到默认 `Html`（它是判定，
    /// 不是查询），直接把它的 `tag()` 交出去，前端就会把"这篇读不到"显示成
    /// "这是 HTML 笔记" —— 一个安静的错报。这里用 `has_entry` 兜住这个歧义，
    /// 而且只看目录、不读正文（列表/详情页会逐篇调用它）。
    pub fn body_format_tag(&self, guid: &str) -> Option<String> {
        let fmt = self.body_format(guid);
        self.has_entry(guid, fmt.entry()).then(|| fmt.tag().to_string())
    }

    /// 读正文**源文本**（不渲染）：md 包 → Markdown；native 包 → HTML（BOM 已剥）。
    ///
    /// 编辑抽屉的初值（`get_note_source`）与 CLI `note-info` 走这条 ——
    /// 拿到什么形态，编辑器就按什么形态改，保存时再按同一形态写回（§20.8）。
    pub fn read_note_body(&self, guid: &str) -> Result<(BodyFormat, String), ZipError> {
        let fmt = self.body_format(guid);
        let bytes = self.read_entry(guid, fmt.entry())?;
        Ok((fmt, decode_utf8_sig(&bytes)))
    }

    /// 读正文并渲染为**可阅读 HTML 文档**（阅读态 / 编辑预览的**唯一出口**，§20.7）：
    /// md 包 → [`crate::md::md_to_html_document`]；native 包 → 原样 `index.html`。
    ///
    /// 调用方拿到的是"可直接塞进 iframe 的 HTML"，与包内形态无关 ——
    /// 故 `wiznote://` 的地址与前端**都不必知道**库是哪种形态。
    /// `title` 只写进 `<title>`（iframe 内不显示），传 guid 即可。
    pub fn read_note_document(&self, guid: &str, title: &str) -> Result<String, ZipError> {
        let (fmt, text) = self.read_note_body(guid)?;
        Ok(match fmt {
            BodyFormat::Md => crate::md::md_to_html_document(&text, title),
            BodyFormat::Html => text,
        })
    }
}

/// 剥离 UTF-8 BOM（G3），不改写数据，仅返回尾部切片
pub fn strip_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    }
}

/// utf-8-sig 解码：剥离 BOM（G3）
pub fn decode_utf8_sig(bytes: &[u8]) -> String {
    String::from_utf8_lossy(strip_bom(bytes)).to_string()
}

/// 按扩展名推断 Content-Type
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "json" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "php" => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 缺失资源的占位图（P4）：灰色框 + 原文件名
pub fn placeholder_svg(filename: &str) -> Vec<u8> {
    let esc = filename
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let short: String = if esc.chars().count() > 40 {
        esc.chars().take(40).collect::<String>() + "…"
    } else {
        esc.clone()
    };
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="220" height="90">
<title>资源缺失: {esc}</title>
<rect x="1" y="1" width="218" height="88" rx="6" fill="#f0f0f0" stroke="#bbb" stroke-dasharray="4 3"/>
<text x="110" y="40" text-anchor="middle" font-family="sans-serif" font-size="12" fill="#999">资源缺失</text>
<text x="110" y="60" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#aaa">{short}</text>
</svg>"##,
    );
    svg.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// guid 规范化：剥/补花括号、防注入
    #[test]
    fn test_normalize_guid() {
        let g = "11111111-2222-3333-4444-555555555555";
        let braced = format!("{{{g}}}");
        assert_eq!(normalize_guid(g).as_deref(), Some(braced.as_str()));
        assert_eq!(normalize_guid(&braced).as_deref(), Some(braced.as_str()));
        assert_eq!(normalize_guid("../etc/passwd"), None);
        assert_eq!(normalize_guid(""), None);
    }

    /// SourceResolver 回归：resolve 拼 notes/{GUID}，mode_tag=src，非法 guid 拒绝
    #[test]
    fn test_source_resolver_path() {
        let r = SourceResolver { notes_dir: PathBuf::from("/data/notes") };
        let g = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert_eq!(r.resolve(g), Some(PathBuf::from(format!("/data/notes/{{{g}}}"))));
        assert_eq!(r.mode_tag(), "src");
        assert_eq!(r.resolve("bad guid!"), None);
    }

    /// EmptyResolver（ViewContext::None）：任何 guid 都解析不到 ⇒ 未打开笔记时读不到数据
    #[test]
    fn test_empty_resolver_reads_nothing() {
        let svc = ZipService::none();
        let g = "11111111-2222-3333-4444-555555555555";
        assert!(svc.read_index_html(g).is_err());
        assert!(!svc.has_entry(g, "index.html"));
        assert!(svc.list_entries(g).is_err());
        // 形态判定在"读不到包"时只能落到默认（Html），但正文读取必须仍是错 ——
        // 形态判定绝不能变成"未打开笔记也能读出东西"的后门
        assert!(svc.read_note_body(g).is_err());
        assert!(svc.read_note_document(g, "t").is_err());
    }

    /// ZipService 端到端回归：真实 zip（含 BOM 的 index.html）流式读取，行为不变
    #[test]
    fn test_zip_service_read_index_html() {
        let dir = std::env::temp_dir().join(format!("wiz-zipserve-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let guid = "12345678-1234-1234-1234-123456789abc";
        let zip_path = dir.join(format!("{{{guid}}}"));
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opt = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file("index.html", opt).unwrap();
            std::io::Write::write_all(
                &mut zw,
                b"\xEF\xBB\xBF<html><body>\xE4\xBD\xA0\xE5\xA5\xBD</body></html>",
            )
            .unwrap();
            zw.finish().unwrap();
        }
        let svc = ZipService::new(dir.clone());
        let html = svc.read_index_html(guid).unwrap();
        assert!(html.contains("你好"), "{html}");
        assert!(!html.starts_with('\u{feff}'), "BOM 应被剥离");
        assert!(svc.has_entry(guid, "index.html"));
        assert!(svc
            .read_index_html("00000000-0000-0000-0000-000000000000")
            .is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 造一个只有指定条目的笔记包（返回 ZipService 与目录）
    fn svc_with_entries(tag: &str, entries: &[(&str, &[u8])]) -> (ZipService, PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("wiz-zipserve-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let guid = "abcdef01-2345-6789-abcd-ef0123456789".to_string();
        {
            let f = std::fs::File::create(dir.join(format!("{{{guid}}}"))).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opt = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, bytes) in entries {
                zw.start_file(*name, opt).unwrap();
                std::io::Write::write_all(&mut zw, bytes).unwrap();
            }
            zw.finish().unwrap();
        }
        (ZipService::new(dir.clone()), dir, guid)
    }

    /// M3 核心：正文形态判定 —— 有 `note.md` 即 md 包（即使还躺着 index.html 也以 md 为准）
    #[test]
    fn test_body_format_by_package_entries() {
        let (svc, d, g) = svc_with_entries("fmt-native", &[("index.html", b"<p>x</p>")]);
        assert_eq!(svc.body_format(&g), BodyFormat::Html);
        std::fs::remove_dir_all(&d).unwrap();

        let (svc, d, g) = svc_with_entries("fmt-md", &[(crate::md::NOTE_MD, b"# t\n")]);
        assert_eq!(svc.body_format(&g), BodyFormat::Md);
        assert_eq!(BodyFormat::Md.entry(), "note.md");
        assert_eq!(BodyFormat::Md.tag(), "md");
        assert_eq!(BodyFormat::Html.entry(), "index.html");
        std::fs::remove_dir_all(&d).unwrap();

        // 两态同存（理论上不该有）→ md 优先，与写路径 package_body_format 同口径
        let (svc, d, g) = svc_with_entries(
            "fmt-both",
            &[("index.html", b"<p>old</p>"), (crate::md::NOTE_MD, b"# new\n")],
        );
        assert_eq!(svc.body_format(&g), BodyFormat::Md);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// M4：形态**短标识**只在正文条目确实存在时才给 —— 读不到包必须是 None，
    /// 不能把 `body_format` 的默认值（Html）当成"这是 HTML 笔记"报出去
    #[test]
    fn test_body_format_tag_is_none_when_body_missing() {
        let (svc, d, g) = svc_with_entries("tag-md", &[(crate::md::NOTE_MD, b"# t\n")]);
        assert_eq!(svc.body_format_tag(&g).as_deref(), Some("md"));
        std::fs::remove_dir_all(&d).unwrap();

        let (svc, d, g) = svc_with_entries("tag-html", &[("index.html", b"<p>x</p>")]);
        assert_eq!(svc.body_format_tag(&g).as_deref(), Some("html"));
        std::fs::remove_dir_all(&d).unwrap();

        // 包在，但两种正文条目都没有（损坏/半途中断的包）
        let (svc, d, g) = svc_with_entries("tag-none", &[("index_files/a.css", b"a{}")]);
        assert_eq!(svc.body_format_tag(&g), None);
        std::fs::remove_dir_all(&d).unwrap();

        // 包根本不存在（库外 guid）
        let svc = ZipService::none();
        assert_eq!(
            svc.body_format_tag("11111111-2222-3333-4444-555555555555"),
            None
        );
    }

    /// md 包：`read_note_body` 给 Markdown 源；`read_note_document` 渲染成 HTML 文档
    #[test]
    fn test_read_note_document_renders_md_package() {        let md = "# 标题\n\n正文 **粗**\n\n```sh\nls -l\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (svc, d, g) = svc_with_entries(
            "doc-md",
            &[(crate::md::NOTE_MD, md.as_bytes()), ("index_files/x.css", b"a{}")],
        );
        let (fmt, src) = svc.read_note_body(&g).unwrap();
        assert_eq!(fmt, BodyFormat::Md);
        assert_eq!(src, md, "源文本必须逐字节等于包内 note.md（BOM 除外）");

        let doc = svc.read_note_document(&g, &g).unwrap();
        assert!(doc.starts_with("<!DOCTYPE html>"), "渲染结果应是完整文档: {doc}");
        assert!(doc.contains("<h1>标题</h1>"), "{doc}");
        assert!(doc.contains("<strong>粗</strong>"), "{doc}");
        assert!(doc.contains("<pre><code class=\"language-sh\">ls -l"), "{doc}");
        assert!(doc.contains("<table>"), "GFM 表格应渲染成 <table>: {doc}");
        assert_eq!(svc.body_format(&g), BodyFormat::Md);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// native 包：`read_note_document` 必须**原样**返回 index.html（BOM 剥、不重渲染）
    #[test]
    fn test_read_note_document_native_passthrough() {
        let html = b"\xEF\xBB\xBF<html><body><p>hi</p></body></html>";
        let (svc, d, g) = svc_with_entries("doc-native", &[("index.html", html)]);
        let (fmt, src) = svc.read_note_body(&g).unwrap();
        assert_eq!(fmt, BodyFormat::Html);
        assert_eq!(src, "<html><body><p>hi</p></body></html>");
        assert_eq!(svc.read_note_document(&g, "t").unwrap(), src, "native 不做任何加工");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// 两个正文档名都没有 → 读正文必须报错（按形态走到的那个条目缺失）
    #[test]
    fn test_read_note_body_missing_both_entries() {
        let (svc, d, g) = svc_with_entries("doc-none", &[("index_files/a.css", b"a{}")]);
        // 无 note.md → 判为 native → 撞 index.html 缺失
        let err = svc.read_note_body(&g).unwrap_err();
        assert!(matches!(err, ZipError::NoSuchEntry(ref p) if p == "index.html"), "{err:?}");
        assert!(svc.read_note_document(&g, "t").is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
