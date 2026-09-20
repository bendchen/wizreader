/**
 * Markdown 高亮单测（M4）。零依赖：Node 22 原生类型剥离 + `node:test`。
 *
 * 运行：`npm run test:logic`
 *
 * **核心不变量**：`stripTags(highlightMd(src)) === escapeHtml(src)` ——
 * 高亮只许插标签，一个可见字符都不能增删改（含换行）。编辑器 overlay 与 textarea
 * 的换行位置必须一一对应，破了这条光标就会跑错行，所以这条是硬约束不是"看起来对"。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import { escapeHtml, highlightMd, stripTags } from '../src/mdhl.ts'

/** 等长不变量断言 */
function invariant(src, label = '') {
  const html = highlightMd(src)
  assert.equal(stripTags(html), escapeHtml(src), `字符被高亮改动过：${label}`)
  return html
}

test('标题 / 引用 / 列表 / 任务 / 表格 / 水平线 都着上类名', () => {
  const html = invariant('# 一级\n> 引用\n- 项\n- [ ] 待办\n1. 有序\n\n---\n\n| a | b |\n|---|---|')
  assert.match(html, /class="md-h1"/)
  assert.match(html, /class="md-quote"/)
  assert.match(html, /class="md-list md-task-box"/)
  assert.match(html, /class="md-hr"/)
  assert.match(html, /class="md-tbl"/)
})

test('围栏内部整段代码色，且不做行内解析', () => {
  const html = invariant('```rust\nlet a = *x*; // **不是强调**\n```\n\n*这才是斜体*')
  const code = html.slice(html.indexOf('md-code'), html.indexOf('\n\n'))
  assert.ok(!code.includes('md-em'), '代码块内不该出现强调标签')
  assert.match(html, /class="md-em"/, '围栏外的斜体仍要高亮')
})

test('围栏行按原样输出（含波浪线围栏与 info 串）', () => {
  const html = invariant('~~~python\nx = 1\n~~~')
  assert.equal((html.match(/md-fence/g) || []).length, 2)
  assert.match(html, /python/)
})

test('行内：代码 / 链接 / 自动链接 / 粗斜体 / 删除线 / 转义', () => {
  const src = '`a*b` [文字](https://x/) <https://y/> **粗** *斜* ~~删~~ \\*转义'
  const html = invariant(src)
  for (const cls of ['md-ic', 'md-link', 'md-url', 'md-strong', 'md-em', 'md-strike', 'md-esc']) {
    assert.match(html, new RegExp(cls), `缺少 ${cls}`)
  }
})

test('链接的标题与 URL 分段着色', () => {
  const html = invariant('[标题](https://a.b/c)')
  assert.match(html, /<span class="md-link">标题<\/span>/)
  assert.match(html, /<span class="md-url">https:\/\/a\.b\/c<\/span>/)
})

test('HTML 特殊字符被转义，但正文一字不差', () => {
  const src = 'a < b && c > d'
  const html = invariant(src)
  assert.match(html, /&lt;/)
  assert.match(html, /&amp;/)
  assert.ok(!/a < b/.test(html), '裸 < 不该出现在输出里（<pre> 里会吞掉后文）')
})

test('空文档 / 纯空行 / 末尾换行都不改变行数', () => {
  for (const src of ['', '\n', '\n\n\n', 'a\n', '\na']) {
    const html = highlightMd(src)
    assert.equal(html.split('\n').length, src.split('\n').length, `行数不一致：${JSON.stringify(src)}`)
    invariant(src, JSON.stringify(src))
  }
})

test('列表前缀与标题前缀单独成段（缩进+标记+任务框一起着色）', () => {
  const html = invariant('  - [ ] 待办事项')
  assert.match(html, /<span class="md-list md-task-box">  - \[ \] <\/span>/)
})

// ------------------------------------------------- 真实语料里最刁的几种形态

test('真实形态：排版缩进换行（v3 折叠后的正文）不产生额外换行', () => {
  const src = '段落一\n段落二\n\n    # 这是纯文本里的井号，不是标题'
  const html = invariant(src)
  assert.match(html, /class="md-h1"/)
})

test('真实形态：未闭合围栏（正文被截断）不吞掉后面的高亮', () => {
  const src = '```\nabc'
  assert.equal(highlightMd(src).split('\n').length, 2)
  invariant(src)
})

test('真实形态：表格分隔行与含反引号的标题', () => {
  const src = '## 用 `wiz-cli` 建库\n| 字段 | 含义 |\n| --- | --- |\n| guid | 主键 |'
  invariant(src)
  assert.match(highlightMd(src), /class="md-ic"/)
})

test('真实形态：跨行强调不越行（`*` 未配对时不得把整篇吞进一个 span）', () => {
  const src = 'a * b\nc * d'
  const html = invariant(src)
  assert.ok(!html.includes('md-em'), '不配对的星号不该产生强调段')
})

test('强调取"最近闭合"：`*斜*` 后面的 `~~删~~` 不得被吞进斜体', () => {
  const html = invariant('*斜* ~~删~~ \\*转义')
  assert.match(html, /md-strike/, '删除线必须自己成段')
  assert.match(html, /md-esc/, '反斜杠转义必须成段')
  const emContent = html.match(/<span class="md-em">([^<]*)<\/span>/)?.[1]
  assert.equal(emContent, '斜', '斜体内容只能是最近闭合标记之间的文字')
})

test('强调不跨越空行/首尾空白', () => {
  assert.ok(!highlightMd('* 项目符号').includes('md-em'), '列表星号不是斜体')
  assert.ok(!highlightMd('a * b * c').includes('md-em'), '首尾带空白不算强调')
})

test('下划线强调只在词边界生效（snake_case 不被拆）', () => {
  assert.ok(!highlightMd('snake_case_name').includes('md-em'))
  assert.match(highlightMd('_斜体_ 正常'), /md-em/)
})

test('超长文本也要守住不变量（1 万行）', () => {
  const src = Array.from({ length: 10000 }, (_, i) => `- 第 ${i} 行 **加粗** \`代码\``).join('\n')
  const t0 = Date.now()
  const html = highlightMd(src)
  const ms = Date.now() - t0
  assert.equal(stripTags(html), escapeHtml(src))
  assert.ok(ms < 3000, `1 万行高亮耗时 ${ms}ms，超出预期`)
})
