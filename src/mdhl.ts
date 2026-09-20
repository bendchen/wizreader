/**
 * Markdown 源码高亮（M4，纯函数、零依赖）。
 *
 * 为什么自己写而不是引 CodeMirror：见 `docs/本地笔记读写实现.md` §24.2（D-M4-2）。
 * 一句话 —— 需要的是"高亮 + 编辑辅助"，不是一套编辑器运行时；本模块是纯函数，
 * 能在 Node 里直接断言，且**只有一条硬不变量**：
 *
 * > `stripTags(highlightMd(src)) === escapeHtml(src)`
 *
 * 即高亮**只允许插入标签**，一个可见字符都不许增删改（含换行）。这条不变量由
 * `tests/mdhl.test.mjs` 逐字符守住 —— 它同时也是编辑器 overlay 对齐的**先决条件**
 * （覆盖层与 textarea 的字符数/换行位置必须完全一致，否则光标会跑到错行）。
 */

/** HTML 转义（`&` 必须第一个替换） */
export function escapeHtml(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}

/** 反转义（把高亮结果里的实体还原成原字符） */
export function unescapeHtml(s: string): string {
  return s.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&')
}

/**
 * 去掉标签**但保留实体**（测试用）。
 *
 * 刻意不在这里反转义：不变量要断言的是"高亮只插了标签、正文该转义的都转义了"，
 * 即 `stripTags(highlightMd(src)) === escapeHtml(src)`。顺手反转义会让这条断言
 * 变成同义反复（那样 `<` 被吞进标签也照样"通过"）。
 */
export function stripTags(html: string): string {
  return html.replace(/<[^>]*>/g, '')
}

// ------------------------------------------------------------------ 行级

type LineKind = 'heading' | 'hr' | 'quote' | 'list' | 'table' | 'fence' | 'blank' | 'text'

interface LineScan {
  kind: LineKind
  /** 行首到"内容"之间的前缀长度（缩进 + 标记 + 空格） */
  prefix: number
  /** 前缀段单独的 class（缩进 + 标记 + 任务框） */
  prefixClass: string
  /** 内容段的容器 class */
  contentClass: string
}

// 宽容口径：这是**高亮**不是解析。缩进限制放宽（用户手写的 md 常见 4+ 空格缩进），
// 判错最多是颜色不准，不会改变文本本身。
const RE_FENCE = /^\s*(`{3,}|~{3,})(.*)$/
const RE_HEADING = /^\s*(#{1,6})(\s+)(.*)$/
const RE_HR = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/
const RE_QUOTE = /^\s*((?:>[ \t]?)+)/
const RE_LIST = /^([ \t]*)([-*+]|\d{1,9}[.)])([ \t]+)(\[[ xX]\][ \t]+)?/
const RE_TABLE = /^\s{0,3}\|.*\|\s*$/

function scanLine(line: string): LineScan {
  if (line.trim() === '') {
    return { kind: 'blank', prefix: 0, prefixClass: '', contentClass: '' }
  }
  if (RE_FENCE.test(line)) {
    return { kind: 'fence', prefix: 0, prefixClass: '', contentClass: 'md-fence' }
  }
  if (RE_HR.test(line)) {
    return { kind: 'hr', prefix: 0, prefixClass: '', contentClass: 'md-hr' }
  }
  const h = RE_HEADING.exec(line)
  if (h) {
    const lvl = h[1].length
    return {
      kind: 'heading',
      prefix: h[1].length + h[2].length,
      prefixClass: `md-h${lvl}`,
      contentClass: `md-h${lvl} md-hc`,
    }
  }
  const li = RE_LIST.exec(line)
  if (li) {
    const task = li[4] ? li[4].length : 0
    return {
      kind: 'list',
      prefix: li[1].length + li[2].length + li[3].length + task,
      prefixClass: task ? 'md-list md-task-box' : 'md-list',
      contentClass: '',
    }
  }
  const q = RE_QUOTE.exec(line)
  if (q) {
    return {
      kind: 'quote',
      prefix: q[1].length,
      prefixClass: 'md-quote',
      contentClass: 'md-quote-c',
    }
  }
  if (RE_TABLE.test(line)) {
    return { kind: 'table', prefix: 0, prefixClass: '', contentClass: 'md-tbl' }
  }
  return { kind: 'text', prefix: 0, prefixClass: '', contentClass: '' }
}

// ------------------------------------------------------------------ 行内

/**
 * 行内标记规则表（**顺序即优先级**）。全部用 `y`（sticky）正则 + `lastIndex` 推进，
 * 避免对每一行做切片；`(?<=\S)` 之类的后视断言一律不用 —— Tauri 在旧版 macOS 上
 * 跑的是系统 WebKit，后视断言的支持面比看起来窄。
 *
 * `cls` 的长度对应**捕获组个数**；无捕获组时只用 `cls[0]`。
 */
const INLINE: { re: RegExp; cls: string[] }[] = [
  { re: /(`+)(\S(?:[\s\S]*?\S)?)\1/y, cls: ['md-tick', 'md-ic'] },
  { re: /!\[([^\]]*)\]\(([^)\s]*)(?:\s+"[^"]*")?\)/y, cls: ['md-link', 'md-url'] },
  { re: /\[([^\]]*)\]\(([^)\s]*)(?:\s+"[^"]*")?\)/y, cls: ['md-link', 'md-url'] },
  { re: /<(?:https?:\/\/|mailto:)[^>\s]+>/y, cls: ['md-link'] },
]

/**
 * 强调（`**粗**` / `*斜*` / `~~删~~` / `_斜_`）在位置 `i` 处的解析。
 *
 * 为什么单独写而不用正则：`(\*\*)(\S(?:[\s\S]*?\S)?)\1` 里的**可选组是贪婪的**，
 * 正则引擎会先试着把可选组吃满，于是"最短内容"根本不成立 —— 实测
 * `*斜* ~~删~~ \*` 会被整段吞成一个斜体（连删除线一起）。这里改用
 * `indexOf` 找**最近的**闭合标记，语义就是人眼看到的那个。
 */
function emphasisAt(
  s: string,
  i: number
): { mark: string; cls: string; contentStart: number; contentEnd: number; closeAt: number } | null {
  const cands: { mark: string; cls: string }[] = [
    { mark: '***', cls: 'md-strong md-em' },
    { mark: '___', cls: 'md-strong md-em' },
    { mark: '**', cls: 'md-strong' },
    { mark: '__', cls: 'md-strong' },
    { mark: '~~', cls: 'md-strike' },
    { mark: '*', cls: 'md-em' },
    { mark: '_', cls: 'md-em' },
  ]
  for (const c of cands) {
    if (!s.startsWith(c.mark, i)) continue
    const L = c.mark.length
    const underscore = c.mark[0] === '_'
    // `_` 只在词边界才算强调 —— `snake_case_name` 不该被拆成斜体
    if (underscore && /[\p{L}\p{N}]/u.test(i > 0 ? s[i - 1] : ' ')) continue
    const close = s.indexOf(c.mark, i + L)
    if (close < 0 || close === i + L) continue
    if (/\s/.test(s[i + L]) || /\s/.test(s[close - 1])) continue // 首尾带空白的不是强调
    if (underscore && /[\p{L}\p{N}]/u.test(s[close + L] ?? ' ')) continue
    return { mark: c.mark, cls: c.cls, contentStart: i + L, contentEnd: close, closeAt: close }
  }
  return null
}

function wrap(cls: string, inner: string): string {
  return cls ? `<span class="${cls}">${inner}</span>` : inner
}

/** 定界符本身也着色，但另加 `md-delim` 供 CSS 调暗（内容才是重点） */
function delim(cls: string, text: string): string {
  return wrap(`${cls} md-delim`, escapeHtml(text))
}

function inlineHtml(s: string): string {
  let out = ''
  let plain = ''
  const flush = () => {
    if (plain) {
      out += escapeHtml(plain)
      plain = ''
    }
  }
  let i = 0
  while (i < s.length) {
    // ① 行内代码 / 链接 / 自动链接
    let hit = false
    for (const rule of INLINE) {
      rule.re.lastIndex = i
      const m = rule.re.exec(s)
      if (!m || m[0].length === 0) continue
      flush()
      let consumed = 0
      for (let k = 1; k < m.length; k++) {
        const g = m[k]
        if (g === undefined) continue
        const at = m[0].indexOf(g, consumed)
        if (at < 0) continue
        const between = m[0].slice(consumed, at)
        if (between) out += wrap(rule.cls[k - 1] ?? '', escapeHtml(between))
        out += wrap(rule.cls[k - 1] ?? '', escapeHtml(g))
        consumed = at + g.length
      }
      if (consumed < m[0].length) out += escapeHtml(m[0].slice(consumed))
      i += m[0].length
      hit = true
      break
    }
    if (hit) continue
    // ② 强调
    const em = emphasisAt(s, i)
    if (em) {
      flush()
      out +=
        delim(em.cls, em.mark) +
        wrap(em.cls, escapeHtml(s.slice(em.contentStart, em.contentEnd))) +
        delim(em.cls, em.mark)
      i = em.closeAt + em.mark.length
      continue
    }
    // ③ 转义
    if (s[i] === '\\' && i + 1 < s.length) {
      flush()
      out += wrap('md-esc', escapeHtml(s.slice(i, i + 2)))
      i += 2
      continue
    }
    plain += s[i]
    i += 1
  }
  flush()
  return out
}

// ------------------------------------------------------------------ 入口

/**
 * 把 Markdown 源码高亮成 HTML。
 *
 * **不变量**：`stripTags(返回值) === escapeHtml(入参)`（标签之外一字不差，含换行）。
 * 换行按原样输出（不用 `<br>`），这样 `<pre>` 里的行数与 textarea 完全一致。
 */
export function highlightMd(src: string): string {
  const out: string[] = []
  let inFence = false
  for (const line of src.split('\n')) {
    if (inFence) {
      if (RE_FENCE.test(line)) {
        inFence = false
        out.push(wrap('md-fence', escapeHtml(line)))
      } else {
        // 围栏内部：整体一段代码色，不做行内解析（代码里的 `*` 不是强调）
        out.push(wrap('md-code', escapeHtml(line)))
      }
      continue
    }
    const sc = scanLine(line)
    if (sc.kind === 'blank') {
      out.push('')
      continue
    }
    if (sc.kind === 'fence') {
      inFence = true
      out.push(wrap('md-fence', escapeHtml(line)))
      continue
    }
    if (sc.kind === 'hr') {
      out.push(wrap('md-hr', escapeHtml(line)))
      continue
    }
    const pre = line.slice(0, sc.prefix)
    const rest = line.slice(sc.prefix)
    out.push(
      (pre ? wrap(sc.prefixClass, escapeHtml(pre)) : '') + wrap(sc.contentClass, inlineHtml(rest))
    )
  }
  return out.join('\n')
}
