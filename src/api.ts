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
  skipped: string[]
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
    invoke<void>('export_note_zip_cmd', { guid, dest }),
  exportNoteHtml: (guid: string, dest: string) =>
    invoke<void>('export_note_html_cmd', { guid, dest }),
  exportFolder: (location: string, dest: string) =>
    invoke<ExportReport>('export_folder_cmd', { location, dest }),
  pickDefaultDataDir: (dir: string) => invoke<void>('pick_default_data_dir', { dir }),
  detectWiznoteDir: () => invoke<string | null>('detect_wiznote_dir'),
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}
