/**
 * 导出动作（FR-08 逃生舱）：目录/全库与单篇两类。
 * 选目标目录、调用后端命令、弹结果摘要都收在这里，供顶栏导出菜单等入口复用，
 * 避免各处口径不一致。全部失败以 alert 呈现，返回是否已执行导出。
 */
import {
  api,
  folderExportSummary,
  safeFileName,
  type FolderZipExportReport,
  type ExportReport,
} from './api'

/** 单篇导出结果摘要（zip / 自包含 HTML 共用） */
export function noteExportSummary(
  r: Pick<ExportReport, 'attachments_exported' | 'attachments_missing' | 'code_blocks_materialized' | 'elapsed_ms'>,
  extra: string
): string {
  return (
    `导出完成（${extra}）：附件 ${r.attachments_exported} 个` +
    (r.attachments_missing ? `，缺失 ${r.attachments_missing} 个` : '') +
    `，物化代码块 ${r.code_blocks_materialized} 处，耗时 ${r.elapsed_ms} ms`
  )
}

/** 导出一个目录（含子目录）；location 传 '' 表示全库。title 覆盖选目录弹窗的标题 */
export async function exportNotesTo(location: string, title?: string): Promise<boolean> {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({
    directory: true,
    title: title ?? (location ? `导出目录「${location}」（含子目录）到…` : '导出全部笔记到…'),
  })
  if (typeof dir !== 'string') return false
  try {
    alert(folderExportSummary(await api.exportFolder(location, dir)))
    return true
  } catch (e) {
    alert(String(e))
    return false
  }
}

/**
 * 批量导出「每份笔记一个 zip」（FR-08.1 批量形态）。
 * **D0**：只产 native —— 源 zip 的字节级原样拷贝（无损、不做任何删减），
 * 因此不再询问「是否瘦身」（原 FR-02 slim 模式已整体取消；论证见 docs/本地笔记读写实现.md §4.6）。
 */
export async function exportNotesAsZips(location: string, title?: string): Promise<boolean> {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({
    directory: true,
    title:
      title ?? (location ? `导出目录「${location}」为每篇一个 zip 到…` : '导出全部笔记为每篇一个 zip 到…'),
  })
  if (typeof dir !== 'string') return false
  try {
    alert(zipExportSummary(await api.exportFolderZips(location, dir)))
    return true
  } catch (e) {
    alert(String(e))
    return false
  }
}

/** 每份笔记一个 zip 的结果摘要（D0：只有 native 一种产物） */
function zipExportSummary(r: FolderZipExportReport): string {
  return (
    `导出完成：${r.notes_exported} 篇笔记各为一个 zip（native：源 zip 逐字节拷贝，无损），` +
    `还原 ${r.folders_exported} 个目录，耗时 ${r.elapsed_ms} ms` +
    // 同步清单计数（云端同步数据分析.md §5/§11 阶段一）
    `\n清单 export.db：新增 ${r.notes_added} / 重导 ${r.notes_reexported} / 复用 ${r.notes_reused}` +
    (r.notes_removed ? ` / 墓碑 ${r.notes_removed}` : '') +
    (r.manifest_path ? `（${r.manifest_path}）` : '') +
    (r.manifest_warnings.length
      ? `\n⚠ 清单自检警告 ${r.manifest_warnings.length} 条：` +
        r.manifest_warnings.slice(0, 3).join('；') +
        (r.manifest_warnings.length > 3 ? ' …' : '')
      : '') +
    (r.skipped.length
      ? `\n跳过 ${r.skipped.length} 项：${r.skipped.slice(0, 3).join('；')}${r.skipped.length > 3 ? ' …' : ''}`
      : '')
  )
}

/** 导出单篇笔记：kind = zip（随包附件）或 html（资源全内联自包含） */
export async function exportNoteAs(kind: 'zip' | 'html', guid: string, title: string): Promise<boolean> {
  const { save } = await import('@tauri-apps/plugin-dialog')
  const dest = await save({ defaultPath: `${safeFileName(title)}.${kind}` })
  if (!dest) return false
  try {
    const r =
      kind === 'zip'
        ? await api.exportNoteZip(guid, dest)
        : await api.exportNoteHtml(guid, dest)
    alert(noteExportSummary(r, kind === 'zip' ? '含物化代码块与附件' : '自包含 HTML'))
    return true
  } catch (e) {
    alert(String(e))
    return false
  }
}
