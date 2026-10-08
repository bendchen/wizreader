//! 对象存储抽象层（设计稿 §2 / D1）
//!
//! `sync.rs` 只依赖 [`ObjectStore`] trait，S3 细节全部关在这里：
//! - [`S3Store`]：rust-s3 0.37 实现（HEAD/GET/PUT/LIST/DELETE 五个操作）
//! - [`MemStore`]：内存替身，无网络单测用（设计稿 §13 测试计划）
//!
//! 上行策略（设计稿 D9 / N11，Q13 定案）：
//! - **单段 PUT + 对象级续传**，不做分片；
//! - **禁止整块缓冲**：`put_object(&[u8])` 会把整个文件读进内存（100 MB × 4 并发 = 400 MB），
//!   一律走 `put_object_stream_builder(key).execute_stream(&mut tokio::fs::File)` 流式路径（N11a）；
//! - MD5 **不上传时现算**——导出流程已在清单里自算过（`note.exported_md5`），直接作为
//!   `Content-MD5` 头（base64，传输层校验）与 `x-amz-meta-content-md5`（hex，续传比对键）带上，
//!   同一个哈希三职：完整性 / 续传键 / 清单一致性（N11b，单遍 I/O）；
//! - 返回 `PutStreamResponse`（不是 `ResponseData`）。

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use base64::Engine as _;
use md5::{Digest, Md5};

// ---------------------------------------------------------------- 数据结构

/// HEAD 结果（元数据键已规范化：小写、去 `x-amz-meta-` 前缀）
#[derive(Debug, Clone, Default)]
pub struct ObjectHead {
    pub key: String,
    pub size: u64,
    pub e_tag: Option<String>,
    pub metadata: HashMap<String, String>,
}

impl ObjectHead {
    /// 取规范化后的元数据，如 `h.meta("content-md5")` ← `x-amz-meta-content-md5`
    pub fn meta(&self, key: &str) -> Option<&String> {
        self.metadata.get(&key.to_lowercase())
    }
}

/// PUT 结果：字节数与该对象的 hex MD5（调用方写回清单/报告用）
#[derive(Debug, Clone, serde::Serialize)]
pub struct PutReport {
    pub bytes: u64,
    pub hex_md5: String,
}

/// 条件 PUT 的**前置条件**（§7.1 纵深防御）。
///
/// 存在的意义：清单 PUT 是"读—改—写"的最后一笔，若在"读到远端清单"与"写回远端清单"之间
/// 有**第二个写入端**插进来，无条件 PUT 会把它刚写的东西**静默回退**。带上前置条件后，
/// 服务端自己来决定写不写 —— 条件是服务端在**同一笔请求内**判的，客户端无窗口可言。
#[derive(Debug, Clone, Copy)]
pub enum Precondition<'a> {
    /// `If-Match: <etag>` —— 只当远端对象当前 ETag 仍是它时才写入（常规提交）
    Match(&'a str),
    /// `If-None-Match: *` —— 只当对象**不存在**时才写入（首次提交 / 建云端库）
    Absent,
}

/// 条件 PUT 的结果。`PreconditionFailed` = **412**，是**正常业务态**（= 「被别人抢先写了」），
/// 不是传输错误：调用方必须回滚本轮铸版并按"重试同步"处理，而不是当成失败上报了事。
#[derive(Debug, Clone)]
pub enum ConditionalPut {
    Written(PutReport),
    PreconditionFailed,
}

/// 服务端**不支持**条件写时返回的错误前缀（有些 S3 兼容实现会对 `If-Match` 回 501）。
/// 调用方据此**降级为无条件 PUT** 并记警告 —— 纵深防御不可用不该阻断正常同步。
pub const ERR_PRECONDITION_UNSUPPORTED: &str = "SYNC_PRECONDITION_UNSUPPORTED";

/// S3 连接参数（与 config::SyncSettings 解耦，凭据由调用方从钥匙串读出后传入）
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub path_style: bool,
    pub access_key: String,
    pub secret_key: String,
}

// ---------------------------------------------------------------- trait

#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// 对象存在返回 Some（404 → Ok(None)，与「对象不存在」是正常业务态的语义一致）
    async fn head(&self, key: &str) -> Result<Option<ObjectHead>, String>;
    /// 整读（仅用于小对象：manifest.db / index.db 合成物；笔记 zip 一律流式落盘见 down 接口）
    async fn get(&self, key: &str) -> Result<Vec<u8>, String>;
    /// 小对象整传（manifest.db 级别；内部自算 MD5）
    async fn put_bytes(
        &self,
        key: &str,
        data: &[u8],
        extra_meta: &[(String, String)],
    ) -> Result<PutReport, String>;
    /// 小对象**条件整传**（`If-Match` / `If-None-Match: *`，设计稿 §7.1 纵深防御）。
    ///
    /// - `Ok(ConditionalPut::Written(_))`：条件成立，已写入；
    /// - `Ok(ConditionalPut::PreconditionFailed)`：**412**，被别的写入端抢先 —— 正常业务态；
    /// - `Err(e)` 且 `e` 以 [`ERR_PRECONDITION_UNSUPPORTED`] 开头：服务端不支持条件写，
    ///   调用方应降级为 [`ObjectStore::put_bytes`]。
    async fn put_bytes_if(
        &self,
        key: &str,
        data: &[u8],
        pre: Precondition<'_>,
        extra_meta: &[(String, String)],
    ) -> Result<ConditionalPut, String>;
    /// 文件流式上传（N11a 禁止缓冲）。`hex_md5` 来自清单（导出时已算），不再读盘重算。
    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        hex_md5: &str,
        content_type: Option<&str>,
        extra_meta: &[(String, String)],
    ) -> Result<PutReport, String>;
    /// 前缀下的全部对象键（扁平键空间，无 delimiter）
    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, String>;
    async fn delete(&self, key: &str) -> Result<(), String>;
}

// ---------------------------------------------------------------- hex/base64 帮手

/// hex MD5 → base64（Content-MD5 头要求 RFC 1864 形式：原始 16 字节的 base64）
pub fn hex_md5_to_content_md5_b64(hex: &str) -> Result<String, String> {
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("非法 MD5 hex（应为 32 位十六进制）: {hex}"));
    }
    let mut raw = [0u8; 16];
    for i in 0..16 {
        raw[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(raw))
}

fn md5_hex(data: &[u8]) -> String {
    format!("{:x}", Md5::digest(data))
}

// ---------------------------------------------------------------- S3Store

pub struct S3Store {
    bucket: s3::bucket::Bucket,
    /// 真云方言适配（2026-09-20 Ceph RGW 实测，boto3 独立复核）：该实现把 `If-Match` 的值
    /// 与**存储的不带引号 ETag** 做字面比较 ⇒ RFC 的带引号形式永远 412（连正确 ETag 也是），
    /// 而**去引号形式语义完全正确**（错误 ETag 照样被拒）。false = 先按 RFC 引号形式发；
    /// 一旦「引号 412 → 去引号重试成功」，置 true，此后直接发去引号形式，省一次往返。
    /// 安全性：去引号重试**只在引号形式 412 之后**发生，且重试结果按原语义解释
    /// （2xx=写入 / 其余=维持 412 判定），不吞掉任何一次真冲突。
    if_match_unquoted: std::sync::atomic::AtomicBool,
}

impl S3Store {
    pub fn new(cfg: &S3Config) -> Result<Self, String> {
        // endpoint 规范化：rust-s3 的自定义 region 需要带 scheme
        let mut endpoint = cfg.endpoint.trim().trim_end_matches('/').to_string();
        if endpoint.is_empty() || cfg.bucket.trim().is_empty() {
            return Err("SYNC_CONFIG_INVALID: endpoint / bucket 不能为空".into());
        }
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            endpoint = format!("https://{endpoint}");
        }
        // NFR-3.3 兜底：明文 HTTP 仅允许本机 / 内网私有 IP（正式校验在 config 校验层，这里防直连绕过）
        if endpoint.starts_with("http://") {
            let host = endpoint
                .trim_start_matches("http://")
                .split(['/', ':'])
                .next()
                .unwrap_or("");
            if !crate::config::is_local_or_private_host(host) {
                return Err(format!(
                    "SYNC_INSECURE_ENDPOINT: 明文 HTTP 仅允许本机或内网私有 IP，公网地址必须 HTTPS（{host}）"
                ));
            }
        }
        let region = s3::Region::Custom {
            region: if cfg.region.is_empty() {
                "us-east-1".into()
            } else {
                cfg.region.clone()
            },
            endpoint,
        };
        let creds = s3::creds::Credentials::new(
            Some(cfg.access_key.as_str()),
            Some(cfg.secret_key.as_str()),
            None::<&str>,
            None::<&str>,
            None::<&str>,
        )
        .map_err(|e| format!("凭据构造失败: {e}"))?;
        let bucket = s3::bucket::Bucket::new(&cfg.bucket, region, creds)
            .map_err(|e| format!("连接构造失败: {e}"))?;
        // Bucket::new 返回 Box<Bucket>；with_path_style 消费 self 且同样返回 Box<Bucket>
        let bucket = if cfg.path_style { bucket.with_path_style() } else { bucket };
        Ok(Self {
            bucket: *bucket,
            if_match_unquoted: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn head(&self, key: &str) -> Result<Option<ObjectHead>, String> {
        // rust-s3 0.37：非 2xx 也走 Ok(…, status)，404 是正常业务态而非 Err
        let (h, status) = self
            .bucket
            .head_object(key)
            .await
            .map_err(|e| format!("HEAD {key} 失败: {e}"))?;
        match status {
            200 => {
                let mut metadata = HashMap::new();
                if let Some(map) = &h.metadata {
                    for (k, v) in map {
                        let key = k.to_lowercase();
                        let key = key.strip_prefix("x-amz-meta-").unwrap_or(&key).to_string();
                        metadata.insert(key, v.clone());
                    }
                }
                Ok(Some(ObjectHead {
                    key: key.to_string(),
                    size: h.content_length.unwrap_or(0).max(0) as u64,
                    e_tag: h.e_tag.clone(),
                    metadata,
                }))
            }
            404 => Ok(None),
            other => Err(format!("HEAD {key} 返回 {other}")),
        }
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, String> {
        let r = self
            .bucket
            .get_object(key)
            .await
            .map_err(|e| format!("GET {key} 失败: {e}"))?;
        match r.status_code() {
            200 => Ok(r.bytes().to_vec()),
            404 => Err(format!("SYNC_NOT_FOUND: {key}")),
            other => Err(format!("GET {key} 返回 {other}")),
        }
    }

    async fn put_bytes(
        &self,
        key: &str,
        data: &[u8],
        extra_meta: &[(String, String)],
    ) -> Result<PutReport, String> {
        let hex = md5_hex(data);
        let (status, rep) = self
            .put_stream_inner(
                key,
                &hex,
                None,
                None,
                extra_meta,
                Body::Bytes(data.to_vec()),
            )
            .await?;
        if !(200..300).contains(&status) {
            return Err(format!("PUT {key} 返回 {status}"));
        }
        Ok(rep)
    }

    async fn put_bytes_if(
        &self,
        key: &str,
        data: &[u8],
        pre: Precondition<'_>,
        extra_meta: &[(String, String)],
    ) -> Result<ConditionalPut, String> {
        let hex = md5_hex(data);
        // If-Match 方言（见 `if_match_unquoted` 字段注释）：已探测为「去引号」实现 ⇒ 直接发去引号值
        let unquoted_mode = self
            .if_match_unquoted
            .load(std::sync::atomic::Ordering::Relaxed);
        let adapted: Option<String> = match (&pre, unquoted_mode) {
            (Precondition::Match(e), true) => Some(e.trim_matches('"').to_string()),
            _ => None,
        };
        let pre_eff: Precondition<'_> = match (&adapted, &pre) {
            (Some(u), _) => Precondition::Match(u.as_str()),
            (None, Precondition::Match(e)) => Precondition::Match(*e),
            (None, Precondition::Absent) => Precondition::Absent,
        };
        let (status, rep) = self
            .put_stream_inner(
                key,
                &hex,
                None,
                Some(pre_eff),
                extra_meta,
                Body::Bytes(data.to_vec()),
            )
            .await?;
        // 引号形式吃了 412 ⇒ 试用「去引号」方言（一次）。重试结果按原语义解释：
        // 2xx = 写入成功（并记住方言）；其余一律**维持第一次的 412 判定**（保守，不吞真冲突）。
        let (status, rep) = if status == 412 && adapted.is_none() {
            if let Precondition::Match(etag) = &pre {
                let unq = etag.trim_matches('"');
                if unq != *etag {
                    match self
                        .put_stream_inner(
                            key,
                            &hex,
                            None,
                            Some(Precondition::Match(unq)),
                            extra_meta,
                            Body::Bytes(data.to_vec()),
                        )
                        .await
                    {
                        Ok((s2, r2)) if (200..300).contains(&s2) => {
                            self.if_match_unquoted
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            (s2, r2)
                        }
                        _ => (status, rep),
                    }
                } else {
                    (status, rep)
                }
            } else {
                (status, rep)
            }
        } else {
            (status, rep)
        };
        match status {
            s if (200..300).contains(&s) => Ok(ConditionalPut::Written(rep)),
            412 | 409 => Ok(ConditionalPut::PreconditionFailed),
            501 | 405 => Err(format!(
                "{ERR_PRECONDITION_UNSUPPORTED}: 服务端对条件 PUT 返回 {status}（未实现）"
            )),
            other => Err(format!("PUT {key}（条件）返回 {other}")),
        }
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        hex_md5: &str,
        content_type: Option<&str>,
        extra_meta: &[(String, String)],
    ) -> Result<PutReport, String> {
        let (status, rep) = self
            .put_stream_inner(
                key,
                hex_md5,
                content_type,
                None,
                extra_meta,
                Body::File(path.to_path_buf()),
            )
            .await?;
        if !(200..300).contains(&status) {
            return Err(format!("PUT {key} 返回 {status}"));
        }
        Ok(rep)
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, String> {
        let results = self
            .bucket
            .list(prefix.to_string(), None)
            .await
            .map_err(|e| format!("LIST {prefix} 失败: {e}"))?;
        let mut out = Vec::new();
        for r in results {
            for obj in r.contents {
                out.push(obj.key);
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        let r = self
            .bucket
            .delete_object(key)
            .await
            .map_err(|e| format!("DELETE {key} 失败: {e}"))?;
        if !(200..300).contains(&r.status_code()) {
            return Err(format!("DELETE {key} 返回 {}", r.status_code()));
        }
        Ok(())
    }
}

/// 上行 body：小对象整块（manifest.db 级别，内存可忽略）与文件流式（N11a 禁止缓冲）二选一
enum Body {
    Bytes(Vec<u8>),
    File(std::path::PathBuf),
}

impl S3Store {
    /// 统一的流式 PUT：组装 builder（Content-MD5 头 + 可选前置条件 + x-amz-meta-*）→ execute_stream。
    /// 单段上传（Q13/D9）。**返回 `(状态码, 报告)`**：非 2xx 不在这里判死 —— `412` 是条件 PUT
    /// 的正常业务态（"被抢先"），由调用方按业务分支处理；其余非 2xx 由调用方当错误。
    async fn put_stream_inner(
        &self,
        key: &str,
        hex_md5: &str,
        content_type: Option<&str>,
        pre: Option<Precondition<'_>>,
        extra_meta: &[(String, String)],
        body: Body,
    ) -> Result<(u16, PutReport), String> {
        let b64 = hex_md5_to_content_md5_b64(hex_md5)?;
        let mut req = self
            .bucket
            .put_object_stream_builder(key)
            .with_metadata("content-md5", hex_md5)
            .map_err(|e| format!("设置元数据失败: {e}"))?
            .with_header(
                http::HeaderName::from_static("content-md5"),
                b64.as_str(),
            )
            .map_err(|e| format!("设置 Content-MD5 失败: {e}"))?
            .with_content_type(content_type.unwrap_or("application/octet-stream"));
        match pre {
            Some(Precondition::Match(etag)) => {
                req = req
                    .with_header(http::header::IF_MATCH, etag)
                    .map_err(|e| format!("设置 If-Match 失败: {e}"))?;
            }
            Some(Precondition::Absent) => {
                req = req
                    .with_header(http::header::IF_NONE_MATCH, "*")
                    .map_err(|e| format!("设置 If-None-Match 失败: {e}"))?;
            }
            None => {}
        }
        for (k, v) in extra_meta {
            req = req
                .with_metadata(k, v)
                .map_err(|e| format!("设置元数据 {k} 失败: {e}"))?;
        }
        let raw = match body {
            Body::Bytes(data) => {
                let mut slice: &[u8] = &data;
                req.execute_stream(&mut slice).await
            }
            Body::File(path) => {
                let mut f = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| format!("SYNC_IO: 打开 {} 失败: {e}", path.display()))?;
                req.execute_stream(&mut f).await
            }
        };
        // ⚠️ rust-s3 对 **>=300 一律转成 `Err`**（`execute_stream` 内部就调 `error_from_response_data`），
        // 状态码藏在 `S3Error::HttpFailWithBody(code, body)` 里 —— **412 也不例外**。
        // 条件 PUT 的业务分支正是靠这个码，故这里必须把状态码**还原**出来交调用方判，
        // 不能顺手 `map_err` 成字符串（那样 412 会被降级成"传输失败"，回滚分支永不触发）。
        let (status, uploaded) = match raw {
            Ok(resp) => (resp.status_code(), resp.uploaded_bytes() as u64),
            Err(s3::error::S3Error::HttpFailWithBody(code, _)) => (code, 0),
            Err(e) => return Err(format!("PUT {key} 失败: {e}")),
        };
        Ok((
            status,
            PutReport {
                bytes: uploaded,
                hex_md5: hex_md5.to_string(),
            },
        ))
    }
}

// ---------------------------------------------------------------- MemStore（测试替身）

#[derive(Default)]
pub struct MemStore {
    objects: std::sync::Mutex<HashMap<String, MemObj>>,
    /// 测试钩子：下一次条件 PUT 强制返回 **412**（模拟"被别的写入端抢先"），用完即清
    force_412: std::sync::atomic::AtomicBool,
    /// 测试钩子：下一次条件 PUT 强制报"服务端不支持"（验降级路径），用完即清
    force_unsupported: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Default)]
struct MemObj {
    data: Vec<u8>,
    meta: HashMap<String, String>,
}

/// 单段 PUT 的 ETag 口径：`"<内容 hex MD5>"`（带引号，与 S3 / MinIO / 替身一致）。
/// 清单固定走单段上传，故这个口径在真实服务端上成立；分片上传的 ETag 形式不同，
/// 但我们的 PUT 全在 8 MiB 单段阈值之内（见 `store.rs` 头部策略说明）。
fn etag_of(data: &[u8]) -> String {
    format!("\"{}\"", md5_hex(data))
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
    /// 供测试断言内部状态
    pub fn len(&self) -> usize {
        self.objects.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 对象的 ETag（条件 PUT 测试用）
    pub fn etag(&self, key: &str) -> Option<String> {
        self.objects.lock().unwrap().get(key).map(|o| etag_of(&o.data))
    }
    /// 测试钩子：让**下一次**条件 PUT 返回 412（模拟被别的写入端抢先）
    pub fn force_precondition_failed(&self) {
        self.force_412
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    /// 测试钩子：让**下一次**条件 PUT 报"服务端不支持条件写"（验降级为无条件 PUT）
    pub fn force_precondition_unsupported(&self) {
        self.force_unsupported
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl ObjectStore for MemStore {
    async fn head(&self, key: &str) -> Result<Option<ObjectHead>, String> {
        let map = self.objects.lock().unwrap();
        Ok(map.get(key).map(|o| ObjectHead {
            key: key.to_string(),
            size: o.data.len() as u64,
            e_tag: Some(etag_of(&o.data)),
            metadata: o.meta.clone(),
        }))
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, String> {
        let map = self.objects.lock().unwrap();
        map.get(key)
            .map(|o| o.data.clone())
            .ok_or_else(|| format!("SYNC_NOT_FOUND: {key}"))
    }

    async fn put_bytes(
        &self,
        key: &str,
        data: &[u8],
        extra_meta: &[(String, String)],
    ) -> Result<PutReport, String> {
        let hex = md5_hex(data);
        let mut meta = HashMap::new();
        meta.insert("content-md5".to_string(), hex.clone());
        for (k, v) in extra_meta {
            meta.insert(k.to_lowercase(), v.clone());
        }
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), MemObj { data: data.to_vec(), meta });
        Ok(PutReport { bytes: data.len() as u64, hex_md5: hex })
    }

    async fn put_bytes_if(
        &self,
        key: &str,
        data: &[u8],
        pre: Precondition<'_>,
        extra_meta: &[(String, String)],
    ) -> Result<ConditionalPut, String> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.force_unsupported.swap(false, SeqCst) {
            return Err(format!(
                "{ERR_PRECONDITION_UNSUPPORTED}: 测试钩子：服务端未实现条件写"
            ));
        }
        if self.force_412.swap(false, SeqCst) {
            return Ok(ConditionalPut::PreconditionFailed);
        }
        let mut map = self.objects.lock().unwrap();
        let current = map.get(key).map(|o| etag_of(&o.data));
        let satisfied = match pre {
            Precondition::Match(etag) => current.as_deref() == Some(etag),
            Precondition::Absent => current.is_none(),
        };
        if !satisfied {
            return Ok(ConditionalPut::PreconditionFailed);
        }
        let hex = md5_hex(data);
        let mut meta = HashMap::new();
        meta.insert("content-md5".to_string(), hex.clone());
        for (k, v) in extra_meta {
            meta.insert(k.to_lowercase(), v.clone());
        }
        map.insert(key.to_string(), MemObj { data: data.to_vec(), meta });
        Ok(ConditionalPut::Written(PutReport {
            bytes: data.len() as u64,
            hex_md5: hex,
        }))
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        hex_md5: &str,
        _content_type: Option<&str>,
        extra_meta: &[(String, String)],
    ) -> Result<PutReport, String> {
        let data = std::fs::read(path).map_err(|e| format!("SYNC_IO: 读取 {} 失败: {e}", path.display()))?;
        // 不变量：调用方带来的 MD5 必须与实际内容一致（清单 exported_md5 与磁盘同源）
        let actual = md5_hex(&data);
        if !hex_md5.is_empty() && actual != hex_md5 {
            return Err(format!(
                "SYNC_MD5_MISMATCH: {key} 清单 {hex_md5} / 磁盘 {actual}"
            ));
        }
        let bytes = data.len() as u64;
        let mut meta = HashMap::new();
        meta.insert("content-md5".to_string(), actual.clone());
        for (k, v) in extra_meta {
            meta.insert(k.to_lowercase(), v.clone());
        }
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), MemObj { data, meta });
        Ok(PutReport { bytes, hex_md5: actual })
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, String> {
        let map = self.objects.lock().unwrap();
        let mut keys: Vec<String> = map
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        keys.sort();
        Ok(keys)
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str, content: &[u8]) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wiz-store-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("obj.bin");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn test_hex_to_content_md5_b64() {
        // md5("foo") = acbd18db4cc2f85cedef654fccc4a4d8
        let b64 = hex_md5_to_content_md5_b64("acbd18db4cc2f85cedef654fccc4a4d8").unwrap();
        assert_eq!(b64, "rL0Y20zC+Fzt72VPzMSk2A==");
        assert!(hex_md5_to_content_md5_b64("zz").is_err());
    }

    #[tokio::test]
    async fn test_memstore_roundtrip_and_skip_semantics() {
        let s = MemStore::new();
        // 404 语义：head → None
        assert!(s.head("notes/abc").await.unwrap().is_none());
        // put_bytes 自算 MD5
        let rep = s.put_bytes("manifest.db", b"hello", &[("revision".into(), "1".into())]).await.unwrap();
        assert_eq!(rep.hex_md5, md5_hex(b"hello"));
        // head 读回规范化元数据
        let h = s.head("manifest.db").await.unwrap().unwrap();
        assert_eq!(h.size, 5);
        assert_eq!(h.meta("content-md5").unwrap(), &md5_hex(b"hello"));
        assert_eq!(h.meta("revision").unwrap(), "1");
        // 对象级续传判定：MD5 一致 → skip
        let again = s.head("manifest.db").await.unwrap().unwrap();
        assert_eq!(again.meta("content-md5").unwrap(), &rep.hex_md5);
        // get
        assert_eq!(s.get("manifest.db").await.unwrap(), b"hello");
        // list
        assert_eq!(s.list_keys("manifest").await.unwrap(), vec!["manifest.db".to_string()]);
        // delete
        s.delete("manifest.db").await.unwrap();
        assert!(s.is_empty());
    }

    /// 条件 PUT 的三条语义（§7.1）：`Absent` 只在空位写、`Match` 只在 ETag 相符时写、
    /// 不符一律 412 且**内容不被改动**；并验两条测试钩子（412 / 不支持）。
    ///
    /// **变异检查**：把 `put_bytes_if` 的前置条件判据删掉（无条件写）→ 三条 `PreconditionFailed`
    /// 断言全部 FAILED。
    #[tokio::test]
    async fn test_memstore_conditional_put_semantics() {
        let s = MemStore::new();
        // Absent：对象不存在 ⇒ 写成功；已存在 ⇒ 412
        assert!(matches!(
            s.put_bytes_if("m.db", b"v1", Precondition::Absent, &[]).await.unwrap(),
            ConditionalPut::Written(_)
        ));
        assert!(matches!(
            s.put_bytes_if("m.db", b"v2", Precondition::Absent, &[]).await.unwrap(),
            ConditionalPut::PreconditionFailed
        ));
        assert_eq!(s.get("m.db").await.unwrap(), b"v1", "412 时内容不得被改动");

        // Match：ETag 相符 ⇒ 写成功；ETag 是别人的（旧的）⇒ 412
        let good = s.etag("m.db").unwrap();
        assert!(matches!(
            s.put_bytes_if("m.db", b"v2", Precondition::Match(&good), &[]).await.unwrap(),
            ConditionalPut::Written(_)
        ));
        assert!(matches!(
            s.put_bytes_if("m.db", b"v3", Precondition::Match(&good), &[]).await.unwrap(),
            ConditionalPut::PreconditionFailed
        ));
        assert_eq!(s.get("m.db").await.unwrap(), b"v2");
        // Match 一个根本不存在的对象 ⇒ 也是 412（不能凭空写）
        assert!(matches!(
            s.put_bytes_if("nope", b"x", Precondition::Match("\"deadbeef\""), &[]).await.unwrap(),
            ConditionalPut::PreconditionFailed
        ));

        // 钩子：412 与"服务端不支持"（后者必须能被调用方识别并降级）
        s.force_precondition_failed();
        assert!(matches!(
            s.put_bytes_if("m.db", b"v9", Precondition::Absent, &[]).await.unwrap(),
            ConditionalPut::PreconditionFailed
        ));
        s.force_precondition_unsupported();
        let e = s
            .put_bytes_if("m.db", b"v9", Precondition::Absent, &[])
            .await
            .unwrap_err();
        assert!(e.starts_with(ERR_PRECONDITION_UNSUPPORTED), "got: {e}");
    }

    #[tokio::test]
    async fn test_memstore_put_file_md5_mismatch_rejected() {
        let s = MemStore::new();
        let p = temp_file("mismatch", b"real-bytes");
        let wrong = md5_hex(b"other");
        let err = s.put_file("notes/g", &p, &wrong, None, &[]).await.unwrap_err();
        assert!(err.contains("SYNC_MD5_MISMATCH"), "got: {err}");
        let right = md5_hex(b"real-bytes");
        let rep = s.put_file("notes/g", &p, &right, None, &[]).await.unwrap();
        assert_eq!(rep.bytes, 10);
        assert_eq!(rep.hex_md5, right);
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn test_s3_endpoint_validation() {
        // 明文 HTTP 非本机 → 拒绝（NFR-3.3 / §4.3 兜底）
        let cfg = S3Config {
            endpoint: "http://minio.example.com:9000".into(),
            bucket: "wiznotes".into(),
            region: "us-east-1".into(),
            path_style: true,
            access_key: "a".into(),
            secret_key: "s".into(),
        };
        assert!(S3Store::new(&cfg).is_err());
        // 本机 / 内网 http 允许
        let cfg = S3Config { endpoint: "http://127.0.0.1:9000".into(), ..cfg };
        assert!(S3Store::new(&cfg).is_ok());
        let cfg = S3Config { endpoint: "http://192.168.1.10:7480".into(), ..cfg };
        assert!(S3Store::new(&cfg).is_ok(), "内网私有 IP 放行（局域网 Ceph）");
        // 无 scheme 补 https
        let cfg = S3Config { endpoint: "minio.example.com:9000".into(), ..cfg };
        assert!(S3Store::new(&cfg).is_ok());
    }
}
