/**
 * 导出动作（FR-08 逃生舱）：目录/全库与单篇两类。
 * 选目标目录、调用后端命令、弹结果摘要都收在这里，供顶栏导出菜单等入口复用，
 * 避免各处口径不一致。全部失败以 alert 呈现，返回是否已执行导出。
 */
import {
  api,
  folderExportSummary,
  formatSize,
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
 * 批量导出「每份笔记一个 zip」（FR-08.1 批量形态）：
 * 选目标目录后二次确认是否 FR-02 存储瘦身（默认关闭，确定= 瘦身，取消= 继承为知原生 zip 格式）。
 */
export async function exportNotesAsZips(location: string, title?: string): Promise<boolean> {
  // 同时引入插件的 confirm：绝不能用全局 window.confirm——dialog 插件注入的 shim
  // 调用不存在的 plugin:dialog|confirm 命令（2.7.3 只注册 open/save/message），必然 reject
  const { open, confirm } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({
    directory: true,
    title:
      title ?? (location ? `导出目录「${location}」为每篇一个 zip 到…` : '导出全部笔记为每篇一个 zip 到…'),
  })
  if (typeof dir !== 'string') return false
  try {
    // FR-02：瘦身默认关闭，需用户显式触发并二次确认（确定/Ok = 瘦身，取消/Cancel = 原生）
    const slim = await confirm(
      '是否进行存储瘦身（FR-02）？\n\n' +
        '瘦身仅保留 index.html 与被引用的资源，体积约可降 45%（实测解压口径 2.4 GB → 1.3 GB），\n' +
        '并在导出目录生成「瘦身报告.csv」供抽查；全程不修改原始数据。\n\n' +
        '「确定」= 瘦身导出；「取消」= 原样导出（继承为知原生 zip 格式，速度更快）',
      { title: '导出格式选择', kind: 'info' }
    )
    alert(zipExportSummary(await api.exportFolderZips(location, dir, slim)))
    return true
  } catch (e) {
    alert(String(e))
    return false
  }
}

/** 每份笔记一个 zip 的结果摘要（原生格式 / 瘦身两种模式共用） */
function zipExportSummary(r: FolderZipExportReport): string {
  return (
    `导出完成：${r.notes_exported} 篇笔记各为一个 zip（${r.slim ? '已瘦身' : '原生格式'}），` +
    `还原 ${r.folders_exported} 个目录，耗时 ${r.elapsed_ms} ms` +
    // 同步清单计数（云端同步数据分析.md §5/§11 阶段一）
    `\n清单 export.db：新增 ${r.notes_added} / 重导 ${r.notes_reexported} / 复用 ${r.notes_reused}` +
    (r.notes_removed ? ` / 墓碑 ${r.notes_removed}` : '') +
    (r.manifest_path ? `（${r.manifest_path}）` : '') +
    (r.slim
      ? `\n瘦身：删除冗余资源 ${r.slim_files_removed} 个 / ${formatSize(r.slim_bytes_removed)}` +
        (r.slim_report_path ? `\n瘦身报告：${r.slim_report_path}` : '')
      : '') +
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
