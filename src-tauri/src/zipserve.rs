//! zip 流式读取服务（T0.2/T2.1）
//!
//! - 按需读取 zip 内条目，绝不落盘解压
//! - LRU 缓存已打开的 zip 句柄（容量 32）
//! - 路径穿越防护：请求路径规范化后必须能在该 zip 条目列表中精确命中
//! - Content-Type 按扩展名推断

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;

use lru::LruCache;
use std::num::NonZeroUsize;
use zip::ZipArchive;

const CACHE_CAP: usize = 32;

pub struct ZipService {
    /// notes 根目录（内含 {GUID} zip 包）
    notes_dir: Mutex<PathBuf>,
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
            notes_dir: Mutex::new(notes_dir),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_CAP).unwrap())),
        }
    }

    pub fn set_notes_dir(&self, dir: PathBuf) {
        *self.notes_dir.lock().unwrap() = dir;
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
        let inner = guid
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(guid);
        let valid = !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-');
        if !valid {
            return Err(ZipError::IllegalPath(guid.into()));
        }
        let note_name = format!("{{{}}}", inner);
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(a) = cache.get(&note_name) {
                return Ok(a.clone());
            }
        }
        let path = self.notes_dir.lock().unwrap().join(&note_name);
        if !path.is_file() {
            return Err(ZipError::NoSuchNote);
        }
        let file = File::open(&path).map_err(|e| ZipError::Io(e.to_string()))?;
        let archive = ZipArchive::new(file).map_err(|e| ZipError::Io(e.to_string()))?;
        let arc = std::sync::Arc::new(Mutex::new(archive));
        self.cache.lock().unwrap().put(note_name.clone(), arc.clone());
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

    /// 读取 index.html 并按 utf-8-sig 解码（G3：忽略 <meta charset>）
    pub fn read_index_html(&self, guid: &str) -> Result<String, ZipError> {
        let bytes = self.read_entry(guid, "index.html")?;
        Ok(decode_utf8_sig(&bytes))
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
