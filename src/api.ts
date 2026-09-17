// Tauri IPC 封装（核心服务接口，与 UI 解耦）
import { invoke } from '@tauri-apps/api/core'

export interface Settings {
  data_dir: string | null
  font_size: number
  theme: string
  read_width: number
  allow_remote: boolean
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

/** 每份笔记导出为一个 zip 的结果（FR-08.1 批量形态 + FR-02 瘦身统计 + 同步清单计数，Rust 侧 serde flatten 展开） */
export interface FolderZipExportReport extends ExportReport {
  slim: boolean
  slim_files_removed: number
  slim_bytes_removed: number
  slim_report_path: string | null
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
  /** 每份笔记导出为一个 zip；slim=true 时执行 FR-02 存储瘦身并出报告 */
  exportFolderZips: (location: string, dest: string, slim: boolean) =>
    invoke<FolderZipExportReport>('export_folder_zips_cmd', { location, dest, slim }),
  /** T4.1–T4.4 全库巡检；exportFull 会跑全库导出（产物约 2.5 GB，慎用） */
  runVerify: (withBench: boolean, exportFull: boolean) =>
    invoke<VerifyReport>('run_verify_cmd', { withBench, exportFull }),
  /** 落盘验收报告（返回 [md 路径, json 路径]） */
  saveVerifyReport: (dest: string, report: VerifyReport) =>
    invoke<string[]>('save_verify_report', { dest, report }),
  pickDefaultDataDir: (dir: string) => invoke<void>('pick_default_data_dir', { dir }),
  detectWiznoteDir: () => invoke<string | null>('detect_wiznote_dir'),
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
