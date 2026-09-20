/**
 * Markdown 编辑辅助（M4，纯函数、零依赖）。
 *
 * 设计铁律（D-M4-3，与 D0「库必须无损」同源）：
 * **每个操作只产出「新文本 + 新选区」，绝不规范化/重排其余字符。**
 * 用户没碰到的字节必须逐字节不动 —— 否则"编辑器自动整理了一篇笔记"就等于
 * 一次静默的全库改写。
 *
 * 全部函数是纯函数，可用 Node 直接断言（`tests/mdedit.test.mjs`），组件只负责
 * 把结果套用到 textarea 与选区上（见 `MdSourceEditor.vue` 的 `applyEdit`）。
 */

export interface Snapshot {
  text: string
  /** 选区起点（含） */
  start: number
  /** 选区终点（不含）；与 `start` 相等表示光标 */
  end: number
}

export interface EditResult {
  text: string
  selStart: number
  selEnd: number
}

export interface Line {
  start: number
  /** 行尾（不含换行符） */
  end: number
  text: string
}

// ------------------------------------------------------------------ 基础

/** 光标所在行 */
export function lineAt(text: string, pos: number): Line {
  const p = Math.max(0, Math.min(pos, text.length))
  const start = text.lastIndexOf('\n', p - 1) + 1
  let end = text.indexOf('\n', p)
  if (end < 0) end = text.length
  return { start, end, text: text.slice(start, end) }
}

/**
 * 选区覆盖的行。**选中区止于某行行首时不把那一行算进来**（编辑器通例）：
 * 否则"从行首往下选到下一行行首"会多缩进一行，用户会觉得"它自己跳了"。
 */
export function blockLines(text: string, start: number, end: number): Line[] {
  const stop = end > start && text[end - 1] === '\n' ? end - 1 : end
  const out: Line[] = []
  let p = lineAt(text, start).start
  for (;;) {
    const ln = lineAt(text, p)
    out.push(ln)
    if (ln.end >= stop || ln.end >= text.length) break
    p = ln.end + 1
  }
  return out
}

/**
 * 行内相对位置映射（改行首而不该丢光标）：
 * 先求公共前缀 `p` 与公共后缀 `s`，落在改动区之前的原样保留、之后的整体平移，
 * 落在改动区**内部**的夹到新片段末尾。
 *
 * 为什么不能用"行首偏移 + 原 rel"：缩进一次就会把光标甩到行首（那是编辑器里
 * 最烦人的一类小毛病）。单测 `cursor_survives_indent` 守这条。
 */
function mapInLine(oldS: string, newS: string, rel: number): number {
  const r = Math.max(0, Math.min(rel, oldS.length))
  let p = 0
  const maxP = Math.min(oldS.length, newS.length)
  while (p < maxP && oldS[p] === newS[p]) p += 1
  let s = 0
  const maxS = Math.min(oldS.length - p, newS.length - p)
  while (
    s < maxS &&
    oldS[oldS.length - 1 - s] === newS[newS.length - 1 - s]
  ) {
    s += 1
  }
  if (r <= p) return r
  if (r >= oldS.length - s) return r + (newS.length - oldS.length)
  return newS.length - s
}

/** 用"新的各行文本"重建全文，并把选区端点搬到对应位置 */
export function rebuild(
  text: string,
  lines: Line[],
  newTexts: string[],
  selStart: number,
  selEnd: number
): EditResult {
  const blockStart = lines[0].start
  let out = text.slice(0, blockStart)
  const offsets: number[] = []
  for (let i = 0; i < lines.length; i++) {
    offsets.push(out.length)
    out += newTexts[i]
    if (i < lines.length - 1) out += '\n'
  }
  out += text.slice(lines[lines.length - 1].end)
  const move = (pos: number): number => {
    for (let i = lines.length - 1; i >= 0; i--) {
      const ln = lines[i]
      if (pos >= ln.start && pos <= ln.end) {
        return offsets[i] + mapInLine(ln.text, newTexts[i], pos - ln.start)
      }
    }
    return pos
  }
  return { text: out, selStart: move(selStart), selEnd: move(selEnd) }
}

/** 位置之前是否有未闭合的围栏（在围栏内时不该套用列表/围栏辅助） */
export function fenceOpenBefore(text: string, pos: number): boolean {
  let n = 0
  for (const line of text.slice(0, pos).split('\n')) {
    if (/^\s*(`{3,}|~{3,})/.test(line)) n += 1
  }
  return n % 2 === 1
}

/** 光标处的"词"（供加粗/斜体/行内代码直接套用；纯空白处返回 null） */
export function wordRangeAt(text: string, pos: number): { start: number; end: number } | null {
  const isWord = (ch: string) => !/[\s*_`~[\]()<>]/.test(ch)
  let s = pos
  let e = pos
  while (s > 0 && isWord(text[s - 1])) s -= 1
  while (e < text.length && isWord(text[e])) e += 1
  return e > s ? { start: s, end: e } : null
}

// ------------------------------------------------------------------ 回车

function nextMarker(marker: string): string {
  const m = /^(\d{1,9})([.)])$/.exec(marker)
  return m ? String(Number(m[1]) + 1) + m[2] : marker
}

const RE_LINE_LIST = /^([ \t]*)([-*+]|\d{1,9}[.)])([ \t]+)(\[[ xX]\][ \t]+)?/
const RE_LINE_QUOTE = /^([ \t]*)((?:>[ \t]?)+)/

/**
 * 回车：列表/引用自动续行；**空项回车退出标记**（这是列表里最常见的动作：
 * 写完最后一项再敲一次回车，期望跳出列表而不是又生成一个空子弹）。
 * 返回 `null` = 不接管，走浏览器默认换行。
 */
export function continueList(c: Snapshot): EditResult | null {
  if (c.start !== c.end) return null
  const line = lineAt(c.text, c.start)
  if (fenceOpenBefore(c.text, line.start)) return null
  const prefix = c.text.slice(line.start, c.start)
  const tail = c.text.slice(c.start, line.end)

  const li = RE_LINE_LIST.exec(prefix)
  if (li) {
    const indent = li[1]
    const task = li[4] ?? ''
    const itemText = prefix.slice(li[0].length) + tail
    if (itemText.trim() === '') {
      // 空项 → 删掉标记，只留缩进（引用/其他前缀不动）
      const cut = line.start + li[0].length
      return {
        text: c.text.slice(0, line.start + indent.length) + c.text.slice(cut),
        selStart: line.start + indent.length,
        selEnd: line.start + indent.length,
      }
    }
    const insert = '\n' + indent + nextMarker(li[2]) + ' ' + (task ? '[ ] ' : '')
    const at = c.start
    return {
      text: c.text.slice(0, at) + insert + c.text.slice(at),
      selStart: at + insert.length,
      selEnd: at + insert.length,
    }
  }

  const q = RE_LINE_QUOTE.exec(prefix)
  if (q) {
    const mark = q[2].replace(/[ \t]+$/, '')
    const after = prefix.slice(q[0].length) + tail
    if (after.trim() === '') {
      const cut = line.start + q[0].length
      return {
        text: c.text.slice(0, line.start + q[1].length) + c.text.slice(cut),
        selStart: line.start + q[1].length,
        selEnd: line.start + q[1].length,
      }
    }
    // 引用里套列表（`> - x`）要连列表标记一起续
    const inner = RE_LINE_LIST.exec(after)
    const innerMark = inner
      ? nextMarker(inner[2]) + ' ' + (inner[4] ? '[ ] ' : '')
      : ''
    const insert = '\n' + q[1] + mark + ' ' + innerMark
    const at = c.start
    return {
      text: c.text.slice(0, at) + insert + c.text.slice(at),
      selStart: at + insert.length,
      selEnd: at + insert.length,
    }
  }
  return null
}

/** 回车：` ``` ` 开围栏后自动补闭合围栏，光标停在两者之间的空行 */
export function continueFence(c: Snapshot): EditResult | null {
  if (c.start !== c.end) return null
  const line = lineAt(c.text, c.start)
  if (c.start !== line.end) return null
  const m = /^\s*(`{3,}|~{3,})([\w+.#-]*)\s*$/.exec(line.text)
  if (!m) return null
  if (fenceOpenBefore(c.text, line.start)) return null
  const fence = m[1][0].repeat(m[1].length)
  const insert = '\n\n' + fence
  return {
    text: c.text.slice(0, c.start) + insert + c.text.slice(c.start),
    selStart: c.start + 1,
    selEnd: c.start + 1,
  }
}

/** 回车：表格行自动续行（保持列数），回车上补一行"|  |  |" */
export function continueTable(c: Snapshot): EditResult | null {
  if (c.start !== c.end) return null
  const line = lineAt(c.text, c.start)
  if (c.start !== line.end) return null
  if (!/^\s{0,3}\|.*\|\s*$/.test(line.text)) return null
  const cols = line.text.trim().replace(/^\||\|$/g, '').split('|').length
  const insert = '\n|' + '  |'.repeat(cols)
  return {
    text: c.text.slice(0, c.start) + insert + c.text.slice(c.start),
    selStart: c.start + insert.length,
    selEnd: c.start + insert.length,
  }
}

// ------------------------------------------------------------------ Tab

const INDENT_UNIT = '  '

/**
 * Tab / Shift+Tab。三种情形：
 * ① 有选区（或跨多行）→ 整体缩进/退格一个单位；
 * ② 光标落在**列表项**上 → 整行缩进（列表首个动作就是"这层要往下一层"）；
 * ③ 其余 → 光标处插入缩进单位。
 */
export function indentSelection(c: Snapshot, dir: 1 | -1): EditResult {
  const lines = blockLines(c.text, c.start, c.end)
  const single = lines.length === 1 && c.start === c.end
  const isItem = single && RE_LINE_LIST.test(lines[0].text)
  if (single && !isItem && dir === 1) {
    return {
      text: c.text.slice(0, c.start) + INDENT_UNIT + c.text.slice(c.start),
      selStart: c.start + INDENT_UNIT.length,
      selEnd: c.start + INDENT_UNIT.length,
    }
  }
  if (single && !isItem && dir === -1) {
    // 光标前若是缩进单位，就删掉它（否则什么都不做）
    const before = c.text.slice(Math.max(0, c.start - INDENT_UNIT.length), c.start)
    if (before === INDENT_UNIT) {
      return {
        text: c.text.slice(0, c.start - INDENT_UNIT.length) + c.text.slice(c.start),
        selStart: c.start - INDENT_UNIT.length,
        selEnd: c.start - INDENT_UNIT.length,
      }
    }
    return { text: c.text, selStart: c.start, selEnd: c.end }
  }
  const next = lines.map((ln) => {
    if (dir === 1) return INDENT_UNIT + ln.text
    const m = /^([ \t]{1,2})/.exec(ln.text)
    return m ? ln.text.slice(m[1].length) : ln.text
  })
  return rebuild(c.text, lines, next, c.start, c.end)
}

// ------------------------------------------------------------------ 行内标记

export type WrapKind = 'bold' | 'italic' | 'code' | 'strike'

const MARKS: Record<WrapKind, string> = {
  bold: '**',
  italic: '*',
  code: '`',
  strike: '~~',
}

/** 行内标记的开关（`**粗**` / `*斜*` / `` `代码` `` / `~~删除~~`）；无选区时套用光标处的词 */
export function toggleWrap(c: Snapshot, kind: WrapKind): EditResult {
  const mark = MARKS[kind]
  const L = mark.length
  const text = c.text
  const selText = text.slice(c.start, c.end)

  // ① 选区外紧贴成对标记 → 取消
  if (selText.length > 0) {
    const before = text.slice(Math.max(0, c.start - L), c.start)
    const after = text.slice(c.end, c.end + L)
    if (before === mark && after === mark) {
      return {
        text: text.slice(0, c.start - L) + selText + text.slice(c.end + L),
        selStart: c.start - L,
        selEnd: c.end - L,
      }
    }
    // ② 选区自身被标记包住 → 取消
    if (selText.length >= 2 * L && selText.startsWith(mark) && selText.endsWith(mark)) {
      const inner = selText.slice(L, selText.length - L)
      return {
        text: text.slice(0, c.start) + inner + text.slice(c.end),
        selStart: c.start,
        selEnd: c.start + inner.length,
      }
    }
    // ③ 包裹
    const at = text.slice(0, c.start) + mark + selText + mark + text.slice(c.end)
    return { text: at, selStart: c.start + L, selEnd: c.end + L }
  }

  // ④ 光标夹在成对标记之间 → 取消
  const before = text.slice(Math.max(0, c.start - L), c.start)
  const after = text.slice(c.start, c.start + L)
  if (before === mark && after === mark) {
    return {
      text: text.slice(0, c.start - L) + text.slice(c.start + L),
      selStart: c.start - L,
      selEnd: c.start - L,
    }
  }
  // ⑤ 套用光标处的词；无词则插入空标记对，光标停中间
  const w = wordRangeAt(text, c.start)
  if (w) {
    const outerBefore = text.slice(Math.max(0, w.start - L), w.start)
    const outerAfter = text.slice(w.end, w.end + L)
    // 词外面正好包着一层**长度恰好为 L** 的标记 → 取消。
    // "恰好 L" 这个限定不能少：`**粗**` 上按 Cmd+I 时左边那一串也是 `*`，
    // 若不限定就会把粗体退成 `*粗*`（用户想的是粗斜体 `***粗***`）。
    const exactlyL =
      text[w.start - L - 1] !== mark && text[w.end + L] !== mark
    if (outerBefore === mark && outerAfter === mark && exactlyL) {
      return {
        text: text.slice(0, w.start - L) + text.slice(w.start, w.end) + text.slice(w.end + L),
        selStart: w.start - L,
        selEnd: w.end - L,
      }
    }
    let ws = text.slice(w.start, w.end)
    let a = w.start
    let b = w.end
    // 词自己就带标记（`**粗**` 被整体取词时）→ 视为取消
    if (ws.length >= 2 * L && ws.startsWith(mark) && ws.endsWith(mark)) {
      ws = ws.slice(L, ws.length - L)
      a += L
      b -= L
    }
    return {
      text: text.slice(0, w.start) + mark + ws + mark + text.slice(w.end),
      selStart: a + L,
      selEnd: b + L,
    }
  }
  return {
    text: text.slice(0, c.start) + mark + mark + text.slice(c.start),
    selStart: c.start + L,
    selEnd: c.start + L,
  }
}

// ------------------------------------------------------------------ 块级

export type BlockKind = 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6' | 'h0' | 'quote' | 'ul' | 'ol' | 'task' | 'fence' | 'hr'

const RE_HEAD = /^(\s*)(#{1,6}\s+)?(.*)$/
const RE_ANY_QUOTE = /^(\s*)>[ \t]?/

/** 拆解一个列表项：缩进 / 标记 / 是否任务框 / 标记之后的内容（纯函数，供块级开关复用） */
function listInfo(line: string): { indent: string; marker: string; task: boolean; content: string } | null {
  const m = RE_LINE_LIST.exec(line)
  if (!m) return null
  return { indent: m[1], marker: m[2], task: m[4] !== undefined, content: line.slice(m[0].length) }
}

/** 块级开关（标题 / 引用 / 列表 / 任务 / 围栏 / 水平线），作用于选区覆盖的每一行 */
export function toggleBlock(c: Snapshot, kind: BlockKind): EditResult {
  const text = c.text
  if (kind === 'hr') {
    const ln = lineAt(text, c.start)
    if (ln.text.trim() === '') {
      // 空行就地变成水平线；非空行则在下一行插入
      const out = text.slice(0, ln.start) + '---' + text.slice(ln.end)
      return { text: out, selStart: ln.start + 3, selEnd: ln.start + 3 }
    }
    const insert = '\n---'
    return {
      text: text.slice(0, ln.end) + insert + text.slice(ln.end),
      selStart: ln.end + insert.length,
      selEnd: ln.end + insert.length,
    }
  }

  const lines = blockLines(text, c.start, c.end)
  /** 所有行都命中同一条行首标记（空行不算破坏，便于"整段加引用/列表"） */
  const all = (re: RegExp) => lines.every((ln) => re.test(ln.text) || ln.text.trim() === '')

  let next: string[]
  switch (kind) {
    case 'h0':
    case 'h1':
    case 'h2':
    case 'h3':
    case 'h4':
    case 'h5':
    case 'h6': {
      const level = kind === 'h0' ? 0 : Number(kind[1])
      const already = level > 0 && lines.every((ln) => {
        const m = RE_HEAD.exec(ln.text)!
        // group2 形如 `### `（带尾随空格）⇒ trim 后长度就是级别，别再 -1
        return m[2] !== undefined && m[2].trim().length === level
      })
      const lv = already ? 0 : level
      next = lines.map((ln) => {
        const m = RE_HEAD.exec(ln.text)!
        const body = m[3]
        return lv > 0 ? `${m[1]}${'#'.repeat(lv)} ${body}` : `${m[1]}${body}`
      })
      break
    }
    case 'quote': {
      const off = all(RE_ANY_QUOTE)
      next = lines.map((ln) =>
        off ? ln.text.replace(RE_ANY_QUOTE, '$1') : ln.text.replace(/^(\s*)/, '$1> ')
      )
      break
    }
    case 'ul':
    case 'ol':
    case 'task': {
      // 逐行读出"缩进 / 标记 / 任务框 / 内容"，只重建**行首标记**，内容一字不动
      const infos = lines.map((ln) => listInfo(ln.text))
      const same = infos.every(
        (it) =>
          it !== null &&
          (kind === 'ul'
            ? /^[-*+]$/.test(it.marker) && !it.task
            : kind === 'ol'
              ? /^\d+[.)]$/.test(it.marker)
              : it.task)
      )
      next = lines.map((ln, i) => {
        const it = infos[i]
        const indent = it ? it.indent : /^\s*/.exec(ln.text)![0]
        const content = it ? it.content : ln.text.slice(indent.length)
        if (same) return indent + content // 同一层再按一次 → 关掉
        const marker = kind === 'ol' ? `${i + 1}.` : kind === 'task' ? '- [ ]' : '-'
        return `${indent}${marker} ${content}`
      })
      break
    }
    case 'fence': {
      const isOpen = (t: string) => /^\s*(`{3,}|~{3,})\s*\S*\s*$/.test(t)
      const isClose = (t: string) => /^\s*(`{3,}|~{3,})\s*$/.test(t)
      const last = lines[lines.length - 1]
      // ① 选区自带围栏 → 去掉这两行
      if (lines.length >= 2 && isOpen(lines[0].text) && isClose(last.text)) {
        const inner = lines.slice(1, -1).map((ln) => ln.text)
        return {
          text: joinBlock(text, lines, inner),
          selStart: lines[0].start,
          selEnd: lines[0].start + inner.join('\n').length,
        }
      }
      // ② 选区**外面**紧贴着围栏 → 也视为"再按一次"，把围栏摘掉。
      //    上一版只认 ①，于是"包起来（选区只覆盖内容）→ 再按"变成了再包一层，
      //    四次点击就长出四道围栏 —— 这是单测抓出来的。
      //
      //    删除区间要成对：开围栏连同它**后面**的换行一起删，闭围栏连同它**前面**的
      //    换行一起删。删哪一侧的换行决定了后面那段是"接着正文"还是"多一个空行"。
      const prev = lines[0].start > 0 ? lineAt(text, lines[0].start - 1) : null
      const next = last.end + 1 <= text.length ? lineAt(text, last.end + 1) : null
      if (prev && next && isOpen(prev.text) && isClose(next.text) && next.start > 0) {
        return {
          text:
            text.slice(0, prev.start) +
            text.slice(lines[0].start, next.start - 1) +
            text.slice(next.end),
          selStart: prev.start,
          selEnd: prev.start + (last.end - lines[0].start),
        }
      }
      const inner = lines.map((ln) => ln.text)
      const start = lines[0].start + 4 // 越过 "```\n"
      return {
        text: joinBlock(text, lines, ['```', ...inner, '```']),
        selStart: start,
        selEnd: start + inner.join('\n').length,
      }
    }
    default:
      return { text, selStart: c.start, selEnd: c.end }
  }
  return rebuild(text, lines, next, c.start, c.end)
}

/** 把"新的各行文本"拼回全文（不搬选区；调用方自行给选区） */
function joinBlock(text: string, lines: Line[], next: string[]): string {
  const head = text.slice(0, lines[0].start)
  const tail = text.slice(lines[lines.length - 1].end)
  return head + next.join('\n') + tail
}

// ------------------------------------------------------------------ 插入

/**
 * 链接 / 图片（Cmd+K）。三种情形都对得上：
 * - 无选区 → `[](url)`，光标停在 `[]` 里（先写标题最顺）
 * - 选区是 URL / 邮箱 → `[](选中的 URL)`，光标停在 `[]` 里
 * - 选区是文字 → `[文字](url)`，**选中 `url` 占位**，直接粘地址即可覆盖
 */
export function insertLink(c: Snapshot, image = false): EditResult {
  const bang = image ? '!' : ''
  const text = c.text
  const sel = text.slice(c.start, c.end)
  const isUrl = /^(https?:\/\/|www\.|mailto:)\S*$/i.test(sel.trim()) || /^\S+@\S+\.\S+$/.test(sel.trim())
  if (!sel) {
    const ins = `${bang}[]()`
    return {
      text: text.slice(0, c.start) + ins + text.slice(c.end),
      selStart: c.start + bang.length + 1,
      selEnd: c.start + bang.length + 1,
    }
  }
  if (isUrl) {
    const ins = `${bang}[](${sel.trim()})`
    return {
      text: text.slice(0, c.start) + ins + text.slice(c.end),
      selStart: c.start + bang.length + 1,
      selEnd: c.start + bang.length + 1,
    }
  }
  const ins = `${bang}[${sel}](url)`
  const urlStart = c.start + bang.length + 1 + sel.length + 2
  return {
    text: text.slice(0, c.start) + ins + text.slice(c.end),
    selStart: urlStart,
    selEnd: urlStart + 3,
  }
}

/** 插入表格骨架（列头 + 分隔行 + 一个空数据行） */
export function insertTable(c: Snapshot, cols = 3, rows = 1): EditResult {
  const n = Math.max(1, Math.min(cols, 12))
  const head = '| ' + Array.from({ length: n }, (_, i) => `列${i + 1}`).join(' | ') + ' |'
  const sep = '| ' + Array.from({ length: n }, () => '---').join(' | ') + ' |'
  const body = '| ' + Array.from({ length: n }, () => '').join(' | ') + ' |'
  const block = [head, sep, ...Array.from({ length: Math.max(1, rows) }, () => body)].join('\n')
  const ln = lineAt(c.text, c.start)
  const at = ln.text.trim() === '' ? ln.start : ln.end
  const ins = ln.text.trim() === '' ? block : '\n' + block
  return {
    text: c.text.slice(0, at) + ins + c.text.slice(at),
    selStart: at + ins.length,
    selEnd: at + ins.length,
  }
}

// ------------------------------------------------------------------ 动作表

/** 编辑器动作名（工具栏按钮与快捷键共用同一份词表，避免两处各写一套） */
export type MdAction =
  | 'bold'
  | 'italic'
  | 'code'
  | 'strike'
  | 'h1'
  | 'h2'
  | 'h3'
  | 'h4'
  | 'h0'
  | 'quote'
  | 'ul'
  | 'ol'
  | 'task'
  | 'fence'
  | 'hr'
  | 'link'
  | 'image'
  | 'table'
  | 'indent'
  | 'outdent'

/** 动作 → 操作。工具栏、快捷键、单测都走这一个入口（口径只有一份） */
export function runAction(c: Snapshot, action: MdAction): EditResult | null {
  switch (action) {
    case 'bold':
    case 'italic':
    case 'code':
    case 'strike':
      return toggleWrap(c, action)
    case 'h1':
    case 'h2':
    case 'h3':
    case 'h4':
    case 'h0':
    case 'quote':
    case 'ul':
    case 'ol':
    case 'task':
    case 'fence':
    case 'hr':
      return toggleBlock(c, action)
    case 'link':
      return insertLink(c, false)
    case 'image':
      return insertLink(c, true)
    case 'table':
      return insertTable(c)
    case 'indent':
      return indentSelection(c, 1)
    case 'outdent':
      return indentSelection(c, -1)
  }
}
