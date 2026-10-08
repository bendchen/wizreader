// Tauri IPC 封装（核心服务接口，与 UI 解耦）
import { invoke } from '@tauri-apps/api/core'

export interface Settings {
  /** 主数据目录（笔记库根，可读可写） */
  library_dir: string | null
  /** 源数据目录（为知原始数据，只读，仅用于导入/导出） */
  source_dir: string | null
  font_size: number
  theme: string
  read_width: number
  allow_remote: boolean
}

/**
 * 视图上下文（与 Rust `commands::ViewContext` 对齐）：
 * - `none`：未打开任何笔记（未设置数据目录，或数据目录的清单不可读）—— 不展示任何笔记
 * - `library`：自有笔记库
 * - `source`：为知笔记原始数据（只读）—— 只能由用户显式进入
 */
export type ViewContext = 'none' | 'library' | 'source'

/** 库准入校验四态（与 Rust `library::LibraryStatus` 对齐） */
export interface LibraryStatus {
  /** empty / ready / no_manifest / rejected */
  kind: string
  reason: string
  note_count: number
  missing_files: number
  warnings: string[]
  dir: string
}

/** 库状态（启动自检 / 设置页；与 Rust `commands::LibraryStatusView` 对齐） */
export interface LibraryStatusView {
  library_dir: string | null
  source_dir: string | null
  context: ViewContext
  /** 库清单（export.db）当前是否可读 —— 「是否打开笔记」的唯一判据 */
  library_readable: boolean
  /** 不可读的原因（library_readable=false 时非空），空态页如实展示 */
  library_error: string
  /**
   * 库准入分类：unset / empty（空目录，待导入）/ no_manifest（有 zip 无清单）/
   * rejected（不可用作库）/ ready
   */
  library_kind: string
  manifest_notes: number
  index_present: boolean
  index_notes: number
  consistent: boolean
}

/** 「导入到我的笔记库」结果（与 Rust `commands::ImportToLibraryReport` 对齐） */
export interface ImportToLibraryReport {
  export: FolderZipExportReport
  attachments_copied: number
  attachments_missing: number
  index: BuildReport
}

export interface BuildReport {
  note_count: number
  source_note_count: number
  package_count: number
  tier1: number
  tier2: number
  tier3: number
  tier4: number
  db_missing: number
  elapsed_ms: number
  warnings: string[]
  ok: boolean
}

export interface TreeNode {
  path: string
  name: string
  pos: number | null
  direct_count: number
  total_count: number
  has_attachment: boolean
  is_system: boolean
  children: TreeNode[]
}

export interface NoteItem {
  guid: string
  title: string
  location: string
  data_modified: string
  created: string
  has_attachment: boolean
  is_webclip: boolean
  doc_type: string
  body_text_length: number
  package_size: number
  is_empty: boolean
}

export interface AttachmentItem {
  file_path: string
  display_name: string
  size: number
  tier: number
  source: string
  exists: boolean
  origin: string
}

export interface NoteDetail {
  guid: string
  title: string
  location: string
  url: string | null
  created: string
  data_modified: string
  package_size: number
  body_text_length: number
  /**
   * 正文形态（M4）：`'md'` = 包内 `note.md`；`'html'` = 为知原生 `index.html`；
   * `null` = 该篇正文条目读不到（包缺失/损坏）—— 此时**不显示形态徽标**，
   * 不要把 null 当成 `'html'`（那会把"读不到"谎报成一种形态）。
   */
  body_format: 'md' | 'html' | null
  attachments: AttachmentItem[]
}

export interface SearchResult {
  guid: string
  title: string
  location: string
  data_modified: string
  fingerprint: string
  snippet: string
  title_hl: string
  dup_count: number
  hit_in: string
}

export interface AttachmentHit {
  file_path: string
  display_name: string
  document_guid: string | null
  tier: number
  source: string
  size: number
}

export interface SearchResponse {
  kw: string
  notes: SearchResult[]
  attachments: AttachmentHit[]
  elapsed_ms: number
}

export interface AttachmentPreview {
  name: string
  size: number
  is_text: boolean
  content: string
}

export interface ExportReport {
  notes_exported: number
  attachments_exported: number
  attachments_missing: number
  folders_exported: number
  code_blocks_materialized: number
  skipped: string[]
  elapsed_ms: number
}

/** 每份笔记导出为一个 zip 的结果（FR-08.1 批量形态 + 同步清单计数，Rust 侧 serde flatten 展开）。
 *  **D0**：只产 native（源 zip 字节级拷贝），已无 slim 统计字段 */
export interface FolderZipExportReport extends ExportReport {
  /** 同步清单 export.db 路径（云端同步数据分析.md §5） */
  manifest_path: string | null
  /** 清单四类计数：新增 / 重导 / 复用（零重写）/ 墓碑 */
  notes_added: number
  notes_reexported: number
  notes_reused: number
  notes_removed: number
  /** 清单不变量自检警告（§5.5） */
  manifest_warnings: string[]
}

/** M4 验收巡检的单项结果（与 Rust `verify::Check` 对齐） */
export interface VerifyCheck {
  id: string
  name: string
  passed: boolean
  skipped: boolean
  actual: string
  expected: string
  samples: string[]
}

export interface VerifyReport {
  ok: boolean
  data_dir: string
  inspection: VerifyCheck[]
  export_check: VerifyCheck[]
  security: VerifyCheck[]
  zero_write: VerifyCheck[]
  bench: VerifyCheck[]
  elapsed_ms: number
}

// ---------- 云同步（FR-07 阶段二）----------

/** 与 Rust `config::SyncSettings` 对齐；secret key 永不出现在此结构（存 keyring） */
export interface SyncSettings {
  enabled: boolean
  /** "writer"（写入端）| "reader"（只读端）；旧值 "export" 由后端归一为 "writer" */
  role: string
  /**
   * @deprecated U1 起同步根**恒为笔记库根**（settings.library_dir），本字段不再参与任何路径推导。
   * 仅为与后端结构对齐而保留（后端迁移后会把它清空）；UI 不应再提供输入框。
   */
  local_root: string
  endpoint: string
  bucket: string
  prefix: string
  region: string
  path_style: boolean
  access_key_id: string
  credential_user: string
  concurrency: number
  auto_check_on_start: boolean
  initialized: boolean
}

export interface SyncConfigView {
  config: SyncSettings
  credential_set: boolean
}

export interface TestConnectionResult {
  ok: boolean
  latency_ms: number
  can_read: boolean
  can_write: boolean
  objects_under_prefix: number
  message: string
}

export interface SyncFailure {
  key: string
  reason: string
  retryable: boolean
}

/** 与 Rust `sync::SyncReport` 对齐（serde 默认字段名） */
export interface SyncReport {
  /** "up" | "down" | "none" */
  direction: string
  uploaded: number
  uploaded_bytes: number
  downloaded: number
  downloaded_bytes: number
  skipped: number
  /** U5：因远端版本更高而被留存到 `_conflicts/` 的篇数 */
  conflicts: number
  trashed: number
  oversized: number
  failures: SyncFailure[]
  manifest_uploaded: boolean
  remote_revision: number | null
  local_revision: number
  elapsed_ms: number
}

export interface GcReport {
  removed: number
  bytes: number
  kept: number
}

export interface SyncStatusView {
  enabled: boolean
  role: string
  initialized: boolean
  last_sync_at: string | null
  last_report: SyncReport | null
  local_revision: number
  trash_items: number
  trash_bytes: number
}

export interface TrashItem {
  guid: string
  title: string
  /** 删除前的库内相对路径 */
  last_path: string
  /** `_trash/` 下的实际相对路径（旧墓碑为 null） */
  trash_rel: string | null
  removed_at: string
  size: number
  /** 磁盘上是否找得到可恢复的文件 */
  restorable: boolean
  /** 不可恢复的原因（restorable=false 时非空） */
  reason: string
  days_left: number
  /** 墓碑是否带恢复载荷（false = v3 及更早墓碑：只能恢复文件，不能还原清单行） */
  has_snapshot: boolean
}

/** 回收站统计（与 Rust `commands::TrashStats` 对齐；根 = 库根） */
export interface TrashStats {
  root: string
  items: number
  bytes: number
  retention_days: number
}

/** 一次库内写操作的结果（与 Rust `library::NoteWriteReport` 对齐，T5/T8） */
export interface NoteWriteReport {
  op: string
  guid: string
  title: string
  exported_path: string
  exported_size: number
  exported_md5: string
  data_modified: string
  /** 行级 revision（写后；delete 为 0 —— 行已移入墓碑） */
  revision: number
  manifest_revision: number
  /** 派生索引是否已同步更新（false = 需重建索引；**不是**写失败） */
  index_updated: boolean
  warnings: string[]
}

/**
 * 正文源码（编辑抽屉初值；与 Rust `commands::NoteSource` 对齐，M3/§20.8）。
 *
 * `format` 是**库内包形态**，不是用户偏好：`md` = 包内正文是 `note.md`（Markdown 源），
 * `html` = 为知原生包（`index.html`）。编辑器据此切换语法与保存口径 ——
 * 保存时形态由包决定（后端按包内实况自动分派），前端不必也不能指定。
 */
export interface NoteSource {
  format: 'md' | 'html'
  /** 正文源文本（BOM 已剥） */
  text: string
}

/**
 * M4 图片插入结果（与 Rust `library::NoteImageReport` 对齐）。
 * `entry` 就是写进正文的相对引用（`index_files/…`）；`reused=true` 表示
 * 包内已有同内容条目、本次零写入复用（同一张图重复插入不重写包）。
 */
export interface NoteImageReport {
  entry: string
  reused: boolean
  op: string
  guid: string
  exported_path: string
  exported_size: number
  exported_md5: string
  data_modified: string
  revision: number
  manifest_revision: number
  index_updated: boolean
  warnings: string[]
}

export const api = {
  getSettings: () => invoke<Settings>('get_settings'),
  updateSettings: (s: Settings) => invoke<void>('update_settings', { settings: s }),
  checkIndex: () => invoke<boolean>('check_index'),
  buildIndex: () => invoke<BuildReport>('build_index_cmd'),
  getTree: () => invoke<TreeNode[]>('get_tree'),
  listNotes: (folder: string | null, sort: string | null, filter: string | null) =>
    invoke<NoteItem[]>('list_notes', { folder, sort, filter }),
  getNoteDetail: (guid: string) => invoke<NoteDetail>('get_note_detail', { guid }),
  previewAttachment: (filePath: string) =>
    invoke<AttachmentPreview>('preview_attachment', { filePath }),
  openAttachmentExternal: (filePath: string) =>
    invoke<void>('open_attachment_external', { filePath }),
  revealInFinder: (filePath: string) => invoke<void>('reveal_in_finder', { filePath }),
  saveAttachmentAs: (filePath: string, dest: string) =>
    invoke<void>('save_attachment_as', { filePath, dest }),
  openExternalUrl: (url: string) => invoke<void>('open_external_url', { url }),
  search: (kw: string, folder: string | null) =>
    invoke<SearchResponse>('search_cmd', { kw, folder }),
  getSearchHistory: () => invoke<[string, number][]>('get_search_history'),
  clearSearchHistory: () => invoke<void>('clear_search_history'),
  getUnlinked: () => invoke<AttachmentItem[]>('get_unlinked'),
  exportNoteZip: (guid: string, dest: string) =>
    invoke<ExportReport>('export_note_zip_cmd', { guid, dest }),
  exportNoteHtml: (guid: string, dest: string) =>
    invoke<ExportReport>('export_note_html_cmd', { guid, dest }),
  exportFolder: (location: string, dest: string) =>
    invoke<ExportReport>('export_folder_cmd', { location, dest }),
  /** 每份笔记导出为一个 zip（**D0**：恒 native 无损，无模式参数） */
  exportFolderZips: (location: string, dest: string) =>
    invoke<FolderZipExportReport>('export_folder_zips_cmd', { location, dest }),
  /** T4.1–T4.4 全库巡检；exportFull 会跑全库导出（产物约 2.5 GB，慎用） */
  runVerify: (withBench: boolean, exportFull: boolean) =>
    invoke<VerifyReport>('run_verify_cmd', { withBench, exportFull }),
  /** 落盘验收报告（返回 [md 路径, json 路径]） */
  saveVerifyReport: (dest: string, report: VerifyReport) =>
    invoke<string[]>('save_verify_report', { dest, report }),
  pickDefaultDataDir: (dir: string) => invoke<void>('pick_default_data_dir', { dir }),
  detectWiznoteDir: () => invoke<string | null>('detect_wiznote_dir'),

  // ---------- 笔记库（FR-11 P0/P1）----------
  /** 选择主数据目录（笔记库）：红线校验 + 准入校验，返回 LibraryStatus 供分支 */
  pickLibraryDir: (dir: string) => invoke<LibraryStatus>('pick_library_dir', { dir }),
  /** 构建库派生索引（后台全量，进度走 index-progress 事件） */
  buildLibraryIndex: () => invoke<BuildReport>('build_library_index_cmd'),
  /** 导入源目录到我的笔记库（目标固定=库根；进度走 export-progress/index-progress） */
  importToLibrary: (location: string) =>
    invoke<ImportToLibraryReport>('import_to_library', { location }),
  /** 库状态（库根、上下文、清单/索引篇数一致性） */
  getLibraryStatus: () => invoke<LibraryStatusView>('get_library_status'),
  /** 切换视图上下文：'library' | 'source' | 'none' */
  setViewContext: (context: ViewContext) => invoke<ViewContext>('set_view_context', { context }),
  /** 空库初始化：目录无清单（kind=empty）时建立空 export.db 并切库上下文（幂等） */
  initLibraryManifest: () => invoke<void>('init_library_manifest'),
  /** 库内新建目录（磁盘 + 索引 folder 表；返回规范化 location `/a/b/`） */
  createLibraryFolder: (path: string) => invoke<string>('create_library_folder', { path }),

  // ---------- 笔记库编辑（FR-11 P2 / S2；M3 起形态自适应）----------
  /** 新建笔记（origin=local）：生成 guid + md 包 + 清单插行（置脏）+ 单篇索引 */
  createNote: (title: string, location: string, md: string) =>
    invoke<NoteWriteReport>('create_note_cmd', { title, location, md }),
  /**
   * 读正文源码（编辑抽屉初值；不含宿主注入与 CSP，可直接写回）。
   * 形态不写死在命令名里：md 包返回 `note.md` 的 Markdown、native 包返回 `index.html` 的 HTML。
   */
  getNoteSource: (guid: string) => invoke<NoteSource>('get_note_source', { guid }),
  /** 该篇在磁盘上的绝对路径（在访达中显示用；库模式按清单解析，前端不拼路径） */
  getNoteFilePath: (guid: string) => invoke<string>('get_note_file_path', { guid }),
  /**
   * 登记预览草稿：返回 token，预览用 `wiznote://{guid}/index.html?draft=<token>`。
   * 这样预览与阅读走同一条协议（相对 index_files/ 能解析、CSP 与兼容层一致），
   * 而草稿**只在带 token 的请求上生效**，不会污染普通阅读。
   *
   * `text` 的形态由**包内实况**决定（后端自行判定并渲染：md 草稿渲染成 HTML 再喂 iframe），
   * 故这里只传文本、不传格式 —— 前端也就没有传错格式的可能。
   */
  setNoteDraft: (guid: string, text: string) =>
    invoke<string>('set_note_draft', { guid, text }),
  clearNoteDraft: () => invoke<void>('clear_note_draft'),
  /**
   * 保存正文：**按包内形态自动分派** —— md 包原子重写 `note.md`，native 包原子重写
   * `index.html`；两者共用同一条路径（整包搬运 + 清单事务 + 单篇索引增量）。
   */
  saveNoteSource: (guid: string, text: string) =>
    invoke<NoteWriteReport>('save_note_source_cmd', { guid, text }),
  /**
   * M4：把一张图片写入笔记包 `index_files/`（文件选择器路径 —— 后端读用户刚选的文件，
   * 不给 WebView 发"按路径读文件"的万能钥匙）。返回的 `entry` 直接填进正文引用。
   */
  addNoteImageFile: (guid: string, filePath: string) =>
    invoke<NoteImageReport>('add_note_image_file_cmd', { guid, filePath }),
  /** M4：粘贴路径 —— 图片字节以 base64 过 IPC，写入同一个包内位置 */
  addNoteImageData: (guid: string, dataB64: string, name: string | null) =>
    invoke<NoteImageReport>('add_note_image_data_cmd', { guid, dataB64, name }),
  /** M4 最小版：把任意文件作为附件写入笔记包 `attachments/`，正文插链接引用 */
  addNoteAttachment: (guid: string, filePath: string) =>
    invoke<NoteImageReport>('add_note_attachment_cmd', { guid, filePath }),
  /** 重命名标题（落地文件名随标题变，云端键不变） */
  renameNote: (guid: string, newTitle: string) =>
    invoke<NoteWriteReport>('rename_note_cmd', { guid, newTitle }),
  /** 移动到目录（newLocation 为知形态，如 `/工作/子目录/`） */
  moveNote: (guid: string, newLocation: string) =>
    invoke<NoteWriteReport>('move_note_cmd', { guid, newLocation }),
  /** 删除 → 移入库根 `_trash/` + 写墓碑 */
  deleteNote: (guid: string) => invoke<NoteWriteReport>('delete_note_cmd', { guid }),
  /** 从回收站恢复（文件回原路径 + 清单行逐字段还原） */
  restoreNote: (guid: string) => invoke<NoteWriteReport>('restore_trash', { guid }),
  /** 回收站统计（库根 `_trash/` 实况） */
  getTrashStats: () => invoke<TrashStats>('trash_stats'),

  // ---------- 云同步（FR-07 阶段二）----------
  getSyncConfig: () => invoke<SyncConfigView>('get_sync_config'),
  /** 用待保存的值测试连接（不必先落盘）；secretKey 为空时使用钥匙串旧值 */
  testCloudConnection: (config: SyncSettings, secretKey: string) =>
    invoke<TestConnectionResult>('test_cloud_connection', { config, secretKey }),
  /** 保存配置；secretKey 非空时写入 keyring（此后不再下发） */
  saveSyncConfig: (config: SyncSettings, secretKey: string | null) =>
    invoke<void>('save_sync_config', { config, secretKey }),
  clearCloudCredentials: () => invoke<void>('clear_cloud_credentials'),
  // U1：`pick_sync_root` 已删 —— 同步根恒为笔记库根，唯一可选的根是 `pickLibraryDir`。
  /** 首次初始化（按角色分流，后台执行，进度走 sync-progress 事件）；role: "writer" | "reader"
   *  【真云 GUI 实测发现（2026-09-20）】后端返回 `InitReport { report: SyncReport }`（嵌套一层），
   *  不是扁平 SyncReport —— 之前声明成 SyncReport 导致 `syncReportSummary(rep)` 读
   *  `rep.failures.length` 抛 TypeError（初始化报告永远渲染失败）。 */
  initCloudSync: (role: string) => invoke<{ report: SyncReport }>('init_cloud_sync', { role }),
  /** direction: "up" | "down" */
  runSync: (direction: string) => invoke<SyncReport>('run_sync', { direction }),
  getSyncStatus: () => invoke<SyncStatusView>('get_sync_status'),
  listTrash: () => invoke<TrashItem[]>('list_trash'),
  /** beforeDays > 0 时只清理早于该天数的目录；否则用默认保留期 30 天 */
  purgeTrash: (beforeDays: number) => invoke<GcReport>('purge_trash', { beforeDays }),
  openTrashDir: () => invoke<void>('open_trash_dir'),
}

/**
 * 写操作错误码 → 人话（T8 硬要求：**保存失败必须显示具体错误码，不静默吞错**）。
 *
 * 后端一律以 `CODE: 说明` 的形态返回（见 `library.rs` / `commands.rs`），
 * 这里保留原码并补一句"该怎么办"：只说"保存失败"等于让用户无从下手；
 * 而把码藏起来，用户报问题时我们也没法定位。
 */
const WRITE_HINTS: Record<string, string> = {
  LOCK_BUSY: '当前有同步或另一个写操作在进行，请稍后重试',
  WRONG_CONTEXT: '该操作只能在「我的笔记库」视图下进行（为知笔记视图是只读的）',
  READER_READONLY:
    '本机角色是「只读端」（reader）：它以云端为准、不接受本地写入。请到写入端修改后再同步过来',
  NO_CONTEXT: '尚未打开任何笔记库',
  LIB_NO_MANIFEST: '数据目录里没有清单（export.db）',
  NOTE_NOT_FOUND: '库内已无此篇（可能已被删除或在别处改动）',
  NOTE_PACKAGE_MISSING: '清单里有该篇，但磁盘上找不到它的 zip',
  NOTE_ALREADY_EXISTS: '清单里已有该篇，未覆盖',
  PATH_TAKEN: '目标路径已被占用，请先移走或改名',
  PATH_UNSAFE: '目标路径不安全（含绝对路径或 ..），已拒绝',
  EMPTY_TITLE: '标题不能为空',
  EMPTY_HTML: '正文为空，已拒绝保存（避免清空笔记）',
  EMPTY_MD: '正文为空，已拒绝保存（避免清空笔记）',
  HTML_HOST_REF: '正文含 wiznote:// 宿主引用，会污染导出与巡检，请删除后再保存',
  MD_HOST_REF: '正文含 wiznote:// 宿主引用，会污染导出与巡检，请删除后再保存',
  ENTRY_NOT_FOUND: '该篇 zip 内没有 index.html（本期只改已有正文，不新建）',
  ENTRY_EXISTS: '包内已有同名条目（追加不覆盖）',
  NOT_IMAGE: '不是可识别的图片（只支持 png/jpg/gif/webp/bmp/svg）',
  IMAGE_TOO_LARGE: '图片超过 20 MB 上限（大素材请走附件，不要塞进正文包）',
  EMPTY_IMAGE: '图片内容为空',
  IMAGE_READ_FAILED: '读取所选图片文件失败',
  IMAGE_B64_DECODE: '剪贴板图片数据解码失败',
  ATTACHMENT_TOO_LARGE: '附件超过 50 MB 上限（更大素材请等分档策略，不要塞进正文包）',
  EMPTY_ATTACHMENT: '附件内容为空',
  ATTACHMENT_READ_FAILED: '读取所选附件文件失败',
  TOMBSTONE_NOT_FOUND: '回收站里没有这一篇（可能已经恢复过）',
  NO_TOMBSTONE_PAYLOAD: '该墓碑来自旧版本，没有恢复载荷 —— 无法还原清单行',
  TRASH_FILE_MISSING: '回收站里已找不到该篇的文件（可能已被超期清理）',
  MANIFEST_BUSY: '清单正被占用，请稍后重试',
}

/** 从任意错误里抽出 `CODE:` 前缀；没有码时返回空串 */
export function errorCode(e: unknown): string {
  const m = /^\s*([A-Z][A-Z0-9_]{2,}):/.exec(String(e))
  return m ? m[1] : ''
}

/** 写操作失败的展示文本：`具体错误码 + 原因 + 建议` */
export function writeErrorText(e: unknown): string {
  const raw = String(e)
  const code = errorCode(e)
  const hint = code ? WRITE_HINTS[code] : ''
  const head = code ? `【${code}】` : ''
  return `${head}${raw}${hint ? `\n\n建议：${hint}` : ''}`
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

/**
 * 导出/保存用的文件名净化（P3），与 Rust `extract::sanitize_title` 保持一致：
 * 替换文件系统非法字符、剔控制字符、去尾部空格与点、兜底 untitled、截 120 字符
 */
export function safeFileName(title: string): string {
  const t = title
    .replace(/[\\/:*?"<>|]/g, '_')
    .replace(/[\u0000-\u001f]/g, '')
    .trim()
    .replace(/\.+$/, '')
  return (t || 'untitled').slice(0, 120)
}

/**
 * 目录/全库导出的结果摘要（FR-08）：目录面板、设置页、阅读区三处入口共用一份口径，
 * 避免各处文案不一致。skipped 仅列前 3 项，避免弹窗装不下。
 */
export function folderExportSummary(r: ExportReport): string {
  return (
    `导出完成：${r.notes_exported} 篇 / ${r.attachments_exported} 个附件` +
    `（缺 ${r.attachments_missing} 个），还原 ${r.folders_exported} 个目录，` +
    `物化代码块 ${r.code_blocks_materialized} 处，耗时 ${r.elapsed_ms} ms` +
    (r.skipped.length
      ? `\n跳过 ${r.skipped.length} 项：${r.skipped.slice(0, 3).join('；')}${r.skipped.length > 3 ? ' …' : ''}`
      : '')
  )
}
