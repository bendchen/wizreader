//! 云同步编排层（**判据口径 = `docs/云同步逻辑.md` v1.0 定稿**；实现文档 §26）
//!
//! **v6 的口径（蓝本 = 为知笔记实测同步模型）**：
//! **版本号只在"上行提交"时由唯一铸版者铸造；本地改动只置"脏闩"，绝不铸版。**
//! 上不上行看闩、下不下行看水位。三种量，再无第四种：
//! - **闩** `note.dirty_info` / `dirty_data`、`attachment.dirty`、墓碑 `synced_revision=0`
//!   → 决定**上行传谁**；写侧置、提交点/下行清。
//! - **水位** `meta.revision`（清单版本，只在提交点推进）+ `meta.{document,attachment,deleted}_watermark`
//!   + 每对象的 `synced_revision` → 决定**下行拉谁**。
//! - **内容指纹** `exported_md5` → 冲突判定与"内容是否已相同"。
//!
//! **判据禁读 `note.revision`**（v3 的行级计数器，v6 起降级为仅 UI）：它是唯一"两边各写各的"
//! 计数器，拿它当判据就是 D 轮两处真 bug（U5 清单级 rev、U4 按篇 rev）的原形 ——
//! 当时本地每写一次就把计数器顶高一格 ⇒ "本地改过一版"会被顶成"与远端持平" ⇒ 判据恒假。
//!
//! **两处有意偏离为知**（§4.7；为知有服务器仲裁者，我方没有）：
//! - **留档而不丢弃**：冲突中被淘汰的那一版旁置 `_conflicts/`，30 天后由启动任务 GC。
//! - **reader 冲突时远端胜出**：为知的客户端是对称的（谁提交谁赢），我方定义了只读端，
//!   它那一版**永远上不去** ⇒ 下载覆盖本地（2026-09-19 用户裁定），本地版旁置留档。
//!
//! 其它职责：
//! - 首次初始化按角色分流（§8.3）：写入端 = 全量上行（清单最后传）；只读端 = 拉清单 →
//!   原子替换 → 差量下行 → 建库索引；
//! - `_trash/` 与 `_conflicts/` GC、启动任务（§6.3/§6.4/§7 第 5 条）。
//!
//! **D0 口径**（导出只产 native，2026-09-17 全局约束）：
//! 导出格式只有 `native` 一种（源 zip 逐字节拷贝，无损），因此本模块**没有模式分支，
//! 也没有历史格式的兼容分支**：对象键里的 `native/` 段只是冻结的协议字面量（[`KEY_NATIVE`]），
//! 与「格式可选」无关；清单的 `export_mode` 是格式标识，仅用于导出复用判定。
//!
//! 硬约束（写死，见设计稿 §6.3）：
//! - 上传**流式**（N11a，禁止整块缓冲；下载对象 ≤ 41.6 MB 暂用整读，见 [`SYNC_DOWNLOAD_NOTE`]）；
//! - `manifest.db` **必须是本轮最后一个 PUT**（§5.3：提交点/事务边界）；
//! - 进程内 `AtomicBool` + 跨进程 `.sync.lock` 文件锁防重入（N4）；
//! - 同步状态写 `~/.wizreader/sync-state.json` 侧车文件（**不写进 manifest**——
//!   设计稿原写 meta.last_sync_at 会让本地清单与远端清单产生无意义漂移，实现时改为侧车）。
//! - **提交点失败必须回滚本地铸版**（见 `sync_up` 步骤 4）：本地"以为已发布"而云端没有，
//!   闩又已清零 ⇒ 这些改动会**永远不再上传**。回滚手段是"预存铸版前的清单字节并写回"。

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::time::SystemTime;

use futures::stream;
use futures::StreamExt;
use md5::{Digest, Md5};
use rusqlite::Connection;
use serde::Serialize;

use crate::config::{SyncSettings, ROLE_READER};
use crate::manifest;
use crate::store::{
    ConditionalPut, ObjectStore, Precondition, S3Config, S3Store, ERR_PRECONDITION_UNSUPPORTED,
};

/// `_trash/` 保留期（§6.4，做成常量便于测试注入）
pub const TRASH_RETENTION_DAYS: u64 = 30;

/// `_conflicts/` 保留期（v6，`docs/云同步逻辑.md` §7 第 5 条）：与 `_trash/` 同为 30 天。
/// 挂启动任务，每次启动扫一遍删超期留档。**窗口的意义**：留档是"被淘汰那一版"的唯一副本，
/// 在云端 LWW 裁决（§11）建成前，它是唯一的安全网。
pub const CONFLICT_RETENTION_DAYS: u64 = 30;

/// 单附件上限（Q13 约束：100 MB，N11d）。超限**照常上传**（单段 PUT 可到 5 GiB），
/// 仅计数进 `SyncReport.oversized` 并写日志——同步正确性不依赖该上限成立
pub const MAX_ATTACHMENT_BYTES: i64 = 100 * 1024 * 1024;

/// 下载侧内存说明（v1 取舍）：`ObjectStore::get` 整读对象。最大对象 41,627,325 B
/// （设计稿 D9），4 并发下瞬时可到 ~170 MB，可接受；后续如需压内存给 trait 加
/// `get_to_file`（rust-s3 有 `response_data_to_writer` 流式落盘接口）。
pub const SYNC_DOWNLOAD_NOTE: &str = "download-v1-uses-buffered-get";

/// 云端对象键的格式段：`{prefix}/native/notes/{GUID}`、`{prefix}/native/attachments/...`。
///
/// 这是**冻结的协议字面量**——键即协议，改动它会改变全部对象路径；它的存在**不代表**
/// 「格式可选」（D0 后只产 native，代码里没有任何按格式分支的逻辑）。
pub const KEY_NATIVE: &str = "native";

/// 冲突留存的库内保留目录（§3.1）：顶层 `_` 前缀 = 系统保留区，浏览侧不显示、
/// 自检孤儿扫描豁免、**不进清单也不进索引**（只读端不会读到它）。
pub const CONFLICT_DIR: &str = "_conflicts";

/// **v6 废止**：只读端护栏的基线（`meta.reader_base_revision`）。
///
/// 旧口径下只读端"本地脏则拒绝下行"，需要一个**不会被本地写动作碰到**的基线来回答
/// "自上次下行后库被写过吗"。v6 改了两件事，这个键就失去存在理由：
/// ① "本地改过没"改读**脏闩**（布尔量，写侧置 —— 天生不会被写动作推高，不需要基线）；
/// ② 只读端**不再阻断**下行（2026-09-19 用户裁定：本地只读、云端改了就直接下载更新）。
///
/// 保留常量只为**文档可比对**（升级前写下的老库键会残留，代码不再读它）；
/// `docs/云同步逻辑.md` §8 作废清单第 1 条。
#[deprecated(note = "v6 废止：改用脏闩，且只读端不再阻断下行（见 docs/云同步逻辑.md §4.4/§8）")]
pub const READER_BASE_REV: &str = "reader_base_revision";

/// 被淘汰对象在库内的留存相对路径：`_conflicts/{guid}_{revision}.zip`。
///
/// 带 revision 而非时间戳：revision 是**铸造号**（内容版本），同一篇被覆盖两次会得到两个
/// 不同文件名（多次冲突都留得下），而时间戳在同一毫秒内会互相覆盖。
///
/// `rev` 的**归属随角色走**（§4.7）：
/// - `writer` 冲突 → 本地胜出，留档的是**远端那一版** ⇒ `rev` = 远端 `synced_revision`；
/// - `reader` 冲突 → 远端胜出，留档的是**本地那一版** ⇒ 用 [`conflict_rel_path_local`]，
///   加 `_local` 后缀区分（两者同篇同号时会撞名）。
pub fn conflict_rel_path(guid: &str, remote_revision: i64) -> String {
    let g = guid.trim_matches(['{', '}']);
    format!("{CONFLICT_DIR}/{g}_{remote_revision}.zip")
}

/// reader 冲突留档：被覆盖的**本地版**（加 `_local` 后缀，与远端版留档区分）。
pub fn conflict_rel_path_local(guid: &str, local_revision: i64) -> String {
    let g = guid.trim_matches(['{', '}']);
    format!("{CONFLICT_DIR}/{g}_{local_revision}_local.zip")
}

// ---------------------------------------------------------------- 报告与进度

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncFailure {
    pub key: String,
    pub reason: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncReport {
    pub direction: String, // "up" | "down" | "none"
    pub uploaded: usize,
    pub uploaded_bytes: u64,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    /// 元数据一致 → 跳过（对象级续传命中）
    #[serde(default)]
    pub skipped: usize,
    /// U5 / v6：冲突中被淘汰那一版**旁置到 `_conflicts/`** 的篇数。
    /// `writer` 上行时 = 被本地顶掉的远端版；`reader` 下行时 = 被远端顶掉的本地版。
    #[serde(default)]
    pub conflicts: usize,
    /// v6：只读端下行**覆盖掉的本地脏**篇数（不阻断，只留痕；本地版已旁置 `_conflicts/*_local.zip`）。
    /// 与 `conflicts` 的关系：`conflicts` 是"总共旁置了几份"，这个是"其中因**只读端被改过**而产生的"。
    #[serde(default)]
    pub overwrote_dirty: usize,
    /// 移入 _trash 的篇数
    #[serde(default)]
    pub trashed: usize,
    /// tier=0（db-missing）等无文件可传的附件计数（N11d）
    #[serde(default)]
    pub oversized: usize,
    #[serde(default)]
    pub failures: Vec<SyncFailure>,
    pub manifest_uploaded: bool,
    pub remote_revision: Option<u64>,
    pub local_revision: u64,
    pub elapsed_ms: u128,
}

impl SyncReport {
    fn new(direction: &str) -> Self {
        Self {
            direction: direction.into(),
            uploaded: 0,
            uploaded_bytes: 0,
            downloaded: 0,
            downloaded_bytes: 0,
            skipped: 0,
            conflicts: 0,
            overwrote_dirty: 0,
            trashed: 0,
            oversized: 0,
            failures: Vec::new(),
            manifest_uploaded: false,
            remote_revision: None,
            local_revision: 0,
            elapsed_ms: 0,
        }
    }
}

/// 进度回调：`(phase, done, total)`，phase ∈ export/upload/manifest/download/index/trash
pub type ProgressFn = Arc<dyn Fn(&str, usize, usize) + Send + Sync>;

#[cfg(test)]
fn noop_progress() -> ProgressFn {
    Arc::new(|_, _, _| {})
}

// ---------------------------------------------------------------- 重入防护（N4）

static SYNC_RUNNING: AtomicBool = AtomicBool::new(false);

/// 进程内 + 跨进程双重防重入。`None` = 已有同步在跑（调用方直接跳过）。
pub struct SyncGuard {
    lock_file: Option<std::fs::File>,
    lock_path: PathBuf,
}

impl Drop for SyncGuard {
    fn drop(&mut self) {
        SYNC_RUNNING.store(false, Ordering::SeqCst);
        if let Some(f) = &self.lock_file {
            let _ = f.unlock(); // std 1.89+ File::unlock
        }
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// `Ok(None)` = 正忙；`Err` = 锁文件操作失败（不视为致命，仅降级为无跨进程锁）
pub fn try_acquire(local_root: &Path) -> Result<Option<SyncGuard>, String> {
    if SYNC_RUNNING.swap(true, Ordering::SeqCst) {
        return Ok(None);
    }
    std::fs::create_dir_all(local_root).map_err(|e| e.to_string())?;
    let path = local_root.join(".sync.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    let lock_file = match f.try_lock() {
        Ok(()) => Some(f),
        Err(_) => {
            // 拿不到跨进程锁：回滚进程内标记，跳过本轮
            SYNC_RUNNING.store(false, Ordering::SeqCst);
            return Ok(None);
        }
    };
    Ok(Some(SyncGuard {
        lock_file,
        lock_path: path,
    }))
}

// ---------------------------------------------------------------- 存储工厂

pub fn s3_store_of(cfg: &SyncSettings, secret: &str) -> Result<S3Store, String> {
    let s3cfg = S3Config {
        endpoint: cfg.endpoint.clone(),
        bucket: cfg.bucket.clone(),
        region: cfg.region.clone(),
        path_style: cfg.path_style,
        access_key: cfg.access_key_id.clone(),
        secret_key: secret.to_string(),
    };
    S3Store::new(&s3cfg)
}

/// 上行端标识（审计用；机器名哈希，不落原文）
fn machine_tag() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default();
    let mut h = Md5::new();
    h.update(host.as_bytes());
    let s = format!("{:x}", h.finalize());
    s[..12].to_string()
}

/// 云端 notes/{GUID} 的 GUID 形态：与源 zip 同名（**含花括号**，设计稿 §5.1）
fn braced(guid: &str) -> String {
    if guid.starts_with('{') {
        guid.to_string()
    } else {
        format!("{{{guid}}}")
    }
}

// ---------------------------------------------------------------- 上行（写入端）

/// 远端清单快照：只装判据真正需要的两样 —— 清单整体 `revision`（水位）与每篇的
/// `(synced_revision, exported_md5)`。
///
/// **v6 起装的是 `synced_revision` 而不是行级 `revision`**：前者是"这篇最后一次进云端的
/// 铸造号"，只用于**冲突副本定名**与**水位重算**，不作判据；后者已降级为本地计数器，
/// 远端值毫无意义（两边各写各的）。
struct RemoteManifest {
    revision: u64,
    notes: HashMap<String, (i64, String)>,
    /// 远端清单对象的 **ETag**（`head` 拿到）—— 条件 PUT `If-Match` 的基准（§7.1）
    etag: Option<String>,
}

/// 取远端清单快照（远端尚无清单 → `None`）。
///
/// 下载物落**机器自管区**（`wiz_home()` 下的暂存目录）而**不是库内**：R8 要求 `~/.wizreader`
/// 与库根严格分离（派生物不入库、不随库上云），而"远端清单长什么样"正是典型派生物。
///
/// 暂存目录按**库根 + 进程**唯一（[`crate::config::sync_stash_dir`]，与 `sync_down` 的护栏同一
/// 口径），用完即删。**不得写死一个共用路径**：同进程里两个库并发上行会互相读到对方的远端
/// 清单，症状是冲突检测静默失效（读到的清单里没有本篇）——并行跑的测试立刻就会踩到。
async fn fetch_remote_manifest(
    store: &dyn ObjectStore,
    cfg: &SyncSettings,
    root: &Path,
) -> Result<Option<RemoteManifest>, String> {
    let key = cfg.cloud_key("manifest.db");
    let Some(head) = store.head(&key).await? else {
        return Ok(None);
    };
    let revision = head
        .meta("revision")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let bytes = store.get(&key).await?;
    let dir = crate::config::sync_stash_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(manifest::MANIFEST_NAME), &bytes).map_err(|e| e.to_string())?;
    let conn = manifest::open_readonly(&dir)?;
    let revs = manifest::load_note_synced_revisions(&conn)?;
    let notes = manifest::load_notes(&conn)?
        .into_iter()
        .map(|(g, n)| {
            let r = revs.get(&g).copied().unwrap_or(0);
            (g, (r, n.exported_md5))
        })
        .collect();
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir); // 暂存只为本段服务，读完即清
    Ok(Some(RemoteManifest { revision, notes, etag: head.e_tag }))
}

/// 写临时文件 + `rename`（库内不留半截文件，与 §4.2 写路径三律同构）。
fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let tmp = dest.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
}

/// 上行（**U2 后：没有"先重新导出"这一级**）。
///
/// `root` = 库根（调用方传 `crate::config::Settings::sync_root()`）。
/// 库本身就是导出结果，故这里**直接扫库**组上传任务：这既是净简化，也是正确性要求 ——
/// 旧的 `reexport_if_needed` 走的是 native `export_folder_zips`，在 md 库上会把 native 包
/// 从源倒进库根，形态混杂且覆盖 md 包。
///
/// **v6 判据（`docs/云同步逻辑.md` §4.2）**：
/// 1. 待上行集合 **只看闩**（`dirty_*` + 未发布墓碑），不比 md5、不比 revision；
/// 2. 集合为空 ⇒ **不铸版、不提交**（空跑不得推进版本号，否则单调性会掺进空转噪声）；
/// 3. 铸版 `new_rev = meta.revision + 1`，与"清闩 + 记 `synced_revision` + 推水位"同处一个事务；
/// 4. **清单最后传**（提交点）；**PUT 失败必须回滚本地铸版** —— 否则"本地以为已发布"而云端
///    并没有，闩又已清零 ⇒ 这些改动会**永远不再上传**（静默丢数据）。回滚 = 把铸版前的清单
///    字节写回，一步到位（比逐行反演闩状态更可靠，也顺带覆盖"半途出错"的情况）。
/// 5. **提交前两道闸**（§7.1）：① 本地判据 `SYNC_MANIFEST_STALE`（本端基线是否已落后于远端）；
///    ② 条件 PUT `If-Match` / `If-None-Match: *`（读—写窗口内是否有人插队；未命中 → 回滚铸版）。
pub async fn sync_up(
    store: &dyn ObjectStore,
    cfg: &SyncSettings,
    root: &Path,
    progress: ProgressFn,
) -> Result<SyncReport, String> {
    let t0 = Instant::now();
    let manifest_file = manifest::manifest_path(root);
    if !manifest_file.exists() {
        return Err(
            "SYNC_NO_MANIFEST: 库清单 export.db 缺失 —— 同步根即库根，请先建立笔记库\
             （从为知笔记导入，或把已有导出根设为数据目录）"
                .into(),
        );
    }
    let mut report = SyncReport::new("up");

    let conn = manifest::open_and_migrate(root)?;
    let notes = manifest::load_notes(&conn)?;
    // 本端手上每篇的**铸造号**（`synced_revision`）= "这一版是从云端的哪一号来的"。
    // 冲突判定要拿它当"我基于的是哪一版"（见下方 `diverged` 的第三个条件）——
    // 它与"闩"是两样量（闩 = 改没改过，铸造号 = 基于哪一版），故单独取。
    let local_srevs = manifest::load_note_synced_revisions(&conn)?;
    report.local_revision = manifest::current_revision(&conn);

    // ——— 步骤 1：待上行集合 —— 只看闩（§4.2 第 1 步）———
    let unsynced: HashSet<String> = manifest::load_unsynced_note_guids(&conn)?;
    let unpublished_tombs: Vec<String> = manifest::load_unpublished_tombstone_guids(&conn)?;
    if !unsynced.is_empty() || !unpublished_tombs.is_empty() {
        let detail = manifest::load_unsynced_note_detail(&conn)?;
        let info_only = detail.iter().filter(|(_, i, d)| *i && !*d).count();
        crate::commands::append_sync_log(&format!(
            "上行差量（闩驱动）：待上行 {} 篇（其中仅元信息脏 {} 篇 ⇒ 零对象上传、只重传清单）、\
             未发布墓碑 {} 条",
            unsynced.len(),
            info_only,
            unpublished_tombs.len()
        ));
    }

    // ——— 冲突留档（writer 侧：**本地胜出**，把将被顶掉的"远端那一版"先取回旁置）———
    //
    // 判据 = 「**本地脏** ∧ 远端 md5 与本地上次发布的那一版分歧
    //        ∧ **远端 `synced_revision` > 本端 `synced_revision`**」。
    //
    // 为什么不再用"清单级/行级 revision 谁大"（D 轮旧口径）：`note.revision` 是两边各写各的
    // 计数器，"谁大"只反映"谁多改了一版"，与"谁改过"无关 —— U5 就栽在这里（两端都从 meta=4
    // 改到 5，远端也是 5 ⇒ 清单级判据对"双写"恒为假）。
    //
    // 第三个条件的来由（= §4.2 第 4 步「条件 PUT `If-Match`」在**提交前**的本地等价物）：
    // 单写者连续改同一篇时，"远端 md5" 就是**本端自己上次发上去的那一版** —— 只看
    // 「本地脏 ∧ md5 分歧」会把每一次后继编辑都判成冲突、把自己的旧版一份份塞进 `_conflicts/`，
    // 真冲突被噪声淹没。`远端 synced_revision > 本端 synced_revision` 恰好回答
    // "远端这一版**不是本端发上去的**吗"：是 ⇒ 别人写过 ⇒ 真冲突；否 ⇒ 只是本端改动的后继。
    // （`local_srevs` 只在提交点被 `mark_notes_synced` 推进，故本端发出去的号必然 ≈ 本端手上的号。）
    // 附件行（步骤 2 组任务要用；这里提前读一份，只为判"本轮到底有没有东西要提交"）
    let atts = manifest::load_attachments(&conn)?;
    let has_att_rows = atts.iter().any(|a| a.tier != 0);
    let remote = if unsynced.is_empty() && unpublished_tombs.is_empty() && !has_att_rows {
        // 笔记无闩、墓碑无未发布、附件无实体行 ⇒ 本轮**不铸版不提交** ⇒ 一次网络都不打
        None
    } else {
        fetch_remote_manifest(store, cfg, root).await?
    };
    report.remote_revision = remote.as_ref().map(|r| r.revision);

    // ——— 提交前置判据（§7.1 的**本地半边**）：本端清单是否仍然基于远端那一版？———
    //
    // 为什么不可省：清单 PUT 是**整份**写。若本端清单已落后于远端（第二写入端，或本端从旧备份
    // 恢复），PUT 上去不只覆盖自己该改的那几篇，还会把**远端其他行一并写回旧值**；而对象字节
    // 不会跟着回退 ⇒ 远端「清单行 ≠ 对象字节」的**永久脱节**（E 两轮闭环 §9b/§9c 实测：新机器
    // 一下行就 `verify-library --deep` 报「体积不符」，再同步也不自愈）。
    // 条件 PUT 挡的是"读—写窗口内的并发"，挡不住"本端基线本身就旧"，故这条判据必须单独存在。
    //
    // 只在 `meta.revision > 0`（已有基线）时判：`== 0` 是 `cloud-init writer` 的建库语义
    // （本端尚未与云端建立基线，本就以本端为准全量上行），不在射程内。
    // 远端 revision **小于**本端是允许的 —— 上一轮有人把远端写回退过，本端上行正好纠正回来。
    if let Some(rm) = &remote {
        let local_rev = manifest::current_revision(&conn);
        if local_rev > 0 && rm.revision > local_rev {
            return Err(format!(
                "SYNC_MANIFEST_STALE: 远端清单 revision（{}）> 本端基线（{}）——\
                 本端清单已落后，整份上行会把远端其他行写回旧值（**已在提交前拦下，云端未改动**）。\
                 请先下行对齐（`cloud-sync down`）再上行；若确认本端才是对的，\
                 请显式以本端重建云端基线（`cloud-init writer`）。",
                rm.revision, local_rev
            ));
        }
    }

    if let Some(rm) = &remote {
        let mut diverged: Vec<(String, i64)> = Vec::new();
        for (g, n) in &notes {
            if !unsynced.contains(g) {
                continue; // 本地没改过 ⇒ 上行不会顶掉任何东西
            }
            let Some((rrev, rmd5)) = rm.notes.get(g) else {
                continue; // 远端没有这一篇 ⇒ 我们是新增，不存在"覆盖远端"
            };
            let lrev = local_srevs.get(g).copied().unwrap_or(0);
            if *rrev > lrev && rmd5 != &n.exported_md5 {
                diverged.push((g.clone(), *rrev));
            }
        }
        for (guid, rrev) in &diverged {
            let key = cfg.cloud_key(&format!("{KEY_NATIVE}/notes/{}", braced(guid)));
            let rel = conflict_rel_path(guid, *rrev);
            match store.get(&key).await {
                Ok(bytes) => match write_atomic(&root.join(&rel), &bytes) {
                    Ok(()) => report.conflicts += 1,
                    Err(e) => report.failures.push(SyncFailure {
                        key: key.clone(),
                        reason: format!("冲突副本落盘失败: {e}"),
                        retryable: true,
                    }),
                },
                Err(e) => report.failures.push(SyncFailure {
                    key: key.clone(),
                    reason: format!("冲突副本下载失败: {e}"),
                    retryable: true,
                }),
            }
        }
        if report.conflicts > 0 {
            crate::commands::append_sync_log(&format!(
                "冲突留档 {} 篇 → {CONFLICT_DIR}/（writer：本地胜出，远端版旁置；\
                 判据：**本地脏 ∧ md5 分歧**；远端清单 revision {} / 本地 {}）",
                report.conflicts, rm.revision, report.local_revision
            ));
        }
    }

    // ——— 步骤 2：组装上传任务（笔记 = 闩驱动；附件 = FR-07.5 整目录差集，行为不变）———
    #[derive(Clone)]
    enum UpKind {
        /// 笔记：`(guid)`
        Note(String),
        /// 附件：`(file_path, 云端相对键)`
        Attachment(String, String),
    }
    struct UpTask {
        key: String,
        path: PathBuf,
        hex_md5: String,
        ct: Option<&'static str>,
        kind: UpKind,
    }
    let mut tasks: Vec<UpTask> = Vec::new();

    for g in &unsynced {
        let Some(n) = notes.get(g) else {
            continue; // 闩在而行不在（并发删除）—— 下一轮墓碑会带走它
        };
        if n.exported_size > MAX_ATTACHMENT_BYTES {
            report.oversized += 1;
        }
        tasks.push(UpTask {
            key: cfg.cloud_key(&format!("{KEY_NATIVE}/notes/{}", braced(g))),
            path: root.join(&n.exported_path),
            hex_md5: n.exported_md5.clone(),
            ct: Some("application/zip"),
            kind: UpKind::Note(g.clone()),
        });
    }

    // 附件（**云端键**分段：Tier1–3 → attachments/；Tier4 → _unlinked_attachments/；
    // 本地落盘见 attach_local_path；tier0 仅元数据跳过）—— `atts` 已在上面读过（判"有无可提交项"用）
    let att_paths: Vec<PathBuf> = atts
        .iter()
        .filter(|a| a.tier != 0)
        .map(|a| attach_local_path(root, &a.file_path))
        .collect();
    // 附件不在清单里带 MD5 → 现算（总量 ~14 MB，spawn_blocking 一次性完成）
    let md5s: HashMap<String, String> = crate::commands::spawn_blocking(move || {
        let mut m = HashMap::new();
        for p in att_paths {
            if let Ok(h) = manifest::md5_file(&p) {
                m.insert(p.to_string_lossy().to_string(), h);
            }
        }
        m
    })
    .await
    .map_err(|e| e.to_string())?;
    for a in &atts {
        if a.tier == 0 {
            report.oversized += 1; // N11(d)：无文件可传的元数据行（db-missing），仅计数
            continue;
        }
        if a.size > MAX_ATTACHMENT_BYTES {
            report.oversized += 1;
        }
        let seg = match a.file_path.strip_prefix("attachments/") {
            Some(s) => s.to_string(),
            None => a.file_path.clone(),
        };
        let cloud_rel = if a.tier == 4 {
            format!("{KEY_NATIVE}/_unlinked_attachments/{seg}")
        } else {
            format!("{KEY_NATIVE}/attachments/{seg}")
        };
        let local = attach_local_path(root, &a.file_path);
        let Some(hex) = md5s.get(local.to_string_lossy().as_ref()) else {
            report.failures.push(SyncFailure {
                key: cloud_rel,
                reason: format!("本地附件缺失: {}", local.display()),
                retryable: false,
            });
            continue;
        };
        tasks.push(UpTask {
            key: cfg.cloud_key(&cloud_rel),
            path: local,
            hex_md5: hex.clone(),
            ct: None,
            kind: UpKind::Attachment(a.file_path.clone(), cloud_rel),
        });
    }

    // ——— 第二级：HEAD 比对 → 差集上传（并发 D6）———
    let total = tasks.len();
    progress("upload", 0, total);
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let results = stream::iter(tasks.into_iter().map(|t| {
        let done = done.clone();
        let progress = progress.clone();
        async move {
            // 对象级续传：远端已有且 content-md5 一致 → skip（**不是判据**，只是省一次 PUT）
            let skip = match store.head(&t.key).await {
                Ok(Some(h)) => h.meta("content-md5") == Some(&t.hex_md5),
                Ok(None) => false,
                Err(e) => return Err((t.key.clone(), format!("HEAD 失败: {e}"), t.kind.clone())),
            };
            let n = done.fetch_add(1, Ordering::SeqCst) + 1;
            progress("upload", n, total);
            if skip {
                return Ok((false, 0u64, t.kind.clone()));
            }
            let rep = store
                .put_file(&t.key, &t.path, &t.hex_md5, t.ct, &[])
                .await
                .map_err(|e| (t.key.clone(), e, t.kind.clone()))?;
            Ok((true, rep.bytes, t.kind.clone()))
        }
    }))
    .buffer_unordered(cfg.concurrency.max(1) as usize)
    .collect::<Vec<_>>()
    .await;

    let mut uploaded_bytes = 0u64;
    // 本轮**予以登记为已发布**的集合：上传成功 与 "内容已与云端一致"（skip）都算 ——
    // 两者都意味着"云端现在有这一版"。失败的不进集合，闩保持置位，下一轮重试。
    let mut synced_notes: HashSet<String> = unsynced.clone();
    let mut synced_atts: Vec<(String, String)> = Vec::new();
    // 真正发生内容变化的附件（用来判断"这一轮到底有没有东西要提交"）——
    // 不能拿 `synced_atts` 当这个判据：附件每轮都全量做差集，skip 的会被记进 synced_atts，
    // 于是"没有任何变化"也会被读成"有变化"，版本号每轮空转、水位失去意义。
    let mut uploaded_atts = 0usize;
    for r in results {
        match r {
            Ok((true, bytes, kind)) => {
                report.uploaded += 1;
                uploaded_bytes += bytes;
                if let UpKind::Attachment(fp, key_rel) = kind {
                    uploaded_atts += 1;
                    synced_atts.push((fp, key_rel));
                }
            }
            Ok((false, _, kind)) => {
                report.skipped += 1;
                if let UpKind::Attachment(fp, key_rel) = kind {
                    synced_atts.push((fp, key_rel));
                }
            }
            Err((key, reason, kind)) => {
                report.failures.push(SyncFailure {
                    key,
                    reason,
                    retryable: true,
                });
                if let UpKind::Note(g) = kind {
                    synced_notes.remove(&g); // 没上去 ⇒ 闩不许清
                }
            }
        }
    }
    report.uploaded_bytes = uploaded_bytes;

    // 附件 cloud_key 回填（键在组任务时已推出，直接写）
    for (file_path, key_rel) in &synced_atts {
        let _ = manifest::set_attachment_cloud_key(
            &conn,
            file_path,
            Some(&cfg.cloud_key(key_rel)),
        );
    }

    // ——— 步骤 3/4：铸版 + 清闩 + 提交点 ———
    let changed =
        !synced_notes.is_empty() || !unpublished_tombs.is_empty() || uploaded_atts > 0;
    if !changed {
        // 空跑：**不铸版、不提交**（§4.2 第 3 步的"集合为空不铸版"）
        report.direction = "none".into();
        report.elapsed_ms = t0.elapsed().as_millis();
        persist_state(&report)?;
        return Ok(report);
    }

    // 预存铸版前的清单字节：PUT 失败时写回 = 把本轮铸版整体撤销
    let pre_mint = std::fs::read(&manifest_file).map_err(|e| e.to_string())?;
    let now_utc = manifest::format_utc(SystemTime::now());
    let mut note_guids: Vec<String> = synced_notes.into_iter().collect();
    note_guids.sort();
    let att_files: Vec<String> = synced_atts.iter().map(|(f, _)| f.clone()).collect();
    {
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let rev = manifest::bump_revision(&tx, &now_utc)?;
        manifest::mark_notes_synced(&tx, &note_guids, rev)?;
        manifest::mark_attachments_synced(&tx, &att_files, rev)?;
        manifest::mark_tombstones_published(&tx, &unpublished_tombs, rev)?;
        manifest::recompute_watermarks(&tx)?;
        tx.commit().map_err(|e| e.to_string())?;
        report.local_revision = rev;
    }
    crate::commands::append_sync_log(&format!(
        "铸版 revision={}（笔记 {} 篇 / 附件 {} 个 / 墓碑 {} 条）",
        report.local_revision,
        note_guids.len(),
        att_files.len(),
        unpublished_tombs.len()
    ));

    // ——— 提交点：manifest.db 最后传（§5.3），带**条件 PUT**（§7.1 纵深防御）———
    progress("manifest", 0, 1);
    let bytes = std::fs::read(&manifest_file).map_err(|e| e.to_string())?;
    let manifest_key = cfg.cloud_key("manifest.db");
    let put_meta = [
        ("revision".to_string(), report.local_revision.to_string()),
        ("updated-by".to_string(), machine_tag()),
    ];
    // 基准 ETag = 本轮**读到**远端清单时它的 ETag（读—改—写里"读"的那一版）。条件是服务端在
    // **同一笔请求里**判的，客户端没有窗口可言；用"读"时的 ETag 而非提交前现取，射程覆盖
    // 从读远端行到 PUT 的**整个组装与上传窗口**。本轮没读过（远端无清单，或只是落墓碑/附件）
    // ⇒ 现 HEAD 一次；对象不存在 ⇒ `If-None-Match: *`（首次提交）；拿不到 ETag ⇒ 退回无条件。
    let head_now = if remote.is_some() {
        None
    } else {
        store.head(&manifest_key).await?
    };
    let base_etag: Option<String> = match &remote {
        Some(rm) => rm.etag.clone(),
        None => head_now.as_ref().and_then(|h| h.e_tag.clone()),
    };
    let precond = if remote.is_none() && head_now.is_none() {
        Some(Precondition::Absent) // 远端还没有清单 ⇒ 首次提交，别和别人撞车
    } else {
        base_etag.as_deref().map(Precondition::Match)
    };
    let put = match precond {
        Some(p) => match store.put_bytes_if(&manifest_key, &bytes, p, &put_meta).await {
            Ok(ConditionalPut::Written(rep)) => Ok(rep),
            Ok(ConditionalPut::PreconditionFailed) => Err(
                "SYNC_MANIFEST_CONFLICT: 远端清单在本轮提交期间被其他写入端改写\
                 （条件 PUT 未命中）"
                    .to_string(),
            ),
            Err(e) if e.starts_with(ERR_PRECONDITION_UNSUPPORTED) => {
                // 纵深防御不可用**不该阻断正常同步**：降级为无条件 PUT，并在同步日志留痕
                crate::commands::append_sync_log(&format!(
                    "⚠️ 条件 PUT 不可用，已降级为无条件清单上传（§7.1 纵深防御失效）: {e}"
                ));
                store.put_bytes(&manifest_key, &bytes, &put_meta).await
            }
            Err(e) => Err(e),
        },
        None => store.put_bytes(&manifest_key, &bytes, &put_meta).await,
    };
    if let Err(e) = put {
        drop(conn); // 写回文件前先放手，免得连接里留着旧页缓存
        let _ = write_atomic(&manifest_file, &pre_mint);
        return Err(format!(
            "上传清单失败（**已回滚本轮铸版**：闩恢复为待上行，下次同步会重试）: {e}"
        ));
    }
    report.manifest_uploaded = true;
    progress("manifest", 1, 1);

    drop(conn);
    report.elapsed_ms = t0.elapsed().as_millis();
    persist_state(&report)?;
    Ok(report)
}

/// 附件在**本机**的物理路径（上行读、下行写两个方向共用同一判据）。
///
/// A1' 后库内附件一律落**系统保留区**（`_attachments/…`；Tier4 源布局本就在
/// `_unlinked_attachments/…`，原样保留），两侧靠 [`manifest::disk_rel_path`] 互转。
///
/// **U1 起不再有第二套判据**：同步根 = 库根，"按源布局读附件"的分支随"源不再是同步根"
/// 一并消失（旧 `attach_local_path(root, data_dir, …)` 的 `data_dir` 参数已删）——
/// 留着它只会让"在源布局与库布局之间选一个"重新变成可能，而那正是 A1' 要消灭的歧义。
///
/// 别把本地布局与**云端对象键**混起来：后者始终按源相对口径推导
/// （`{prefix}/native/attachments/…`，见 [`KEY_NATIVE`]），本地布局变化不影响键。
fn attach_local_path(root: &Path, file_path: &str) -> PathBuf {
    root.join(manifest::disk_rel_path(file_path))
}

// ---------------------------------------------------------------- 下行（只读端 / 换机）

// ---------------------------------------------------------------- 下行（只读端 / 换机）

/// 与远端**内容或落点**不一致的本地篇目（含本地已删）—— **只用于报告归因**。
///
/// **不是判据**（v6 的判据只有三种量：闩 / 水位 / md5，见 `docs/云同步逻辑.md` §6.1）。
/// 它只回答"哪些篇目的本地版与远端版不一样"，供覆盖时把"具体动了哪几篇"写进日志 ——
/// 免得用户只看到"覆盖了 3 篇"却说不出是哪三篇。
///
/// 旧口径里这里还有一个 `detect_local_modifications`（拿"行级 rev 也不落后"当护栏判据）。
/// v6 一并删除：① 只读端**不再阻断**下行，判据本身没了用处；② "本地改过没"改由**脏闩**
/// 回答，比"md5 不同 ∧ 行级 rev 不落后"这种复合推断可靠得多（后者要求两个计数器可比，
/// 而它们两边各写各的 —— 正是 D 轮 U4 栽的地方）。
fn diverged_note_guids(
    local_notes: &HashMap<String, manifest::ManifestNote>,
    local_tombstones: &HashSet<String>,
    remote_conn: &Connection,
) -> Result<Vec<String>, String> {
    let remote_notes = manifest::load_notes(remote_conn)?;
    let remote_tombstones = manifest::load_tombstone_guids(remote_conn)?;
    let mut out = Vec::new();
    for (g, n) in local_notes {
        match remote_notes.get(g) {
            Some(r) => {
                if r.exported_md5 != n.exported_md5 || r.exported_path != n.exported_path {
                    out.push(g.clone());
                }
            }
            None => {
                if !remote_tombstones.contains(g) {
                    out.push(g.clone()); // 本地有、远端没有且无墓碑
                }
            }
        }
    }
    for g in local_tombstones {
        if remote_notes.contains_key(g) && !remote_tombstones.contains(g) {
            out.push(g.clone()); // 本地删了、远端还带着
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// 下行（只读端 / 换机）。
///
/// **v6 判据（`docs/云同步逻辑.md` §4.3）**：
/// 1. 有没有更新看**水位**（`remote.revision > meta.revision`，1 次 HEAD）；
/// 2. 逐篇差分：**"内容同不同"用 md5**、**"本地改过没"用闩**；
/// 3. **下行不铸版** —— 照为知，客户端下行只是接受权威水位，不产生新版本
///    （`meta.revision` 对齐远端；`meta.*_watermark` 从远端清单里的 `synced_revision` 重算）；
/// 4. **冲突按角色定向**（§4.7）：`writer` → 本地胜出（远端版旁置、本地行回写并置脏）；
///    `reader` → **远端胜出**（下载覆盖、本地版旁置）。两边都**不丢弃**被淘汰的那一版。
pub async fn sync_down(
    store: &dyn ObjectStore,
    cfg: &SyncSettings,
    root: &Path,
    progress: ProgressFn,
) -> Result<SyncReport, String> {
    let t0 = Instant::now();
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let mut report = SyncReport::new("down");
    let manifest_key = cfg.cloud_key("manifest.db");

    // 1. HEAD 清单比对**水位**（开销 = 1 次 HEAD）
    let head = store
        .head(&manifest_key)
        .await?
        .ok_or("SYNC_NO_MANIFEST: 远端清单不存在（远端可能尚未初始化）")?;
    let remote_rev = head
        .meta("revision")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    report.remote_revision = Some(remote_rev);
    report.local_revision = local_revision(root).await;

    if root.join(manifest::MANIFEST_NAME).exists() && remote_rev <= report.local_revision {
        report.direction = "none".into();
        report.elapsed_ms = t0.elapsed().as_millis();
        persist_state(&report)?;
        return Ok(report); // 无更新（§4.3 步骤 1）
    }

    // 2. 拉清单 → 校验 MD5
    //
    // v6 起**没有"替换前的只读端护栏"**了。旧口径要在替换前拦下"本地被改过"的只读端
    // （`SYNC_READER_DIRTY` 阻断）；v6 裁定（2026-09-19 用户）：**本地只读、云端改了就直接
    // 下载更新** —— "脏则拒"会把一次误改变成该端**永久无法同步**，护栏变成枷锁。
    // 代价由"旁置留档"承担：被覆盖掉的本地版照样存进 `_conflicts/*_local.zip`，不静默丢。
    let bytes = store.get(&manifest_key).await?;
    let expect = head.meta("content-md5").cloned().unwrap_or_default();
    let actual = format!("{:x}", Md5::digest(&bytes));
    if !expect.is_empty() && expect != actual {
        return Err(format!(
            "SYNC_MD5_MISMATCH: 清单远端 {expect} / 下载 {actual}"
        ));
    }

    // 2b. **替换前**先把"本地这一版是什么"记下来 —— 清单被远端内容整体覆盖之后就读不到了。
    //     三样都要：md5/落点（差分）、**脏闩**（冲突判定）、`synced_revision`（留档定名/水位）。
    //     还有本地墓碑：区分"远端删过这篇"与"远端从没见过这篇"（两者在 note 表里都表现为"没有"，
    //     但同步语义相反 —— 前者是正常删除传播，后者是本地新增）。
    struct LocalPrev {
        notes: HashMap<String, manifest::ManifestNote>,
        unsynced: HashSet<String>,
        synced_revs: HashMap<String, i64>,
        tombstones: HashSet<String>,
    }
    let local_prev: Option<LocalPrev> = manifest::open_readonly(root).ok().map(|c| LocalPrev {
        notes: manifest::load_notes(&c).unwrap_or_default(),
        unsynced: manifest::load_unsynced_note_guids(&c).unwrap_or_default(),
        synced_revs: manifest::load_note_synced_revisions(&c).unwrap_or_default(),
        tombstones: manifest::load_tombstone_guids(&c).unwrap_or_default(),
    });
    let empty_prev = LocalPrev {
        notes: HashMap::new(),
        unsynced: HashSet::new(),
        synced_revs: HashMap::new(),
        tombstones: HashSet::new(),
    };
    let lp = local_prev.as_ref().unwrap_or(&empty_prev);

    // 2c. 远端清单落**暂存**目录（`load_notes` 只认文件），读成内存后即清。
    //     暂存路径必须按「库根 + 进程」唯一（`sync_stash_dir`）：写死一个共用路径会让同进程里
    //     两个库并发下行互相读到对方的清单 —— 症状是"本地新增"被误判（另一库清单里当然没有这篇）。
    let stash = crate::config::sync_stash_dir(root);
    std::fs::create_dir_all(&stash).map_err(|e| e.to_string())?;
    std::fs::write(stash.join(manifest::MANIFEST_NAME), &bytes).map_err(|e| e.to_string())?;
    let rconn = manifest::open_readonly(&stash)?;
    let remote_notes = manifest::load_notes(&rconn)?;
    let remote_srevs = manifest::load_note_synced_revisions(&rconn)?;
    let diverged = diverged_note_guids(&lp.notes, &lp.tombstones, &rconn)?;
    drop(rconn);
    let _ = std::fs::remove_dir_all(&stash);

    // 2d. **冲突判定：按角色定向**（§4.7）。判据：`本地脏 ∧ md5 分歧`；
    //     writer 侧**再加一条** `远端 synced_revision > 本端 synced_revision`。
    //     ① `md5 一致` ⇒ 内容本来相同，只是元数据/落点落后 ⇒ 不算冲突；
    //     ② `¬本地脏` ⇒ 本地只是**落后**（没人改过它）⇒ 正常覆盖，也不算冲突；
    //     ③ writer 的额外条件是「远端那一版**不是本端发上去的**」—— 即 §4.2 第 4 步
    //        条件 PUT（`If-Match`）在本地侧的等价物。`rrev <= lrev` ⇒ 远端内容就是本端
    //        上次发布的那一版，本次只是它的**后继** ⇒ 不是冲突（少了这条，单写者每改一遍
    //        就把自己的旧版塞一份进 `_conflicts/`）。
    //     reader 侧**不加**第③条：它那一版永远上不去 ⇒ 任何本地改动都必须旁置留档（§4.4），
    //        覆盖之前先保副本，与"远端前进没前进"无关。
    let is_reader = cfg.role == ROLE_READER;
    // (guid, 远端 synced_revision)：真冲突 ⇒ 除"保本地"外还要**旁置远端版**
    let mut writer_conflicts: Vec<(String, i64)> = Vec::new();
    // writer 侧"本地脏且分歧"的篇目：**一律保本地、本轮不下载**（本次未上行的改动不能被盖掉）。
    // 它是 `writer_conflicts` 的超集 —— 后继式改动也要挡住下载，否则"清单回写 + 重置闩"
    // 两步都会缺位，磁盘与清单当场脱节。
    let mut writer_local_wins: Vec<String> = Vec::new();
    // (guid, 本地 synced_revision)：reader 覆盖前留档**本地版**
    let mut reader_overwrites: Vec<(String, i64)> = Vec::new();
    for (g, n) in &lp.notes {
        let Some(rmd5) = remote_notes.get(g).map(|r| r.exported_md5.as_str()) else {
            continue; // 远端没有：删除传播 / 只在远端看不到，不构成覆盖冲突
        };
        if rmd5 == n.exported_md5 {
            continue;
        }
        if !lp.unsynced.contains(g) {
            continue;
        }
        let rrev = remote_srevs.get(g).copied().unwrap_or(0);
        let lrev = lp.synced_revs.get(g).copied().unwrap_or(0);
        if is_reader {
            reader_overwrites.push((g.clone(), lrev));
        } else {
            writer_local_wins.push(g.clone());
            if rrev > lrev {
                writer_conflicts.push((g.clone(), rrev));
            }
        }
    }

    // 2e. 原子替换清单（临时文件 + 校验 + rename，避免读到半截库）
    let manifest_file = manifest::manifest_path(root);
    write_atomic(&manifest_file, &bytes)?;
    report.downloaded_bytes += bytes.len() as u64;

    let conn = manifest::open_and_migrate(root)?;
    // 4'. 对齐水位（§4.3 第 4 步）：下行**不铸版**，只接受远端水位。
    //     为什么清单版本可以直接对齐：它是"本端清单的版本"，下行后本端清单**就是**远端那一份。
    //     而 `synced_revision` / 闩都随清单内容一起照抄 —— 这正是"把权威水位搬过来"的含义。
    manifest::set_meta(&conn, "revision", &remote_rev.to_string())?;
    manifest::recompute_watermarks(&conn)?;
    let notes = manifest::load_notes(&conn)?;

    // 3. 冲突留档的**文件操作**（都必须在下载覆盖之前完成，否则本地版已经没了）
    //
    // `local_keep` = writer 侧「本地版胜出、本轮不下载」的篇目（见上文 `writer_local_wins`）。
    // 下载/回收/清单回写三处**都**以它为准：真冲突还要额外旁置远端版，而"后继式本地改动"
    // 只需"别动本地"——两者都**不能**被远端内容盖掉。
    let local_keep: HashSet<String> = writer_local_wins.iter().cloned().collect();
    for (guid, rrev) in &writer_conflicts {
        // writer 侧：本地胜出 ⇒ 把**远端那一版**取回来旁置（本地文件原地不动）
        let key = cfg.cloud_key(&format!("{KEY_NATIVE}/notes/{}", braced(guid)));
        match store.get(&key).await {
            Ok(b) => match write_atomic(&root.join(conflict_rel_path(guid, *rrev)), &b) {
                Ok(()) => report.conflicts += 1,
                Err(e) => report.failures.push(SyncFailure {
                    key: key.clone(),
                    reason: format!("冲突副本落盘失败: {e}"),
                    retryable: true,
                }),
            },
            Err(e) => report.failures.push(SyncFailure {
                key: key.clone(),
                reason: format!("冲突副本下载失败: {e}"),
                retryable: true,
            }),
        }
    }
    for (guid, lrev) in &reader_overwrites {
        // reader 侧：远端胜出 ⇒ 把**本地那一版**先拷出来旁置，随后它会被下载覆盖
        report.overwrote_dirty += 1;
        let Some(p) = lp.notes.get(guid) else { continue };
        let src = root.join(&p.exported_path);
        if !src.is_file() {
            continue; // 文件都不在（清单与磁盘脱节），没有可留的
        }
        let dest = root.join(conflict_rel_path_local(guid, *lrev));
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::copy(&src, &dest) {
            Ok(_) => report.conflicts += 1,
            Err(e) => report.failures.push(SyncFailure {
                key: conflict_rel_path_local(guid, *lrev),
                reason: format!("本地版留档失败（该篇仍会被远端版覆盖）: {e}"),
                retryable: true,
            }),
        }
    }
    if report.conflicts > 0 || report.overwrote_dirty > 0 {
        crate::commands::append_sync_log(&format!(
            "冲突处置：{} 篇留档 → {CONFLICT_DIR}/（{} {} 胜出；只读端覆盖本地脏 {} 篇；\
             归因篇目（如 {}））",
            report.conflicts,
            if is_reader { "reader：远端" } else { "writer：本地" },
            if is_reader { "覆盖本地" } else { "旁置远端" },
            report.overwrote_dirty,
            diverged
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // 3'. 差集：远端 md5 与本地清单不同（或本地文件缺失）→ 下载；本地有/远端无 → _trash
    //
    // **落点 = 清单的 `exported_path`**（D 轮修正）。库内布局由导入/导出决定
    // （`{目录}/{标题}.zip`，见 export.rs），云端的 `native/notes/{GUID}` 只是**对象键**。
    // 这里曾自己拼 `notes/{GUID}`（旧"导出根 = 导航根"时代的布局）：下行的篇目落在清单
    // 指不到的地方 ⇒ 解析器读不到正文、索引 FTS 全空、库自检报孤立文件。
    // 键与落点是两回事，绝不能互相推导。
    let mut downloads: Vec<(String, PathBuf)> = Vec::new();
    let mut trash_candidates: Vec<(String, PathBuf)> = Vec::new(); // (guid, 本地旧落点)
    for n in notes.values() {
        let dest = root.join(&n.exported_path);
        // 本地清单里 md5 已与远端一致且文件在 → 无需下载
        let same = lp
            .notes
            .get(&n.guid)
            .map(|p| p.exported_md5 == n.exported_md5)
            .unwrap_or(false);
        if !(same && dest.is_file()) && !local_keep.contains(&n.guid) {
            // 注：writer 侧"本地脏且分歧"的篇（**含真冲突**）一律不下载 —— 本地版胜出：
            // 真冲突已把远端版旁置到 `_conflicts/`，后继式改动则原样留着等下次上行。
            // 两者的清单行都会在同一轮末被回写成**本地行**（见 3''）。
            downloads.push((
                cfg.cloud_key(&format!("{KEY_NATIVE}/notes/{}", braced(&n.guid))),
                dest.clone(),
            ));
        }
        // 远端改了标题/目录（`exported_path` 变了）→ 本地旧落点是残留文件：同一个 guid 不能有
        // 两个文件，否则库自检按"文件不在清单里"报孤儿。移入回收站（可恢复），不直接删。
        // writer 侧"本地版胜出"的篇跳过：本地文件就是胜出版，不能被当残留搬走。
        if local_keep.contains(&n.guid) {
            continue;
        }
        if let Some(p) = lp.notes.get(&n.guid) {
            if p.exported_path != n.exported_path {
                let old = root.join(&p.exported_path);
                if old.is_file() && old != dest {
                    trash_candidates.push((n.guid.clone(), old));
                }
            }
        }
    }
    // 本地清单里有、远端清单里没有 → 远端（写入端）删了它 → 本地文件移入 `_trash`（Q8）
    for (guid, p) in &lp.notes {
        if notes.contains_key(guid) || local_keep.contains(guid) {
            continue;
        }
        let old = root.join(&p.exported_path);
        if old.is_file() {
            trash_candidates.push((guid.clone(), old));
        }
    }

    // 3''. writer 侧"本地版胜出"篇的**清单回写**：步骤 2e 刚照抄来的远端行必须被本地行盖回去，
    //      否则清单描述的是远端内容、磁盘上是本地内容（两者脱节，`exported_md5` 对不上，
    //      `check_invariants` 当场报错）。范围 = `writer_local_wins`（真冲突 + 后继式改动）。
    //      同时**置脏**：本地这一版还没上行，下一次 `sync_up` 会铸新号把它顶上去
    //      （这正是为知"本地版带新号顶上去"的形态）。
    //      ★ `synced_revision` 必须回写成本端**原来的号**（v6 修正）：它回答的是"我这一版基于
    //      云端哪一号"。照抄远端值 ⇒ `sync_up` 的第三条判据（远端 srev > 本端 srev）恒假 ⇒
    //      下一次上行判不出真冲突、把对方那一版**静默覆盖**（承重⑤ 对称双写用例栽在此处）。
    for guid in &writer_local_wins {
        if let Some(row) = lp.notes.get(guid) {
            manifest::upsert_note(&conn, row)?;
            manifest::mark_note_dirty(&conn, guid, true, true)?;
            let lrev = lp.synced_revs.get(guid).copied().unwrap_or(0);
            manifest::set_note_synced_revision(&conn, guid, lrev)?;
        }
    }
    manifest::recompute_watermarks(&conn)?;

    // 3b. 附件差集（Tier1–4 落盘；tier0 无文件）
    let atts = manifest::load_attachments(&conn)?;
    let mut att_downloads: Vec<(String, PathBuf)> = Vec::new();
    for a in &atts {
        let Some(key_rel) = &a.cloud_key else { continue };
        // cloud_key 为完整对象键（相对 prefix）；本地按**库内保留区**落盘（A1'：`_attachments/…`）
        let local = root.join(manifest::disk_rel_path(&a.file_path));
        if !local.exists() {
            att_downloads.push((cfg.cloud_key(key_rel), local));
        }
    }
    // cloud_key 尚未回填（旧清单）→ 按规则现推
    for a in &atts {
        if a.cloud_key.is_some() || a.tier == 0 {
            continue;
        }
        let seg = match a.file_path.strip_prefix("attachments/") {
            Some(s) => s.to_string(),
            None => a.file_path.clone(),
        };
        let cloud_rel = if a.tier == 4 {
            format!("{KEY_NATIVE}/_unlinked_attachments/{seg}")
        } else {
            format!("{KEY_NATIVE}/attachments/{seg}")
        };
        let local = root.join(manifest::disk_rel_path(&a.file_path));
        if !local.exists() {
            att_downloads.push((cfg.cloud_key(&cloud_rel), local));
        }
    }

    let total = downloads.len() + att_downloads.len();
    progress("download", 0, total);
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut dl: Vec<(String, PathBuf)> = downloads;
    dl.extend(att_downloads);
    let results = stream::iter(dl.into_iter().map(|(key, dest)| {
        let done = done.clone();
        let progress = progress.clone();
        async move {
            let bytes = store.get(&key).await.map_err(|e| (key.clone(), e))?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| (key.clone(), e.to_string()))?;
            }
            let t = dest.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&t, &bytes).map_err(|e| (key.clone(), e.to_string()))?;
            std::fs::rename(&t, &dest).map_err(|e| (key.clone(), e.to_string()))?;
            let n = done.fetch_add(1, Ordering::SeqCst) + 1;
            progress("download", n, total);
            Ok::<_, (String, String)>((key, bytes.len()))
        }
    }))
    .buffer_unordered(cfg.concurrency.max(1) as usize)
    .collect::<Vec<_>>()
    .await;
    for r in results {
        match r {
            Ok((_, n)) => {
                report.downloaded += 1;
                report.downloaded_bytes += n as u64;
            }
            Err((key, reason)) => report.failures.push(SyncFailure {
                key,
                reason,
                retryable: true,
            }),
        }
    }

    // 本地有 / 远端无（或被远端改名）→ `_trash/{YYYY-MM-DD}/`（Q8）。定名与 `library::delete_note`
    // 共用 `trash_dest`：同一天内两条路径都往同一个回收站放东西，重名保护必须互认。
    for (guid, p) in trash_candidates {
        let Ok((dest, _)) = crate::library::trash_dest(root, &p.to_string_lossy(), &guid) else {
            continue;
        };
        if std::fs::rename(&p, &dest).is_ok() {
            report.trashed += 1;
            // 移走文件后旧目录可能空了 —— 库里不该留空目录（浏览侧会显示空文件夹）
            crate::library::prune_empty_dirs(p.parent(), root);
        }
    }

    // 4. **建库索引**（U3：不再合成 WIZ_* 兼容源索引）
    //
    // 旧链路是「合成一个 WIZ_* 形态的 index.db → build_index（源索引）」，现在 export.db
    // 本身就是库索引的输入 ⇒ 少一个中间物、少一次全量合成，且索引落到**库索引**位置
    // （`index-{hash8}.db`，§5.3）而不是源索引 `index.db`。只读端下行完即可浏览/检索。
    progress("index", 0, 1);
    let index_db = crate::config::index_file_for_library(root);
    let lib = root.to_path_buf();
    let p = progress.clone();
    crate::commands::spawn_blocking(move || -> Result<(), String> {
        let resolver = Arc::new(crate::library::LibraryResolver::new(lib.clone())?);
        crate::indexer::build_library_index(&lib, resolver, &index_db, &|d, t| p("index", d, t))
            .map(|_| ())
    })
    .await
    .map_err(|e| e.to_string())??;
    progress("index", 1, 1);

    drop(conn);
    report.elapsed_ms = t0.elapsed().as_millis();
    persist_state(&report)?;
    Ok(report)
}

/// 本地清单的**版本水位**（`meta.revision`）。库不存在清单 → 0。
async fn local_revision(root: &Path) -> u64 {
    if !root.join(manifest::MANIFEST_NAME).exists() {
        return 0;
    }
    match manifest::open_and_migrate(root) {
        Ok(c) => manifest::current_revision(&c),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------- 首次初始化（§8.3）

/// 写入端初始化（§7.3）：**校验库就绪 → 全量上行**（U2 后不再有"先导出"这一步）。
/// **后台执行**（Q16）。
///
/// 库要由导入 / 设置数据目录那一步先建好（CLI 侧是 `build-md-library` / `export-zips`）。
/// 两件事分开的好处：初始化**不再依赖 `source_dir`**（源可选、可弃，§8.3），
/// 而且"同步"与"造库"的失败原因不再混在一条链路里（用户看到的错误各不相同）。
pub async fn bootstrap_export(
    store: &dyn ObjectStore,
    cfg: &SyncSettings,
    root: &Path,
    progress: ProgressFn,
) -> Result<SyncReport, String> {
    if !manifest::manifest_path(root).is_file() {
        return Err(
            "SYNC_NO_MANIFEST: 库清单 export.db 缺失 —— 同步根即库根，请先建立笔记库（导入为知笔记，\
             或把已有导出根设为数据目录）后再初始化"
                .into(),
        );
    }
    sync_up(store, cfg, root, progress).await
}

/// 只读端初始化（§7.3）：拉清单 → 差量下行 → 建库索引（§8.3）
pub async fn bootstrap_reader(
    store: &dyn ObjectStore,
    cfg: &SyncSettings,
    root: &Path,
    progress: ProgressFn,
) -> Result<SyncReport, String> {
    sync_down(store, cfg, root, progress).await
}

// ---------------------------------------------------------------- _trash GC（§6.4 / Q8）

#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    pub removed: usize,
    pub bytes: u64,
    pub kept: usize,
}

/// 物理清理 `_trash/` 下超期项。幂等；单条失败记 warning 不中断；
/// 删除前 canonicalize 校验目标在 `_trash/` 之内（防路径穿越）。
pub fn gc_trash(local_root: &Path, days: u64) -> Result<GcReport, String> {
    let trash_dir = local_root.join("_trash");
    let mut rep = GcReport { removed: 0, bytes: 0, kept: 0 };
    if !trash_dir.is_dir() {
        return Ok(rep);
    }
    let trash_canon = trash_dir.canonicalize().unwrap_or_else(|_| trash_dir.clone());
    let today = today_utc();
    let mut warnings = Vec::new();

    let date_dirs: Vec<PathBuf> = std::fs::read_dir(&trash_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    for dir in date_dirs {
        let name = dir.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        // 目录名即移除日期（比 mtime 可靠——拷贝/解压不丢）；解析失败回退 mtime
        let age_days = match parse_date(&name) {
            Some(d) => {
                let (ty, tm, td) = today_ymd();
                let now_d = days_from_civil(ty, tm, td);
                now_d.saturating_sub(d)
            }
            None => dir_age_days(&dir) as i64,
        };
        if age_days < days as i64 {
            rep.kept += count_files(&dir);
            continue;
        }
        let _ = today; // today 仅用于推导（上面已用）
        let _ = &name;
        remove_tree_if_inside(&dir, &trash_canon, &mut rep, &mut warnings);
    }
    if !warnings.is_empty() {
        crate::commands::append_sync_log(&warnings.join("\n"));
    }
    Ok(rep)
}

/// 物理清理 `_conflicts/` 下超期留档（v6，`docs/云同步逻辑.md` §7 第 5 条）。
///
/// 与 [`gc_trash`] 的两处差别：
/// - `_conflicts/` 是**扁平**的（`{guid}_{rev}.zip` / `{guid}_{rev}_local.zip`，无日期目录）
///   ⇒ 按**文件 mtime** 计龄，而不是解析目录名；
/// - 只删文件、不递归删目录（认不出的子目录一律 `kept`，宁可留着也不误删）。
///
/// 与 `_trash/` GC 一样**只动物理文件、不写清单** —— 因此不影响任何判据
/// （留档本就不入清单、不进索引）。删除前 canonicalize 校验目标在 `_conflicts/` 之内。
pub fn gc_conflicts(local_root: &Path, days: u64) -> Result<GcReport, String> {
    let dir = local_root.join(CONFLICT_DIR);
    let mut rep = GcReport { removed: 0, bytes: 0, kept: 0 };
    if !dir.is_dir() {
        return Ok(rep);
    }
    let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
    let mut warnings = Vec::new();
    let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .collect();
    for p in entries {
        if !p.is_file() {
            rep.kept += 1; // 认不出的子目录：不动
            continue;
        }
        if dir_age_days(&p) < days {
            rep.kept += 1;
            continue;
        }
        let Ok(canon) = p.canonicalize() else {
            warnings.push(format!("GC 跳过（无法解析路径 {}）", p.display()));
            continue;
        };
        if !canon.starts_with(&dir_canon) {
            warnings.push(format!("GC 拒绝删除 _conflicts 之外的路径: {}", p.display()));
            continue;
        }
        let sz = p.metadata().map(|m| m.len()).unwrap_or(0);
        if std::fs::remove_file(&canon).is_ok() {
            rep.removed += 1;
            rep.bytes += sz;
        } else {
            warnings.push(format!("GC 删除失败: {}", p.display()));
        }
    }
    if !warnings.is_empty() {
        crate::commands::append_sync_log(&warnings.join("\n"));
    }
    Ok(rep)
}

fn remove_tree_if_inside(dir: &Path, trash_canon: &Path, rep: &mut GcReport, warnings: &mut Vec<String>) {
    // 路径穿越防线：目标 canonicalize 后必须仍在 _trash/ 内
    let canon = match dir.canonicalize() {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!("GC 跳过（无法解析路径 {}）: {e}", dir.display()));
            return;
        }
    };
    if !canon.starts_with(trash_canon) {
        warnings.push(format!("GC 拒绝删除 _trash 之外的路径: {}", dir.display()));
        return;
    }
    rep.bytes += size_of_tree(&canon);
    rep.removed += count_files(&canon);
    if std::fs::remove_dir_all(&canon).is_err() {
        warnings.push(format!("GC 删除失败: {}", dir.display()));
    }
}

fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    Some(days_from_civil(y, m, d))
}

fn today_ymd() -> (i64, i64, i64) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    // days → civil（Hinnant 反演）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// 今日 UTC 日期 `YYYY-MM-DD`：`_trash/` 日期目录名（`gc_trash` 按此解析回天数）。
/// pub 供库写路径（`library::delete_note`）复用同一口径。
pub fn today_utc() -> String {
    let (y, m, d) = today_ymd();
    format!("{y:04}-{m:02}-{d:02}")
}

/// 今天距 epoch 的天数（list_trash 计算剩余保留天数用）
pub fn today_days() -> i64 {
    let (y, m, d) = today_ymd();
    days_from_civil(y, m, d)
}

/// 解析墓碑 removed_at（"YYYY-MM-DD HH:MM:SSZ" 或 "YYYY-MM-DD"）→ 距 epoch 天数
pub fn parse_removed_date(s: &str) -> Option<i64> {
    parse_date(s.get(..10)?)
}

/// civil date → days since epoch（Hinnant）
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn dir_age_days(dir: &Path) -> u64 {
    let mt = dir
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(mt) / 86400
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
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

fn size_of_tree(dir: &Path) -> u64 {
    let mut n = 0u64;
    let mut stack = vec![dir.to_path_buf()];
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

// ---------------------------------------------------------------- 启动任务（§6.3）

/// 启动任务：`_trash/` + `_conflicts/` GC → HEAD 水位比对 →（开关开且远端有更新）差量下行。
/// **任何失败都返回 Err 由调用方写日志/状态行，绝不阻塞首屏、绝不弹框。**
pub async fn startup_tasks(progress: ProgressFn) -> Result<String, String> {
    let t0 = Instant::now();
    let settings = crate::config::load_settings();
    let cfg = &settings.sync;

    // 1. 本地 GC（必做，与云开关无关）。
    //    T7 收口：GC 的根 = `Settings::trash_root()`（库模式 = library_dir），
    //    与 `library::delete_note` 的落点、回收站三命令**同一个根** —— 否则
    //    "删进库的回收站、GC 却在扫同步根" 会让库内 `_trash/` 永不清空。
    let mut summary = String::new();
    if let Some(root) = settings.trash_root() {
        let rep = gc_trash(&root, TRASH_RETENTION_DAYS)?;
        summary.push_str(&format!(
            "GC 清理 {} 项 / 保留 {} 项（{} 天保留期）",
            rep.removed, rep.kept, TRASH_RETENTION_DAYS
        ));
        // v6：`_conflicts/` 同批 GC（30 天）。留档是"被淘汰那一版"的唯一副本，
        // 在云端 LWW 裁决（`云同步逻辑.md` §11）建成前，这是唯一的安全网 ⇒ 有保留期，但不无限留。
        let rc = gc_conflicts(&root, CONFLICT_RETENTION_DAYS)?;
        if rc.removed > 0 || rc.kept > 0 {
            summary.push_str(&format!(
                "；冲突留档 GC 清理 {} 项 / 保留 {} 项（{} 天）",
                rc.removed, rc.kept, CONFLICT_RETENTION_DAYS
            ));
        }
    }

    // 2. 云端检查（仅 enabled，且必须已有库根 —— U1 后同步根恒为库根）
    let sync_root = settings.sync_root();
    if cfg.enabled && sync_root.is_some() {
        let root = sync_root.expect("刚判过 is_some");
        let user = cfg.credential_user.clone();
        if user.is_empty() {
            summary.push_str("；云同步已启用但未配置凭据");
        } else {
            let secret = crate::credential::read_secret(&user)?;
            let store = s3_store_of(cfg, &secret)?;
            let manifest_key = cfg.cloud_key("manifest.db");
            if let Some(h) = store.head(&manifest_key).await? {
                let remote_rev = h.meta("revision").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let local_rev = local_revision(&root).await;
                if remote_rev > local_rev {
                    if cfg.auto_check_on_start {
                        let rep = sync_down(&store, cfg, &root, progress).await?;
                        summary.push_str(&format!(
                            "；自动下行 {} 篇（{} 失败）",
                            rep.downloaded,
                            rep.failures.len()
                        ));
                    } else {
                        summary.push_str(&format!("；远端有更新（revision {remote_rev} > 本地 {local_rev}），未自动下载"));
                    }
                } else {
                    summary.push_str("；远端无更新");
                }
            } else {
                summary.push_str("；远端清单不存在（可能尚未初始化）");
            }
        }
    } else if cfg.enabled {
        summary.push_str("；云同步已启用但未设置笔记库（同步根即库根，U1）");
    }
    let _ = t0;
    Ok(summary)
}

// ---------------------------------------------------------------- 状态侧车（§7.3）

fn state_path() -> PathBuf {
    crate::config::wiz_home().join("sync-state.json")
}

fn persist_state(report: &SyncReport) -> Result<(), String> {
    let dir = crate::config::wiz_home();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let doc = serde_json::json!({
        "last_sync_at": manifest::format_utc(std::time::SystemTime::now()),
        "last_report": report,
    });
    std::fs::write(state_path(), serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// 读取上次同步状态（get_sync_status 用）；无记录返回 None
pub fn load_state() -> Option<(String, SyncReport)> {
    let s = std::fs::read_to_string(state_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let at = v.get("last_sync_at")?.as_str()?.to_string();
    let rep: SyncReport = serde_json::from_value(v.get("last_report")?.clone()).ok()?;
    Some((at, rep))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wiz-sync-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 测试隔离：把 wiz_home 重定向到临时目录，防止读写真实 ~/.wizreader。
    /// （sync 流程会 persist_state 到 wiz_home/sync-state.json、build_index 写 wiz_home/index.db）
    fn isolate_wiz_home() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("wiz-sync-test-wizhome-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("WIZREADER_HOME", &dir);
        });
    }

    /// U1：同步根恒为**调用方传入的库根**，代码不再读 `cfg.local_root`。
    /// 故这里**故意把 `local_root` 指向一个不存在的目录**：若还有哪条路径偷看它，
    /// 这些测试会立刻在错误路径上失败（相当于一次常驻的变异检查）。
    fn cfg_of(_root: &Path) -> SyncSettings {
        SyncSettings {
            local_root: "/nonexistent/legacy-sync-root".into(),
            prefix: "wiz".into(),
            ..Default::default()
        }
    }

    /// 真实小 zip（正文可被 `note_body_text` 解析）—— 用它而不是假字节，
    /// 才能把"下行完即可**检索**"这条验收真跑出来（假字节只会让正文解析失败、FTS 为空）。
    fn write_zip(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("index.html", opt).unwrap();
        std::io::Write::write_all(&mut zw, body.as_bytes()).unwrap();
        zw.finish().unwrap();
    }

    fn seed_manifest(root: &Path, guid: &str, body: &str) -> PathBuf {
        seed_manifest_rev(root, guid, body, "1")
    }

    /// 造一个"最小库根"：`docs/n.zip` + 清单（含一行 note）。
    /// 行级 `revision` 也置 1 —— 真实库里"被写过"的篇目就是这个样子，
    /// U4 护栏（`local_rev >= remote_rev`）与 U5 冲突命名都依赖它非 0。
    fn seed_manifest_rev(root: &Path, guid: &str, body: &str, meta_rev: &str) -> PathBuf {
        let zip_path = root.join("docs").join("n.zip");
        write_zip(&zip_path, body);
        let conn = manifest::open_and_migrate(root).unwrap();
        manifest::upsert_note(
            &conn,
            &manifest::ManifestNote {
                guid: guid.into(),
                title: "t".into(),
                location: "/d/".into(),
                created: String::new(),
                data_modified: "2024-01-01".into(),
                url: None,
                doc_type: None,
                has_attachment: false,
                package_size: std::fs::metadata(&zip_path).unwrap().len() as i64,
                exported_path: "docs/n.zip".into(),
                exported_size: std::fs::metadata(&zip_path).unwrap().len() as i64,
                exported_md5: manifest::md5_file(&zip_path).unwrap(),
                export_mode: "native".into(),
                exported_at: "2026-09-17".into(),
                origin: crate::manifest::ORIGIN_WIZNOTE.into(),
                content_format: crate::manifest::FORMAT_HTML.into(),
            },
        )
        .unwrap();
        manifest::bump_note_revision(&conn, guid).unwrap();
        manifest::set_meta(&conn, "export_mode", "native").unwrap();
        manifest::set_meta(&conn, "revision", meta_rev).unwrap();
        zip_path.parent().unwrap().to_path_buf()
    }

    /// 在**已建好的**库根里追加一篇（落点 `docs/{tag}.zip`）。
    ///
    /// 用途：造"远端前进的是**别的**篇"这一场景 —— 那是"writer 本地脏但**不是**冲突"
    /// （`远端 synced_revision == 本端 synced_revision`）唯一能成立的前提：只有远端**没**重铸
    /// 这一篇，`writer` 侧的第二条件才判得出"远端那一版就是我自己发的"。
    /// 新行按 `upsert_note` 的 INSERT 支自带闩（= 有内容待上行）。
    fn add_note(root: &Path, guid: &str, tag: &str, body: &str) {
        let rel = format!("docs/{tag}.zip");
        let zip_path = root.join(&rel);
        write_zip(&zip_path, body);
        let size = std::fs::metadata(&zip_path).unwrap().len() as i64;
        let conn = manifest::open_and_migrate(root).unwrap();
        manifest::upsert_note(
            &conn,
            &manifest::ManifestNote {
                guid: guid.into(),
                title: tag.into(),
                location: "/d/".into(),
                created: String::new(),
                data_modified: "2024-01-01".into(),
                url: None,
                doc_type: None,
                has_attachment: false,
                package_size: size,
                exported_path: rel,
                exported_size: size,
                exported_md5: manifest::md5_file(&zip_path).unwrap(),
                export_mode: "native".into(),
                exported_at: "2026-09-17".into(),
                origin: crate::manifest::ORIGIN_WIZNOTE.into(),
                content_format: crate::manifest::FORMAT_HTML.into(),
            },
        )
        .unwrap();
    }

    /// 在本机对某篇做一次**真实库内写**（走 `library::save_note_html`，与 UI / CLI 同一条路径）。
    ///
    /// **为什么不手搓等价物**（`docs/云同步逻辑.md` §6.3 末段的纪律）：自造的"改 md5 +
    /// `bump_note_revision`"会漏掉 v6 判据的**关键一半 —— 置脏闩**。漏了它，`sync_up` 根本
    /// 看不到待上行项，测试退化成"在我自己规定的世界里自证空跑"。真实写路径一次给出全部：
    /// 落盘字节、`exported_md5`/`exported_size`、`data_modified`、行级计数器、**闩**，
    /// 且**不碰** `meta.revision`（那正是 §6.3 ② 要守的东西）。
    ///
    /// **调用方必须持 [`crate::library::test_write_lock`]**：库写路径要取 `sync::try_acquire`
    /// 的**进程级**锁（库级写锁与同步共用一把），并行跑会与其它写类测试互相判 `LOCK_BUSY`。
    fn local_write(root: &Path, guid: &str, html: &str) {
        let index_db = crate::config::index_file_for_library(root);
        crate::library::save_note_html(root, &index_db, guid, html).unwrap();
    }

    /// 读清单里某篇的 `exported_path`（下行落点的唯一权威）
    fn exported_path_of(root: &Path, guid: &str) -> String {
        let conn = manifest::open_readonly(root).unwrap();
        let notes = manifest::load_notes(&conn).unwrap();
        notes.get(guid).expect("清单里应有该篇").exported_path.clone()
    }

    /// 整库复制（模拟"另一台机器拿到同一份库"）：逐文件复制，含清单与全部包。
    /// U5 的对称双写用例要靠它造出"两侧从同一基线出发"的前提。
    fn copy_lib(from: &Path, to: &Path) {
        fn walk(src: &Path, dst: &Path) {
            std::fs::create_dir_all(dst).unwrap();
            for e in std::fs::read_dir(src).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                let q = dst.join(e.file_name());
                if p.is_dir() {
                    walk(&p, &q);
                } else {
                    std::fs::copy(&p, &q).unwrap();
                }
            }
        }
        walk(from, to);
    }

    #[tokio::test]
    async fn test_up_down_roundtrip_on_memstore() {
        isolate_wiz_home();
        let src = temp_root("src");
        let dst = temp_root("dst");
        let guid = "11111111-2222-3333-4444-555555555555";
        seed_manifest(&src, guid, "<html><body>苹果 bluestore 集群</body></html>");
        let cfg = cfg_of(&src);
        let src_bytes = std::fs::read(src.join("docs").join("n.zip")).unwrap();

        let store = MemStore::new();
        // U2 后没有"先重新导出"这一级：上行直接扫库，故不再有 data_dir 参数
        let p = noop_progress();
        let rep = sync_up(&store, &cfg, &src, p.clone()).await.unwrap();
        assert!(rep.manifest_uploaded);
        assert_eq!(rep.uploaded, 1, "1 篇笔记 zip 上传");
        assert_eq!(rep.conflicts, 0, "远端尚无清单，无冲突可言");
        assert!(rep.failures.is_empty(), "{:?}", rep.failures);
        // 空跑（闩干净）：**不铸版、不提交**，且一次网络都不打（v6：判据是闩，不是"再比一遍 md5"）
        let rep2 = sync_up(&store, &cfg, &src, p.clone()).await.unwrap();
        assert_eq!(rep2.direction, "none", "无待上行项 ⇒ 空跑");
        assert_eq!(rep2.uploaded, 0);
        assert!(!rep2.manifest_uploaded, "空跑不得提交清单");
        // 对象级续传（**只在闩仍置位时才有意义**）：把闩重新置上、内容一字未改 ——
        // 这正是"上一轮对象传成了、清单没提交成功"的回滚态。重跑应只 HEAD 命中并 skip，
        // 不重复 PUT 对象，但仍要铸版 + 提交清单（否则闩永远清不掉、版本号也不会推进）。
        {
            let c = manifest::open_and_migrate(&src).unwrap();
            manifest::mark_note_dirty(&c, guid, false, true).unwrap();
        }
        let rep3 = sync_up(&store, &cfg, &src, p.clone()).await.unwrap();
        assert_eq!(rep3.uploaded, 0, "内容已在云端且 md5 一致 ⇒ 对象级续传命中");
        assert_eq!(rep3.skipped, 1, "跳过对象计入 skipped");
        assert!(rep3.manifest_uploaded, "但清单仍须提交（闩已清、要铸版）");
        assert_eq!(rep3.conflicts, 0, "单写者重复上行不得留冲突副本（判据不得误伤正常续传）");

        // 只读端：换根拉取
        let cfg_r = cfg_of(&dst);
        let rep_d = sync_down(&store, &cfg_r, &dst, p.clone()).await.unwrap();
        assert_eq!(rep_d.downloaded, 1);
        // **落点 = 清单的 exported_path**（D 轮修正）：不是自己拼的 `notes/{GUID}`。
        // 真实库的 exported_path 是 `{目录}/{标题}.zip`，拼错了解析器就找不到正文。
        let note = dst.join(&exported_path_of(&src, guid));
        assert!(note.is_file(), "下行落点应为清单的 exported_path: {}", note.display());
        assert_eq!(std::fs::read(&note).unwrap(), src_bytes, "下行内容与上行逐字节一致");
        assert!(
            !dst.join("notes").is_dir(),
            "不得再自己拼 notes/ 目录（旧布局残留）：落点由清单决定"
        );
        assert!(dst.join(manifest::MANIFEST_NAME).exists(), "清单已原子替换");

        // U3：下行建的是**库索引**（`index-{hash8}.db`），且**不再**产出 WIZ_* 兼容源索引
        let idx = crate::config::index_file_for_library(&dst);
        assert!(idx.is_file(), "库索引应已建立: {}", idx.display());
        assert!(!dst.join("index.db").exists(), "U3 后不得再合成兼容源索引 index.db");
        let ic = rusqlite::Connection::open(&idx).unwrap();
        let n: i64 = ic.query_row("SELECT count(*) FROM note", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "库索引应有 1 篇（export.db 直接作为输入）");
        let folders: i64 = ic.query_row("SELECT count(*) FROM folder", [], |r| r.get(0)).unwrap();
        assert!(folders >= 1, "目录树应已建立（可浏览）");
        let hits: i64 = ic
            .query_row("SELECT count(*) FROM note_fts WHERE note_fts MATCH 'bluestore'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 1, "只读端下行完**即可检索**（U3 的验收口径）");
        drop(ic);

        // 无更新时第二次 down 直接返回（revision 相同）
        let rep_d2 = sync_down(&store, &cfg_r, &dst, p).await.unwrap();
        assert_eq!(rep_d2.direction, "none");
    }

    /// 承重⑤（简单面）：远端内容已分叉、本地也改过 ⇒ **writer 冲突**。
    /// 本地胜出（本地版照旧上行）、被淘汰的**远端那一版**旁置
    /// `_conflicts/{guid}_{远端 rev}.zip`，该篇**不被远端覆盖**；留档**不入清单、不进索引**。
    ///
    /// **变异检查**：把判据退回"比清单级 / 行级 revision 谁大" → 必 FAILED
    /// （D 轮原 bug：两端从同一基线各改一版，两侧计数器撞车 ⇒ 判据恒假 ⇒ 远端版被静默吃掉）。
    #[tokio::test]
    async fn test_writer_conflict_retains_remote_version_and_local_wins() {
        let _s = crate::library::test_write_lock(); // local_write 走真实库写路径（取进程级写锁）
        isolate_wiz_home();
        let a = temp_root("cf-a");
        let b = temp_root("cf-b");
        let guid = "55555555-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>基线</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        // A 首次上行（闩驱动）→ 远端清单出现第 1 版
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();

        // 另一台机器拿到同一份库，改同一篇并推上去 → 远端铸出新号
        copy_lib(&a, &b);
        local_write(&b, guid, "<html><body>远端版 v2</body></html>");
        sync_up(&store, &cfg_of(&b), &b, p.clone()).await.unwrap();
        let remote_srev = {
            let c = manifest::open_readonly(&b).unwrap();
            manifest::load_note_synced_revisions(&c).unwrap().get(guid).copied().unwrap_or(0)
        };
        let remote_zip = std::fs::read(b.join("docs").join("n.zip")).unwrap();
        assert!(remote_srev > 1, "远端应已铸出新号（> 首号）: {remote_srev}");

        // A 也改这一篇（A 手上仍基于旧号）→ 上行时构成**真冲突**
        local_write(&a, guid, "<html><body>本地版 v2</body></html>");
        let local_zip = std::fs::read(a.join("docs").join("n.zip")).unwrap();
        // §7.1 起：第二写入端**必须先下行对齐**再上行（否则整份清单会把 B 那一行写回旧值，
        // 提交前判据会拦下并报 SYNC_MANIFEST_STALE）。对齐这一步正是真实第二台机器会做的，
        // 而且对齐**不会**吃掉 A 的本地版 —— writer 侧真冲突是"本地胜出、一律不下载"。
        let d = sync_down(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        assert_eq!(d.overwrote_dirty, 0, "writer 真冲突不得被远端覆盖");
        assert_eq!(std::fs::read(a.join("docs").join("n.zip")).unwrap(), local_zip);
        let rep = sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        assert_eq!(rep.conflicts, 1, "应留档 1 篇远端版: {:?}", rep.failures);
        let conflict = a.join(conflict_rel_path(guid, remote_srev));
        assert!(conflict.is_file(), "留档应在 {}", conflict.display());
        assert_eq!(std::fs::read(&conflict).unwrap(), remote_zip, "留档的必须是**远端那一版**");
        // 本地胜出：本地文件原地不动，且它已顶到云端
        assert_eq!(std::fs::read(a.join("docs").join("n.zip")).unwrap(), local_zip, "本地版不得被覆盖");
        let key = cfg_of(&a).cloud_key(&format!("{KEY_NATIVE}/notes/{{{guid}}}"));
        assert_eq!(
            store.get(&key).await.unwrap(),
            local_zip,
            "writer 冲突的胜方是本地版 ⇒ 云端对象应已变成本地那一版"
        );
        // 留档不入清单、不进索引
        let conn = manifest::open_readonly(&a).unwrap();
        let notes = manifest::load_notes(&conn).unwrap();
        assert_eq!(notes.len(), 1, "留档不得进清单");
        assert!(!notes.values().any(|n| n.exported_path.contains(CONFLICT_DIR)));
    }

    /// 承重⑤的**对称双写**面（E 两机闭环第一次跑就栽在这里）：两端从**同一基线**各改一版
    /// ⇒ 两侧清单 revision 与行级 revision **同时撞车**，旧口径（比 rev 谁大）恒为平局、
    /// 判不出冲突 ⇒ 远端版被静默覆盖。
    ///
    /// 同一测试顺带守住**第二条件的边界**：单写者连续改同一篇（远端那一版就是他自己发的）
    /// **不得**产生任何留档 —— 否则每改一遍就留一份自己的旧版，真冲突被噪声淹没。
    #[tokio::test]
    async fn test_writer_conflict_symmetric_double_edit_retains_remote() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("sym-a");
        let b = temp_root("sym-b");
        let guid = "aaaaaaaa-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>基线</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();

        // 另一台机器拿到**同一份**库（逐字节复制，含清单与包）
        copy_lib(&a, &b);
        {
            let c = manifest::open_readonly(&b).unwrap();
            assert!(
                manifest::load_unsynced_note_guids(&c).unwrap().is_empty(),
                "复制来的库其闩应为干净（闩由写侧置、同步侧清，不随复制变成脏）"
            );
        }

        // 单写者连改两版：**不得**留档（远端那一版就是 A 自己发的 ⇒ 不是冲突）
        local_write(&a, guid, "<html><body>来自 A（第1版）</body></html>");
        let rep1 = sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        assert_eq!(rep1.conflicts, 0, "单写者后继编辑不是冲突，不得留档: {:?}", rep1.failures);
        local_write(&a, guid, "<html><body>来自 A（第2版）</body></html>");
        let rep2 = sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        assert_eq!(rep2.conflicts, 0, "连续第二版同样不是冲突");
        let a_zip = std::fs::read(a.join("docs").join("n.zip")).unwrap();
        let remote_srev = {
            let c = manifest::open_readonly(&a).unwrap();
            manifest::load_note_synced_revisions(&c).unwrap().get(guid).copied().unwrap_or(0)
        };

        // B 从**同一基线**（复制来的那一份）独立改一版 —— 这正是旧口径必然判漏的场景
        local_write(&b, guid, "<html><body>来自 B</body></html>");
        let b_zip = std::fs::read(b.join("docs").join("n.zip")).unwrap();

        // B 上行：**必须**先把远端那一版留档，否则 A 的改动被静默吃掉
        // （§7.1 起：B 的清单基线也旧了，故先下行对齐 —— 对齐同样不会吃掉 B 的本地版）
        let d = sync_down(&store, &cfg_of(&b), &b, p.clone()).await.unwrap();
        assert_eq!(d.overwrote_dirty, 0, "writer 真冲突不得被远端覆盖");
        assert_eq!(std::fs::read(b.join("docs").join("n.zip")).unwrap(), b_zip);
        let rep = sync_up(&store, &cfg_of(&b), &b, p.clone()).await.unwrap();
        assert_eq!(rep.conflicts, 1, "对称双写必须留档 1 篇: {:?}", rep.failures);
        let conflict = b.join(conflict_rel_path(guid, remote_srev));
        assert!(conflict.is_file(), "留档应在 {}", conflict.display());
        assert_eq!(std::fs::read(&conflict).unwrap(), a_zip, "留档的必须是**远端（A）那一版**");
        // 本地版胜出：本地文件没被动过，且它已被推上云（覆盖远端）
        assert_eq!(std::fs::read(b.join("docs").join("n.zip")).unwrap(), b_zip, "本地版不得被覆盖");
        let key = cfg_of(&b).cloud_key(&format!("{KEY_NATIVE}/notes/{{{guid}}}"));
        assert_eq!(
            store.get(&key).await.unwrap(),
            b_zip,
            "writer 冲突的胜方是本地版 ⇒ 云端对象应已变成本地那一版"
        );
        // 留档不入清单、不进索引
        let conn = manifest::open_readonly(&b).unwrap();
        let notes = manifest::load_notes(&conn).unwrap();
        assert_eq!(notes.len(), 1, "留档不得进清单");
        assert!(!notes.values().any(|n| n.exported_path.contains(CONFLICT_DIR)));
    }

    /// 承重⑥：**reader 冲突 → 远端胜出**（2026-09-19 用户裁定）：
    /// 本地只读、云端改了，**就直接下载云端数据并更新**，不因本地脏而卡住下行。
    ///
    /// 必须同时满足三件事：① **不抛** `SYNC_READER_DIRTY`（不再阻断）；
    /// ② 该篇**被远端版覆盖**；③ 被覆盖的本地版**旁置留档**，不静默丢。
    ///
    /// **变异检查**：把处置改回"脏则拒"（抛 `SYNC_READER_DIRTY`）→ 本测试必 FAILED。
    #[tokio::test]
    async fn test_reader_conflict_overwrites_local_and_retains_local_version() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("ro-a");
        let b = temp_root("ro-b");
        let guid = "dddddddd-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>v1</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        let mut writer = cfg_of(&a);
        writer.role = crate::config::ROLE_WRITER.into();
        sync_up(&store, &writer, &a, p.clone()).await.unwrap();

        let mut reader = cfg_of(&b);
        reader.role = ROLE_READER.into();
        sync_down(&store, &reader, &b, p.clone()).await.unwrap();
        let local_srev = {
            let c = manifest::open_readonly(&b).unwrap();
            manifest::load_note_synced_revisions(&c).unwrap().get(guid).copied().unwrap_or(0)
        };

        // 只读端本地被改过（真实写路径 ⇒ 置闩）；写入端同时前进一版 ⇒ 真分叉
        local_write(&b, guid, "<html><body>本地改过的一版</body></html>");
        let local_zip = std::fs::read(b.join("docs").join("n.zip")).unwrap();
        local_write(&a, guid, "<html><body>v2 来自云端</body></html>");
        sync_up(&store, &writer, &a, p.clone()).await.unwrap();
        let v2_remote = std::fs::read(a.join("docs").join("n.zip")).unwrap();

        // ① 不阻断：正常返回
        let rep = sync_down(&store, &reader, &b, p).await.expect("reader 不再因本地脏而阻断下行");
        assert_eq!(rep.overwrote_dirty, 1, "应报告覆盖了 1 篇本地脏: {:?}", rep.failures);
        assert_eq!(rep.conflicts, 1, "被覆盖的本地版应旁置留档 1 份");
        // ② 远端胜出
        assert_eq!(
            std::fs::read(b.join("docs").join("n.zip")).unwrap(),
            v2_remote,
            "reader 冲突时云端版本胜出 ⇒ 本地文件应被覆盖"
        );
        // ③ 本地版留档（不静默丢）
        let keep = b.join(conflict_rel_path_local(guid, local_srev));
        assert!(keep.is_file(), "被覆盖的本地版应在 {}", keep.display());
        assert_eq!(std::fs::read(&keep).unwrap(), local_zip, "留档的必须是**本地那一版**");
        // 覆盖后闩必须干净，否则只读端每轮都会被判"脏"
        let c = manifest::open_readonly(&b).unwrap();
        assert!(manifest::load_unsynced_note_guids(&c).unwrap().is_empty(), "覆盖后闩必须清干净");
    }

    /// **承重②的同步侧边界**：writer 本地脏、远端也变了，但远端那一版**就是自己发的**
    /// （`远端 srev == 本端 srev`）⇒ 不是冲突，却也**绝不能下载覆盖** —— 否则本地未上行的
    /// 改动被静默吃掉。
    ///
    /// 这一支是加"第二条件"（`远端 srev > 本端 srev`）时**必须同时守住**的边界：
    /// 只加条件而不挡下载，就会开出这条丢数据的口子。场景靠**另一篇**被远端改动来推进
    /// 清单水位（故需要第二篇，见 `add_note`）。
    #[tokio::test]
    async fn test_writer_local_edit_survives_down_when_remote_not_advanced() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("wd-a");
        let b = temp_root("wd-b");
        let gx = "ffffffff-2222-3333-4444-555555555555";
        let gy = "ffffffff-3333-4444-5555-666666666666";
        seed_manifest(&a, gx, "<html><body>X v1</body></html>");
        add_note(&a, gy, "y", "<html><body>Y v1</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        let mut writer = cfg_of(&a);
        writer.role = crate::config::ROLE_WRITER.into();
        sync_up(&store, &writer, &a, p.clone()).await.unwrap();

        // B 拿到同一份库，改了 X 但**还没上行**
        copy_lib(&a, &b);
        let mut writer_b = cfg_of(&b);
        writer_b.role = crate::config::ROLE_WRITER.into();
        local_write(&b, gx, "<html><body>B 本地未上行的改动</body></html>");
        let bx_zip = std::fs::read(b.join("docs").join("n.zip")).unwrap();

        // 云端由 A 前进一版：改的是**别的**篇（Y）⇒ X 的铸造号**不动**
        local_write(&a, gy, "<html><body>Y v2</body></html>");
        sync_up(&store, &writer, &a, p.clone()).await.unwrap();

        // B 下行：X 本地脏 ⇒ 保本地、不下载；且**不留档**（远端那一版是它的祖先，不是对手）
        let rep = sync_down(&store, &writer_b, &b, p.clone()).await.unwrap();
        assert_eq!(rep.conflicts, 0, "远端没重铸 X ⇒ 不是冲突，不得留档: {:?}", rep.failures);
        assert_eq!(
            std::fs::read(b.join("docs").join("n.zip")).unwrap(),
            bx_zip,
            "writer 的本地脏篇不得被下载覆盖"
        );
        // 清单行必须回写成**本地行**（否则清单描述远端、磁盘是本地 ⇒ 当场脱节）
        let conn = manifest::open_readonly(&b).unwrap();
        assert_eq!(
            manifest::load_notes(&conn).unwrap().get(gx).unwrap().exported_md5,
            manifest::md5_file(&b.join("docs").join("n.zip")).unwrap(),
            "清单 md5 必须与磁盘一致"
        );
        assert!(
            manifest::load_unsynced_note_guids(&conn).unwrap().contains(gx),
            "该篇仍应带闩（还没上行）"
        );
        drop(conn);

        // 下一次上行即把本地版顶上去
        let rep_up = sync_up(&store, &writer_b, &b, p).await.unwrap();
        assert!(rep_up.manifest_uploaded, "本地未上行的改动应能上行");
        let key = cfg_of(&b).cloud_key(&format!("{KEY_NATIVE}/notes/{{{gx}}}"));
        assert_eq!(store.get(&key).await.unwrap(), bx_zip, "云端应已被本地版顶掉");
    }

    /// 护栏的**删除传播面**（承重检查）：写入端删掉一篇后，只读端本地还留着它 ——
    /// 「本地有 & 远端 note 表没有」**不是**本地改动，而是远端删除的正常传播，必须放行，
    /// 并把本地那份移进 `_trash`。判据靠**墓碑**：远端 `deleted` 表里有它 ≠ 本地新增。
    /// （没有这条区分时，删除永远传不到只读端 —— E 两机闭环第一次跑就栽在这里。）
    #[tokio::test]
    async fn test_down_propagates_remote_deletion() {
        // `delete_note` 取 `sync::try_acquire` 的**进程级**写锁 → 必须与库写测试共用串行闸门
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("rm-a");
        let b = temp_root("rm-b");
        let guid = "88888888-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>将被远端删除</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        let mut writer = cfg_of(&a);
        writer.role = crate::config::ROLE_WRITER.into();
        sync_up(&store, &writer, &a, p.clone()).await.unwrap();

        let mut reader = cfg_of(&b);
        reader.role = ROLE_READER.into();
        sync_down(&store, &reader, &b, p.clone()).await.unwrap();
        let note = b.join(exported_path_of(&b, guid));
        assert!(note.is_file(), "只读端先得有这篇");

        // 写入端删除（文件入回收站 + 清单落墓碑）→ 上行
        crate::library::delete_note(&a, &crate::config::index_file_for_library(&a), guid).unwrap();
        sync_up(&store, &writer, &a, p.clone()).await.unwrap();

        // 只读端下行：放行，且把本地那份移进回收站
        let rep = sync_down(&store, &reader, &b, p).await.unwrap();
        assert_eq!(rep.trashed, 1, "远端删掉的篇目应在只读端移入回收站: {:?}", rep.failures);
        assert!(!note.exists(), "原文件应已不在");
        let trash = b.join("_trash").join(crate::sync::today_utc());
        assert!(trash.is_dir(), "回收站日期目录应在 {}", trash.display());
        assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 1, "回收站里应有 1 个文件");
    }

    /// 反方向：只读端**自己删了**一篇却没上行 ⇒ 远端仍带着它。
    ///
    /// v6 的处置与旧口径相反：不阻断，而是**远端胜出 —— 把这一篇拉回来**
    /// （只读端本地删除永远上不去，留着"删掉"这个状态没有意义，覆盖是唯一能收敛的动作）。
    #[tokio::test]
    async fn test_down_restores_note_deleted_locally_on_reader() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("rdel-a");
        let b = temp_root("rdel-b");
        let guid = "99999999-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>只读端本地删除</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        let mut writer = cfg_of(&a);
        writer.role = crate::config::ROLE_WRITER.into();
        sync_up(&store, &writer, &a, p.clone()).await.unwrap();
        let mut reader = cfg_of(&b);
        reader.role = ROLE_READER.into();
        sync_down(&store, &reader, &b, p.clone()).await.unwrap();

        // 只读端本地删除（库层路径，绕过 UI 的置灰）—— 这正是"误把它当写入端"的动作
        crate::library::delete_note(&b, &crate::config::index_file_for_library(&b), guid).unwrap();
        assert!(!b.join(exported_path_of(&a, guid)).is_file(), "本地文件已删");

        // 远端再前进一版，让下行真的走起来
        local_write(&a, guid, "<html><body>远端 v2</body></html>");
        sync_up(&store, &writer, &a, p).await.unwrap();

        // 不阻断：远端胜出 ⇒ 这一篇被拉回来
        let rep = sync_down(&store, &reader, &b, noop_progress()).await.expect("reader 不再阻断");
        assert!(rep.downloaded >= 1, "远端仍带着这篇 ⇒ 应被拉回来: {:?}", rep.failures);
        assert!(b.join(exported_path_of(&a, guid)).is_file(), "被本地删掉的篇目应已拉回");
    }

    /// 承重④：`sync_down` 走**水位**短路（`remote.revision <= meta.revision` ⇒ 1 次 HEAD 就返回），
    /// 且下行**不铸版** —— 本地清单版本**等于**远端水位，不是"远端 +1"。
    ///
    /// **变异检查**：下行改成 `bump_revision()` → `local == remote` 必 FAILED。
    #[tokio::test]
    async fn test_down_aligns_watermark_and_short_circuits() {
        isolate_wiz_home();
        let a = temp_root("wm-a");
        let b = temp_root("wm-b");
        let guid = "eeeeeeee-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>x</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();

        let mut reader = cfg_of(&b);
        reader.role = ROLE_READER.into();
        let rep1 = sync_down(&store, &reader, &b, p.clone()).await.unwrap();
        // 下行不铸版：本地清单版本 == 远端水位
        let local_after = {
            let c = manifest::open_readonly(&b).unwrap();
            manifest::current_revision(&c)
        };
        assert_eq!(
            local_after,
            rep1.remote_revision.unwrap(),
            "下行**不铸版**：本地清单版本应等于远端水位"
        );
        // 水位已对齐 ⇒ 第二次直接 none、一个对象都不拉
        let rep2 = sync_down(&store, &reader, &b, p).await.unwrap();
        assert_eq!(rep2.direction, "none");
        assert_eq!(rep2.downloaded, 0);
    }

    /// 承重检查：**下行必须把闩清干净** —— 否则只读端每轮都会被判"本地脏"，
    /// 于是"只是落后"被读成"本地改过"（旧口径下这正是 U4 误判的来源）。
    /// 下下来的篇目带着远端的 `synced_revision`（>0）**不是**问题：那是铸造号，不是闩。
    #[tokio::test]
    async fn test_down_clears_latch_and_uses_synced_revision_not_row_revision() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("gd-a");
        let b = temp_root("gd-b");
        let guid = "77777777-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>same</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        let mut reader = cfg_of(&b);
        reader.role = ROLE_READER.into();
        sync_down(&store, &reader, &b, p.clone()).await.unwrap();

        {
            let conn = manifest::open_and_migrate(&b).unwrap();
            assert!(
                manifest::load_unsynced_note_guids(&conn).unwrap().is_empty(),
                "下行来的篇目必须是**干净**的（闩清零）—— '闩' 与 '铸造号' 是两回事"
            );
            let srev = manifest::load_note_synced_revisions(&conn).unwrap();
            assert!(srev.get(guid).copied().unwrap_or(0) > 0, "但它带着远端的铸造号（>0）");
        }

        // 远端前进一版：本地只是**落后**，没人改过本地 → 必须放行（且要真的拉到新内容）
        local_write(&a, guid, "<html><body>same v2</body></html>");
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        let rep = sync_down(&store, &reader, &b, p).await.unwrap();
        assert!(rep.downloaded >= 1, "只读端落后时应能更新");
        assert_eq!(rep.overwrote_dirty, 0, "没人改过本地 ⇒ 不得报告覆盖本地脏");
        assert_eq!(rep.conflicts, 0, "落后不是冲突 ⇒ 不得留档");
    }

    /// 承重③：`sync_up` 的**空跑不铸版**与**铸版即清闩**两面。
    ///
    /// **变异检查**：去掉 `changed` 判据（无条件铸版）→ 第二次上行的 `local_revision`
    /// 会前进，断言必 FAILED；去掉 `mark_notes_synced` → 闩不清，同样 FAILED。
    #[tokio::test]
    async fn test_up_mints_only_when_changed_and_clears_latch() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("up-a");
        let guid = "cccccccc-1111-2222-3333-444444444444";
        seed_manifest(&a, guid, "<html><body>v1</body></html>");
        let store = MemStore::new();
        let p = noop_progress();

        let rep1 = sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        let rev1 = rep1.local_revision;
        assert!(rep1.manifest_uploaded, "首轮应铸版并提交");
        {
            let c = manifest::open_readonly(&a).unwrap();
            assert!(manifest::load_unsynced_note_guids(&c).unwrap().is_empty(), "上行后闩必须清零");
            let srev = manifest::load_note_synced_revisions(&c).unwrap();
            assert_eq!(
                srev.get(guid).copied().unwrap_or(0),
                rev1 as i64,
                "synced_revision = 本轮铸造号"
            );
            assert_eq!(
                manifest::watermark(&c, manifest::DOCUMENT_WATERMARK),
                rev1,
                "文档水位应推进到本轮铸造号"
            );
        }

        // 空跑：闩干净 ⇒ 不铸版、不提交，且**连远端清单都不查**（无待上行项 ⇒ 不可能有冲突）
        let rep2 = sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        assert_eq!(rep2.direction, "none", "无待上行项 ⇒ 空跑");
        assert!(!rep2.manifest_uploaded, "空跑不得提交清单");
        assert_eq!(rep2.local_revision, rev1, "空跑不得推进版本号");
        assert_eq!(rep2.remote_revision, None, "空跑不打网络");

        // 真改一版 → 又铸新号，且只 +1
        local_write(&a, guid, "<html><body>v2</body></html>");
        let rep3 = sync_up(&store, &cfg_of(&a), &a, p).await.unwrap();
        assert_eq!(rep3.local_revision, rev1 + 1, "有改动才铸版，且只 +1");
        assert!(rep3.manifest_uploaded);
    }

    /// **§7.1 ①：本端基线已落后 ⇒ 提交前拦下**（`SYNC_MANIFEST_STALE`）。
    ///
    /// 场景就是 E 两机闭环 §9b 那个：第二台写入端的清单停在旧号，而远端已被先写者推进。
    /// 若放它上行，整份清单会把**别人那些行**一并写回旧值（对象字节却不回退）⇒
    /// 远端「清单行 ≠ 对象字节」的永久脱节。
    ///
    /// **变异检查**：删掉 `rm.revision > local_rev` 那道判据 → 本用例不再返回错误，
    /// `unwrap_err` 当场 panic；且"远端清单字节未被改动"的断言也会 FAILED。
    #[tokio::test]
    async fn test_up_aborts_when_local_manifest_is_stale() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("stale-a");
        let b = temp_root("stale-b");
        let guid = "88888888-2222-3333-4444-555555555555";
        let other = "99999999-2222-3333-4444-555555555555";
        seed_manifest(&a, guid, "<html><body>v1</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();

        // B 从 A 的基线出发（此刻与远端对齐），然后**远端又前进了一版**（A 改了别的篇）
        copy_lib(&a, &b);
        add_note(&a, other, "o", "<html><body>other</body></html>");
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        let remote_bytes = store.get(&cfg_of(&a).cloud_key("manifest.db")).await.unwrap();

        // B 也改了自己那一篇（有闩 ⇒ 会走到提交前判据），但它的基线已经旧了
        local_write(&b, guid, "<html><body>v1-from-b</body></html>");
        let err = sync_up(&store, &cfg_of(&b), &b, p).await.unwrap_err();
        assert!(err.contains("SYNC_MANIFEST_STALE"), "got: {err}");
        assert!(
            err.contains("云端未改动"),
            "错误信息必须点明云端没动过（否则用户会以为要人工修云端）: {err}"
        );
        assert_eq!(
            store.get(&cfg_of(&a).cloud_key("manifest.db")).await.unwrap(),
            remote_bytes,
            "拦下就必须**真的没写**：远端清单字节一字未动"
        );
        let c = manifest::open_readonly(&b).unwrap();
        assert!(
            !manifest::load_unsynced_note_guids(&c).unwrap().is_empty(),
            "被拦下 ⇒ 闩必须**保持置位**（改动没丢，对齐后还能再上行）"
        );
    }

    /// **§7.1 ②：条件 PUT 未命中（412）⇒ 回滚本轮铸版**。
    ///
    /// 这是"读—改—写窗口里被第二个写入端插队"的那个分支：服务端判条件不过，客户端必须把
    /// 刚铸的版本号与清掉的闩**整体回滚**，否则"本地以为已发布、云端其实没收到、闩又清了"
    /// ⇒ 这些改动永远不再上传。
    ///
    /// **变异检查**：把 `ConditionalPut::PreconditionFailed` 分支当成成功（或忽略回滚）→
    /// `assert_eq!(current_revision, rev1)` 与"闩仍置位"两条同时 FAILED。
    #[tokio::test]
    async fn test_up_conditional_put_412_rolls_back_mint() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("cond412-a");
        let guid = "aaaaaaaa-3333-4444-5555-666666666666";
        seed_manifest(&a, guid, "<html><body>v1</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        let rep1 = sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        let rev1 = rep1.local_revision;
        let remote_bytes = store.get(&cfg_of(&a).cloud_key("manifest.db")).await.unwrap();

        local_write(&a, guid, "<html><body>v2</body></html>");
        store.force_precondition_failed(); // 模拟"提交那一刻别人抢先写了"
        let err = sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap_err();
        assert!(err.contains("SYNC_MANIFEST_CONFLICT"), "got: {err}");
        assert!(err.contains("已回滚本轮铸版"), "got: {err}");
        assert_eq!(
            store.get(&cfg_of(&a).cloud_key("manifest.db")).await.unwrap(),
            remote_bytes,
            "412 ⇒ 远端清单不得被改动"
        );
        let c = manifest::open_readonly(&a).unwrap();
        assert_eq!(
            manifest::current_revision(&c),
            rev1,
            "本地铸版必须被回滚（否则本地编号凭空前进，与云端对不上）"
        );
        assert!(
            !manifest::load_unsynced_note_guids(&c).unwrap().is_empty(),
            "闩必须恢复为待上行（否则这次改动永远不再上传 = 静默丢数据）"
        );
        drop(c);

        // 反面：下一轮（不再注入 412）必须能正常补上 —— 回滚是可恢复的，不是死局
        let rep2 = sync_up(&store, &cfg_of(&a), &a, p).await.unwrap();
        assert_eq!(rep2.local_revision, rev1 + 1, "重试应正常铸版");
        assert!(rep2.manifest_uploaded);
    }

    /// **§7.1 ③：服务端不支持条件写 ⇒ 降级为无条件 PUT**（纵深防御不可用不该阻断同步）。
    ///
    /// **变异检查**：把"不支持"也当错误返回 → 本用例的 `unwrap` 直接 panic。
    #[tokio::test]
    async fn test_up_falls_back_when_conditional_put_unsupported() {
        let _s = crate::library::test_write_lock();
        isolate_wiz_home();
        let a = temp_root("cond501-a");
        let guid = "bbbbbbbb-3333-4444-5555-666666666666";
        seed_manifest(&a, guid, "<html><body>v1</body></html>");
        let store = MemStore::new();
        let p = noop_progress();
        sync_up(&store, &cfg_of(&a), &a, p.clone()).await.unwrap();
        local_write(&a, guid, "<html><body>v2</body></html>");
        store.force_precondition_unsupported();
        let rep = sync_up(&store, &cfg_of(&a), &a, p).await.unwrap();
        assert!(rep.manifest_uploaded, "降级后仍应完成提交");
        let c = manifest::open_readonly(&a).unwrap();
        assert!(manifest::load_unsynced_note_guids(&c).unwrap().is_empty(), "闩照样得清");
    }

    /// 承重⑦：`_conflicts/` GC —— mtime 超 30 天的留档被物理删除，未超期的保留。
    /// 与 `_trash/` 的 GC 同构：**只动物理文件、不写清单**（故不误伤任何判据）。
    ///
    /// **变异检查**：阈值改回"不删除"（或把 `days` 写死成 `u64::MAX`）→ 必 FAILED。
    #[test]
    fn test_gc_conflicts_retention_and_traversal_guard() {
        let root = temp_root("gcc");
        let dir = root.join(CONFLICT_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("aaaa-bbbb_3.zip");
        let fresh = dir.join("aaaa-bbbb_7.zip");
        std::fs::write(&old, vec![0u8; 64]).unwrap();
        std::fs::write(&fresh, vec![0u8; 8]).unwrap();
        // 把 old 的 mtime 拨到 31 天前（GC 按**文件 mtime**计龄）
        let back = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 86_400);
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(back)).unwrap();
        drop(f);

        let rep = gc_conflicts(&root, CONFLICT_RETENTION_DAYS).unwrap();
        assert_eq!(rep.removed, 1, "超期留档应被删除");
        assert_eq!(rep.bytes, 64);
        assert_eq!(rep.kept, 1, "未超期留档应保留");
        assert!(!old.exists(), "超期留档必须物理删除");
        assert!(fresh.exists(), "未超期留档不得被删");

        // 幂等：再跑一遍无副作用
        let rep2 = gc_conflicts(&root, CONFLICT_RETENTION_DAYS).unwrap();
        assert_eq!(rep2.removed, 0);

        // 路径穿越防线：符号链接指向 _conflicts 之外 → 拒绝删除
        let outside = temp_root("gcc-outside");
        std::fs::write(outside.join("victim.txt"), b"do-not-delete").unwrap();
        #[cfg(unix)]
        {
            let link = dir.join("escape.zip");
            std::os::unix::fs::symlink(outside.join("victim.txt"), &link).unwrap();
            let _ = gc_conflicts(&root, 0); // days=0 ⇒ 全部"超期"，专测穿越防线
            assert!(outside.join("victim.txt").exists(), "穿越目标不得被删");
        }
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D0 护栏：对象键的 `native/` 段是**冻结的协议字面量**（键即协议，不得改动）
    #[tokio::test]
    async fn test_object_keys_use_native_segment() {
        isolate_wiz_home();
        let src = temp_root("seg");
        let guid = "44444444-2222-3333-4444-555555555555";
        seed_manifest(&src, guid, "<html><body>payload</body></html>");
        let cfg = cfg_of(&src);
        let store = MemStore::new();
        sync_up(&store, &cfg, &src, noop_progress()).await.unwrap();
        let key = cfg.cloud_key(&format!("{KEY_NATIVE}/notes/{{{guid}}}"));
        assert!(store.head(&key).await.unwrap().is_some(), "键必须是 {key}");
        // 反向断言：不得存在没有格式段的键（键即协议，删段会改变全部对象路径）
        assert!(store.head(&cfg.cloud_key(&format!("notes/{{{guid}}}"))).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_manifest_uploaded_last_and_revision_meta() {
        isolate_wiz_home();
        let src = temp_root("src2");
        seed_manifest(&src, "22222222-2222-3333-4444-555555555555", "<html><body>x</body></html>");
        let cfg = cfg_of(&src);
        let store = MemStore::new();
        sync_up(&store, &cfg, &src, noop_progress()).await.unwrap();
        let h = store.head(&cfg.cloud_key("manifest.db")).await.unwrap().unwrap();
        // 清单 meta.revision = **提交点铸出的号**：建库时 seed 的 1 被铸成 2
        // （库内写不铸版、只有上行提交点能铸 —— 见 `docs/云同步逻辑.md` §4.1/§8）
        let expected = {
            let c = manifest::open_readonly(&src).unwrap();
            manifest::current_revision(&c).to_string()
        };
        assert_eq!(h.meta("revision").unwrap(), &expected);
        assert_eq!(expected, "2", "库内写不铸版、提交点才铸：1 → 2");
        assert_eq!(h.meta("content-md5").unwrap(), &manifest::md5_file(&src.join(manifest::MANIFEST_NAME)).unwrap());
        assert!(h.meta("updated-by").is_some());
    }

    #[tokio::test]
    async fn test_sync_down_without_remote_manifest_errors() {
        isolate_wiz_home();
        let dst = temp_root("dst2");
        let store = MemStore::new();
        let err = sync_down(&store, &cfg_of(&dst), &dst, noop_progress()).await.unwrap_err();
        assert!(err.contains("SYNC_NO_MANIFEST"));
    }

    #[test]
    fn test_gc_trash_retention_and_traversal_guard() {
        let root = temp_root("gc");
        let old = root.join("_trash").join("2020-01-01");
        let fresh = root.join("_trash").join(today_utc());
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(old.join("a.zip"), vec![0u8; 100]).unwrap();
        std::fs::write(fresh.join("b.zip"), vec![0u8; 7]).unwrap();

        let rep = gc_trash(&root, TRASH_RETENTION_DAYS).unwrap();
        assert_eq!(rep.removed, 1, "超期目录清理");
        assert_eq!(rep.bytes, 100);
        assert_eq!(rep.kept, 1, "当日目录保留");
        assert!(!old.exists());
        assert!(fresh.exists());

        // 幂等：再跑一遍无副作用
        let rep2 = gc_trash(&root, TRASH_RETENTION_DAYS).unwrap();
        assert_eq!(rep2.removed, 0);

        // 路径穿越防线：符号链接指向 _trash 之外 → 拒绝删除
        let outside = temp_root("outside");
        std::fs::write(outside.join("victim.txt"), b"do-not-delete").unwrap();
        let link = root.join("_trash").join(today_utc()).join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let rep3 = gc_trash(&root, TRASH_RETENTION_DAYS).unwrap();
        assert!(outside.join("victim.txt").exists(), "穿越目标不得被删");
        let _ = rep3;
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// U3 的"删除"侧断言：`write_compat_source_index` 已随之删除。
    /// 故这里不再有 WIZ_* 合成物的形状测试；空索引/无附件索引的容错由
    /// `manifest::populate_attachments_from_index` 自己的单测覆盖。
    /// 取而代之的是 **U5 冲突文件名口径**的廉价单测（下行/上行两侧共用它）。
    #[test]
    fn test_conflict_rel_path_shape() {
        assert_eq!(
            conflict_rel_path("55555555-2222-3333-4444-555555555555", 7),
            "_conflicts/55555555-2222-3333-4444-555555555555_7.zip"
        );
        // 带花括号的 guid 归一化（与云端键同一口径）
        assert_eq!(
            conflict_rel_path("{55555555-2222-3333-4444-555555555555}", 1),
            "_conflicts/55555555-2222-3333-4444-555555555555_1.zip"
        );
        // revision 不同 → 文件名不同 ⇒ 多次冲突都留得下（不会互相覆盖）
        assert_ne!(
            conflict_rel_path("g", 1),
            conflict_rel_path("g", 2),
            "带 revision 的意义就是多次冲突都能留存"
        );
    }

    #[test]
    fn test_days_from_civil_roundtrip() {
        let (y, m, d) = today_ymd();
        let days = days_from_civil(y, m, d);
        assert_eq!(parse_date(&format!("{y:04}-{m:02}-{d:02}")), Some(days));
        // 已知锚点：2026-09-17（1970-01-01 起算）
        assert_eq!(days_from_civil(2026, 9, 17), 20_713);
    }
}
