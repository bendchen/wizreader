/**
 * Markdown 编辑辅助单测（M4）。零依赖：Node 22 原生类型剥离 + `node:test`。
 *
 * 运行：`npm run test:logic`（= `node --test tests/*.test.mjs`）
 *
 * 末组"保真铁律"是本文件最重要的部分：所有操作都**只许动它该动的区间**，
 * 未涉及的行必须逐字节不变 —— 这是 D0「库必须无损」在编辑器一侧的落地。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  blockLines,
  continueFence,
  continueList,
  continueTable,
  fenceOpenBefore,
  indentSelection,
  insertLink,
  insertTable,
  lineAt,
  toggleBlock,
  runAction,
  toggleWrap,
  wordRangeAt,
} from '../src/mdedit.ts'

const at = (text, start = text.length, end = start) => ({ text, start, end })

// ------------------------------------------------------------------ 回车

test('回车续列表：无序', () => {
  const r = continueList(at('- 一'))
  assert.equal(r.text, '- 一\n- ')
  assert.equal(r.selStart, r.text.length)
})

test('回车续列表：有序编号递增且保持分隔符', () => {
  assert.equal(continueList(at('3. 三')).text, '3. 三\n4. ')
  assert.equal(continueList(at('9) 九')).text, '9) 九\n10) ')
})

test('回车续列表：任务框续行一律给未勾选', () => {
  assert.equal(continueList(at('- [x] 做完的')).text, '- [x] 做完的\n- [ ] ')
})

test('回车续列表：嵌套保留缩进', () => {
  assert.equal(continueList(at('  - 内层')).text, '  - 内层\n  - ')
})

test('回车续列表：空项回车退出列表（只留缩进）', () => {
  const r = continueList(at('  - ', 4))
  assert.equal(r.text, '  ')
  assert.equal(r.selStart, 2)
})

test('回车续引用 + 引用内列表', () => {
  assert.equal(continueList(at('> 引用')).text, '> 引用\n> ')
  assert.equal(continueList(at('> - 项')).text, '> - 项\n> - ')
  assert.equal(continueList(at('> ')).text, '')
})

test('回车在普通段落里不接管', () => {
  assert.equal(continueList(at('普通文字')), null)
})

test('回车在围栏内不接管（代码里的 - 不是列表）', () => {
  const text = '```\n- a'
  assert.equal(continueList(at(text)), null)
})

test('回车补闭合围栏，光标停中间空行', () => {
  const r = continueFence(at('```rust'))
  assert.equal(r.text, '```rust\n\n```')
  assert.equal(r.selStart, 8)
  assert.equal(r.selStart, r.selEnd)
})

test('回车补围栏：已有闭合围栏 / 非围栏行不接管', () => {
  assert.equal(continueFence(at('```\ncode\n```')), null)
  assert.equal(continueFence(at('普通行')), null)
})

test('回车续表格行，列数保持一致', () => {
  const r = continueTable(at('| a | b | c |'))
  assert.equal(r.text, '| a | b | c |\n|  |  |  |')
})

// ------------------------------------------------------------------ Tab

test('Tab 无选区：插入缩进单位', () => {
  const r = indentSelection(at('abc', 3), 1)
  assert.equal(r.text, 'abc  ')
  assert.equal(r.selStart, 5)
})

test('Tab 落在列表项上：缩进整行，且光标跟着走（不甩到行首）', () => {
  const r = indentSelection(at('- abc', 5), 1)
  assert.equal(r.text, '  - abc')
  assert.equal(r.selStart, 7, '光标应右移 2，而不是回到行首')
})

test('Tab / Shift+Tab 多行选择：整块缩进与退格', () => {
  const text = 'a\nb\nc'
  const r = indentSelection({ text, start: 0, end: 5 }, 1)
  assert.equal(r.text, '  a\n  b\n  c')
  const back = indentSelection({ text: r.text, start: r.selStart, end: r.selEnd }, -1)
  assert.equal(back.text, text)
})

test('Shift+Tab 无选区：吃掉光标前的缩进单位', () => {
  const r = indentSelection(at('  abc', 2), -1)
  assert.equal(r.text, 'abc')
  assert.equal(r.selStart, 0)
})

// ------------------------------------------------------------------ 行内

test('行内标记：包裹 / 取消（选区外紧贴）', () => {
  const text = 'hello world'
  const wrapped = toggleWrap({ text, start: 0, end: 5 }, 'bold')
  assert.equal(wrapped.text, '**hello** world')
  const off = toggleWrap({ text: wrapped.text, start: wrapped.selStart, end: wrapped.selEnd }, 'bold')
  assert.equal(off.text, text)
})

test('行内标记：选中内容自带标记也能取消', () => {
  const r = toggleWrap({ text: '**粗**', start: 0, end: 7 }, 'bold')
  assert.equal(r.text, '粗')
  assert.equal(r.selEnd - r.selStart, 1)
})

test('行内标记：无选区时套用光标处的词', () => {
  const r = toggleWrap({ text: 'hello world', start: 2, end: 2 }, 'code')
  assert.equal(r.text, '`hello` world')
  assert.equal(r.text.slice(r.selStart, r.selEnd), 'hello')
})

test('行内标记：光标停在词首且词已被包住 → 取消（不能变成 ****粗****）', () => {
  const r = toggleWrap({ text: '**粗**', start: 2, end: 2 }, 'bold')
  assert.equal(r.text, '粗')
  assert.equal(r.selStart, 0)
  assert.equal(r.selEnd, 1)
})

test('行内标记：对粗体按斜体 → 变粗斜体（不把外层粗体吃掉）', () => {
  const r = toggleWrap({ text: '**粗**', start: 2, end: 2 }, 'italic')
  assert.equal(r.text, '***粗***')
})

test('行内标记：光标夹在空标记对中间 → 取消', () => {
  const r = toggleWrap({ text: '****', start: 2, end: 2 }, 'bold')
  assert.equal(r.text, '')
})

test('行内标记：空白处无词 → 插入空标记对，光标居中', () => {
  const r = toggleWrap({ text: 'a  b', start: 2, end: 2 }, 'bold')
  assert.equal(r.text, 'a **** b')
  assert.equal(r.selStart, 4)
})

test('取词：标点与标记不算词', () => {
  const w = wordRangeAt('foo-bar `x`', 2)
  assert.deepEqual(w, { start: 0, end: 7 })
  assert.equal(wordRangeAt('  ', 1), null)
})

// ------------------------------------------------------------------ 链接

test('链接：无选区 → 光标进方括号', () => {
  const r = insertLink(at('前'))
  assert.equal(r.text, '前[]()')
  assert.equal(r.selStart, r.selEnd)
  assert.equal(r.text.slice(r.selStart - 1, r.selStart + 1), '[]')
})

test('链接：选中 URL → 放进圆括号', () => {
  const r = insertLink({ text: 'https://x.dev/a', start: 0, end: 15 })
  assert.equal(r.text, '[](https://x.dev/a)')
  assert.equal(r.selStart, 1)
})

test('链接：选中文字 → 文字作标题，URL 占位被选中', () => {
  const r = insertLink({ text: '点这里', start: 0, end: 3 })
  assert.equal(r.text, '[点这里](url)')
  assert.equal(r.text.slice(r.selStart, r.selEnd), 'url')
})

test('图片：多个感叹号前缀', () => {
  const r = insertLink({ text: '图', start: 0, end: 1 }, true)
  assert.equal(r.text, '![图](url)')
})

// ------------------------------------------------------------------ 块级

test('标题：设置与再按清除', () => {
  const text = '标题'
  const h2 = toggleBlock(at(text), 'h2')
  assert.equal(h2.text, '## 标题')
  const off = toggleBlock({ text: h2.text, start: h2.selStart, end: h2.selEnd }, 'h2')
  assert.equal(off.text, '标题')
})

test('标题：从 h1 切到 h3 不叠加井号', () => {
  const r = toggleBlock({ text: '# 标题', start: 3, end: 3 }, 'h3')
  assert.equal(r.text, '### 标题')
})

test('引用：整段加/去 >', () => {
  const text = 'a\nb'
  const on = toggleBlock({ text, start: 0, end: 3 }, 'quote')
  assert.equal(on.text, '> a\n> b')
  const off = toggleBlock({ text: on.text, start: on.selStart, end: on.selEnd }, 'quote')
  assert.equal(off.text, 'a\nb')
})

test('列表：无序 / 有序（自动编号）/ 任务', () => {
  const text = 'a\nb\nc'
  assert.equal(toggleBlock({ text, start: 0, end: 5 }, 'ul').text, '- a\n- b\n- c')
  assert.equal(toggleBlock({ text, start: 0, end: 5 }, 'ol').text, '1. a\n2. b\n3. c')
  assert.equal(toggleBlock({ text, start: 0, end: 5 }, 'task').text, '- [ ] a\n- [ ] b\n- [ ] c')
})

test('列表：同层再按一次关掉，内容一字不动', () => {
  const on = toggleBlock({ text: 'a\nb', start: 0, end: 3 }, 'ul').text
  const off = toggleBlock({ text: on, start: 0, end: on.length }, 'ul')
  assert.equal(off.text, 'a\nb')
})

test('列表：任务项切普通列表要真的去掉任务框', () => {
  const r = toggleBlock({ text: '- [ ] a', start: 0, end: 8 }, 'ul')
  assert.equal(r.text, '- a')
})

test('围栏：整块包起来 / 再按一次去掉（不叠第二道围栏）', () => {
  const text = 'x = 1\ny = 2'
  const on = toggleBlock({ text, start: 0, end: 11 }, 'fence')
  assert.equal(on.text, '```\nx = 1\ny = 2\n```')
  assert.equal(on.text.slice(on.selStart, on.selEnd), text)
  const off = toggleBlock({ text: on.text, start: on.selStart, end: on.selEnd }, 'fence')
  assert.equal(off.text, text, '包起来之后选区只覆盖内容，再按必须摘掉围栏')
})

test('围栏：连按四次也只有一道围栏', () => {
  let cur = { text: 'x = 1', start: 0, end: 5 }
  for (let i = 0; i < 4; i++) {
    const r = toggleBlock(cur, 'fence')
    assert.ok((r.text.match(/```/g) || []).length <= 2, `第 ${i + 1} 次后出现多道围栏：${r.text}`)
    cur = { text: r.text, start: r.selStart, end: r.selEnd }
  }
  assert.equal(cur.text, 'x = 1')
})

test('水平线：空行就地替换，非空行另起一行', () => {
  assert.equal(toggleBlock(at(''), 'hr').text, '---')
  assert.equal(toggleBlock(at('文字'), 'hr').text, '文字\n---')
})

test('表格骨架：列头 + 分隔行 + 空数据行', () => {
  const r = insertTable(at(''), 3, 1)
  assert.equal(r.text, '| 列1 | 列2 | 列3 |\n| --- | --- | --- |\n|  |  |  |')
})

// ------------------------------------------------------------------ 边界

test('blockLines：选区止于下一行行首时不吞那一行', () => {
  const text = 'a\nb\nc'
  assert.equal(blockLines(text, 0, 2).length, 1)
  assert.equal(blockLines(text, 0, 3).length, 2)
})

test('lineAt / fenceOpenBefore', () => {
  const ln = lineAt('aa\nbb', 4)
  assert.deepEqual([ln.start, ln.end, ln.text], [3, 5, 'bb'])
  assert.equal(fenceOpenBefore('```\ncode', 8), true)
  assert.equal(fenceOpenBefore('```\ncode\n```\nafter', 17), false)
})

// ---------------------------------------------------- 保真铁律（最重要）

/** 把一次操作的结果按"公共前后缀"折算成最小改动区间，供"其余字符是否动过"断言 */
function changedSpan(oldText, newText) {
  let p = 0
  const maxP = Math.min(oldText.length, newText.length)
  while (p < maxP && oldText[p] === newText[p]) p += 1
  let s = 0
  const maxS = Math.min(oldText.length - p, newText.length - p)
  while (s < maxS && oldText[oldText.length - 1 - s] === newText[newText.length - 1 - s]) s += 1
  return { from: p, to: oldText.length - s }
}

test('保真：每个操作只动它该动的那一段，其余逐字节不变', () => {
  const text = '前言\n\n- 一项\n- 二项\n\n> 引用\n\n`code`\n\n尾注'
  const ops = [
    ['续列表', (c) => continueList(c)],
    ['加粗', (c) => toggleWrap(c, 'bold')],
    ['缩进', (c) => indentSelection(c, 1)],
    ['标题', (c) => toggleBlock(c, 'h2')],
    ['链接', (c) => insertLink(c)],
    ['表格', (c) => insertTable(c, 2, 1)],
  ]
  const pos = text.indexOf('- 一项') + 3
  for (const [name, fn] of ops) {
    const r = fn({ text, start: pos, end: pos })
    if (!r) continue
    const span = changedSpan(text, r.text)
    // 改动区必须落在"当前行"内（列表项那行），前后文不可能被碰到。
    // 上界给 +1：块级插入允许落在行尾换行符之后（= 下一行行首，仍是同一处）
    const lineStart = text.lastIndexOf('\n', pos - 1) + 1
    const lineEnd = text.indexOf('\n', pos)
    assert.ok(span.from >= lineStart, `${name}: 动了光标行之前的内容`)
    assert.ok(span.to <= lineEnd + 1, `${name}: 动了光标行之后的内容`)
  }
})

test('保真：多行块级操作不碰块外的字节', () => {
  const text = '头部\n\nalpha\nbeta\n\n尾部'
  const start = text.indexOf('alpha')
  const end = text.indexOf('beta') + 4
  for (const kind of ['ul', 'ol', 'task', 'quote', 'fence', 'h3']) {
    const r = toggleBlock({ text, start, end }, kind)
    const span = changedSpan(text, r.text)
    assert.ok(span.from >= start, `${kind}: 动了块前的内容`)
    assert.ok(span.to <= end, `${kind}: 动了块后的内容`)
  }
})

test('保真：围栏内部不接受列表/缩进辅助（代码即代码）', () => {
  const text = '```\n- a\n```'
  const pos = text.indexOf('- a') + 3
  assert.equal(continueList({ text, start: pos, end: pos }), null)
})

// ------------------------------------------------------------ 动作词表（工具栏）

test('动作词表：每个动作都接得上，且与直接调用同一结果', () => {
  const text = '标题\n内容'
  const c = { text, start: 0, end: 0 }
  const expect = {
    bold: toggleWrap(c, 'bold'),
    italic: toggleWrap(c, 'italic'),
    code: toggleWrap(c, 'code'),
    strike: toggleWrap(c, 'strike'),
    h1: toggleBlock(c, 'h1'),
    h2: toggleBlock(c, 'h2'),
    h3: toggleBlock(c, 'h3'),
    h4: toggleBlock(c, 'h4'),
    h0: toggleBlock(c, 'h0'),
    quote: toggleBlock(c, 'quote'),
    ul: toggleBlock(c, 'ul'),
    ol: toggleBlock(c, 'ol'),
    task: toggleBlock(c, 'task'),
    fence: toggleBlock(c, 'fence'),
    hr: toggleBlock(c, 'hr'),
    link: insertLink(c, false),
    image: insertLink(c, true),
    table: insertTable(c),
    indent: indentSelection(c, 1),
    outdent: indentSelection(c, -1),
  }
  for (const [action, want] of Object.entries(expect)) {
    const got = runAction(c, action)
    assert.notEqual(got, null, `动作 ${action} 没接上`)
    assert.deepEqual(got, want, `动作 ${action} 与直接调用结果不一致`)
  }
})
