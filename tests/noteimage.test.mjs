/**
 * 图片插入 / 粘贴纯逻辑单测（M4）。零依赖：Node 原生类型剥离 + `node:test`。
 *
 * 运行：`npm run test:logic`
 *
 * 口径（与 src/noteimage.ts 文件头一致）：**格式判定不在前端**（后端按魔数嗅探），
 * 前端只判定"粘贴里有没有图、以什么形态出现"，并负责 data: URI 抽取/替换与
 * 引用片段拼装。这里的断言就是这些判定的合同。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  attachmentSnippet,
  base64ToBytes,
  bytesToBase64,
  dataUriToBase64,
  decidePaste,
  extractDataUris,
  imageSnippet,
  replaceDataUris,
} from '../src/noteimage.ts'

const PNG_B64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=='
const PNG_URI = `data:image/png;base64,${PNG_B64}`
const JPEG_URI = 'data:image/jpeg;base64,/9j/4AAQSkZJRg=='
const SVG_URI = 'data:image/svg+xml;base64,PHN2Zy8+'

// ------------------------------------------------------------------ 粘贴判定

test('粘贴判定：剪贴板有图片文件（截图）→ files', () => {
  const f = new File([new Uint8Array([1])], 'x.png', { type: 'image/png' })
  assert.equal(decidePaste({ files: [f], text: '', html: '' }), 'files')
})

test('粘贴判定：纯文本内嵌 data: 图 → text-data', () => {
  assert.equal(
    decidePaste({ files: [], text: `前文 ${PNG_URI} 后文`, html: '' }),
    'text-data'
  )
})

test('粘贴判定：富文本 img 内嵌 data: 图 → html-data', () => {
  assert.equal(
    decidePaste({
      files: [],
      text: '纯文本没有图',
      html: `<p>a</p><img src="${PNG_URI}"><p>b</p>`,
    }),
    'html-data'
  )
})

test('粘贴判定：无图 → none（走浏览器默认粘贴）', () => {
  assert.equal(decidePaste({ files: [], text: '普通文字', html: '<p>富文本</p>' }), 'none')
  assert.equal(decidePaste({ files: [], text: '', html: '' }), 'none')
})

test('粘贴判定：优先级 files > text-data > html-data', () => {
  const f = new File([new Uint8Array([1])], 'x.png', { type: 'image/png' })
  assert.equal(
    decidePaste({ files: [f], text: PNG_URI, html: `<img src="${PNG_URI}">` }),
    'files'
  )
  assert.equal(
    decidePaste({ files: [], text: PNG_URI, html: `<img src="${PNG_URI}">` }),
    'text-data'
  )
})

test('粘贴判定：img 无 data: 源（http/相对路径）不算图 → none', () => {
  assert.equal(
    decidePaste({
      files: [],
      text: '',
      html: '<img src="https://example.com/a.png"><img src="index_files/a.png">',
    }),
    'none'
  )
})

// ------------------------------------------------------------------ data: URI 提取与替换

test('提取：按出现顺序抽出全部 data: URI，串精确', () => {
  const text = `a${PNG_URI}b ${JPEG_URI} c`
  const uris = extractDataUris(text)
  assert.deepEqual(uris, [PNG_URI, JPEG_URI])
})

test('提取：没有 data: 图 → 空数组', () => {
  assert.deepEqual(extractDataUris('data:text/html;base64,PGI+'), [])
})

test('替换：命中的换成包内引用，没命中的原样保留', () => {
  const out = replaceDataUris(`<p>x ${PNG_URI} y</p>`, { [PNG_URI]: 'index_files/a_1234.png' })
  assert.equal(out, '<p>x index_files/a_1234.png y</p>')
})

test('替换：多张图逐一替换，互不串位', () => {
  const map = { [PNG_URI]: 'index_files/a.png', [JPEG_URI]: 'index_files/b.jpg' }
  assert.equal(
    replaceDataUris(`${PNG_URI} ${JPEG_URI}`, map),
    'index_files/a.png index_files/b.jpg'
  )
})

// ------------------------------------------------------------------ base64 往返

test('base64 往返：data: URI → 纯 base64 → 字节还原 PNG 魔数', () => {
  const b64 = dataUriToBase64(PNG_URI)
  assert.equal(b64, PNG_B64)
  const bytes = base64ToBytes(b64)
  assert.deepEqual(
    [...bytes.slice(0, 8)],
    [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
    '必须是 PNG 魔数（后端嗅探就靠它）'
  )
})

test('base64 编码：字节 → base64 分块不丢数据（> 0x8000 跨块）', () => {
  const bytes = new Uint8Array(0x8000 + 37)
  for (let i = 0; i < bytes.length; i++) bytes[i] = i & 0xff
  const back = base64ToBytes(bytesToBase64(bytes))
  assert.deepEqual([...back], [...bytes])
})

// ------------------------------------------------------------------ 引用片段

test('片段：md 形态 → ![]() 语法，alt 净化方括号', () => {
  assert.equal(imageSnippet('index_files/a_1.png', '截图', true), '![截图](index_files/a_1.png)')
  assert.equal(imageSnippet('index_files/a_1.png', 'a[b]c', true), '![abc](index_files/a_1.png)')
})

test('片段：html 形态 → <img>，alt 净化引号', () => {
  assert.equal(
    imageSnippet('index_files/a_1.png', 'he said "hi"', false),
    '<img src="index_files/a_1.png" alt="he said hi">'
  )
})

test('片段：entry 由后端保证无空格括号，md 语法不会被截断', () => {
  const s = imageSnippet('index_files/截图_1a2b3c4d.png', '', true)
  assert.match(s, /^!\[\]\(index_files\/[^()]+\)$/)
})

// ------------------------------------------------------------------ 附件片段

test('附件片段：md 形态 → 链接语法，名字净化方括号', () => {
  assert.equal(
    attachmentSnippet('attachments/报告_1a2b3c4d.docx', '季度报告', true),
    '[季度报告](attachments/报告_1a2b3c4d.docx)'
  )
  assert.equal(
    attachmentSnippet('attachments/a.zip', 'a[b].zip', true),
    '[ab.zip](attachments/a.zip)'
  )
})

test('附件片段：html 形态 → <a href>，名字净化引号', () => {
  assert.equal(
    attachmentSnippet('attachments/a.docx', 'he said "hi"', false),
    '<a href="attachments/a.docx">he said hi</a>'
  )
})

test('附件片段：空名字兜底「附件」；entry 无空格括号不被 md 截断', () => {
  assert.equal(attachmentSnippet('attachments/x.bin', '', true), '[附件](attachments/x.bin)')
  const s = attachmentSnippet('attachments/演示_1a2b3c4d.mp4', '演示', true)
  assert.match(s, /^\[演示\]\(attachments\/[^()]+\)$/)
})
