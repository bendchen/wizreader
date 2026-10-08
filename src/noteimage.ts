/**
 * M4 图片插入 / 粘贴的**纯逻辑**（零依赖、零 IPC —— `npm run test:logic` 可直接断言）。
 *
 * 分工边界：
 * - **类型判定不在前端**：图片格式嗅探（魔数 → 扩展名）由后端 `library::sniff_image_ext`
 *   负责 —— 剪贴板里的图常常没有文件名/扩展名，前端报的 MIME 也不可信；
 * - 本模块只做：粘贴载荷判定（有没有图、以什么形态出现）、data: URI 的提取与替换
 *   （网页复制来的内嵌图）、base64 编解码（图片字节过 IPC）、引用片段拼装
 *   （md → `![alt](index_files/…)`；html → `<img src="index_files/…">`）。
 */

/** 一次粘贴事件里同步抽出的载荷（在 `preventDefault` 之前抽，ClipboardEvent 之后即失效） */
export interface PastePayload {
  /** 剪贴板里的图片文件（截图 / 复制的图片） */
  files: File[]
  /** text/plain（可能有，也可能没有） */
  text: string
  /** text/html（富文本来源常有；内嵌图多为 data: URI） */
  html: string
}

export type PasteKind = 'none' | 'files' | 'text-data' | 'html-data'

/** data:image 的 base64 URI（非全局版：判定用；matchAll 提取用全局版）。
 *  base64 段用 `[A-Za-z0-9+/]+={0,2}`：`=` 只能是末尾 padding（最多 2 个），
 *  否则 `…==b` 会把后随字符也吞进 URI，替换时把正文吃掉。 */
const DATA_URI_ONE = /data:image\/[a-z0-9.+-]+;base64,[A-Za-z0-9+/]+={0,2}/
const DATA_URI_ALL = /data:image\/[a-z0-9.+-]+;base64,[A-Za-z0-9+/]+={0,2}/g

/** 判定这次粘贴里"图"以什么形态出现。`none` = 没有图 → 走浏览器默认粘贴。 */
export function decidePaste(p: PastePayload): PasteKind {
  if (p.files.length > 0) return 'files'
  if (DATA_URI_ONE.test(p.text)) return 'text-data'
  if (/<img\s/i.test(p.html) && DATA_URI_ONE.test(p.html)) return 'html-data'
  return 'none'
}

/** 从事件里同步抽取载荷（只在 preventDefault 之前调用一次） */
export function buildPastePayload(e: ClipboardEvent): PastePayload {
  const dt = e.clipboardData
  const files: File[] = []
  if (dt) {
    for (const it of Array.from(dt.items)) {
      if (it.kind === 'file' && it.type.startsWith('image/')) {
        const f = it.getAsFile()
        if (f) files.push(f)
      }
    }
  }
  return {
    files,
    text: dt?.getData('text/plain') ?? '',
    html: dt?.getData('text/html') ?? '',
  }
}

/** 提取文本里全部 data: 图片 URI（保持出现顺序与原文精确串，替换键即用它） */
export function extractDataUris(text: string): string[] {
  return [...text.matchAll(DATA_URI_ALL)].map((m) => m[0])
}

/** 按 extractDataUris 给出的精确串替换；map 里没有的保持原样 */
export function replaceDataUris(text: string, map: Record<string, string>): string {
  return text.replace(DATA_URI_ALL, (whole) => map[whole] ?? whole)
}

/** data: URI → 纯 base64 段 */
export function dataUriToBase64(uri: string): string {
  const i = uri.indexOf('base64,')
  return i >= 0 ? uri.slice(i + 7) : ''
}

/** base64 → 字节（粘贴路径把图片字节交给后端用） */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/** 字节 → base64（分块避免 String.fromCharCode 的栈上限） */
export function bytesToBase64(bytes: Uint8Array): string {
  let s = ''
  const chunk = 0x8000
  for (let i = 0; i < bytes.length; i += chunk) {
    s += String.fromCharCode(...bytes.subarray(i, i + chunk))
  }
  return btoa(s)
}

/**
 * 正文引用片段。`entry` 来自后端（`index_files/…`，stem 白名单已剔空格/括号），
 * 这里只负责两种正文形态的语法差：md 用 `![]()`，html 用 `<img>`。
 */
export function imageSnippet(entry: string, alt: string, isMd: boolean): string {
  const safeAlt = alt.replace(/[[\]"]/g, '').slice(0, 80)
  return isMd ? `![${safeAlt}](${entry})` : `<img src="${entry}" alt="${safeAlt}">`
}

/**
 * 附件引用片段（M4 最小版）。`entry` 来自后端（`attachments/…`，stem/ext 白名单净化）。
 * 附件不能内联渲染，一律插链接：md `[名](entry)`、html `<a href="entry">名</a>`。
 * `name` 是给用户看的原始文件名，这里只剔会断 md/html 语法的字符。
 */
export function attachmentSnippet(entry: string, name: string, isMd: boolean): string {
  const safeName = name.replace(/[[\]"]/g, '').slice(0, 80) || '附件'
  return isMd ? `[${safeName}](${entry})` : `<a href="${entry}">${safeName}</a>`
}
