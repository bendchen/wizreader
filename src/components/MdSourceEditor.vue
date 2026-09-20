<script setup lang="ts">
/**
 * Markdown 源码编辑器（M4；§24.2）。
 *
 * **它仍然是"源码编辑器"**，不是富文本：库内正文必须逐字段可预测，任何所见即所得
 * 编辑器都会规范化用户没碰过的标签/空白（D0「库必须无损」）。M4 补的是**源码编辑
 * 体验** —— 语法高亮、编辑辅助、快捷键、状态栏；渲染预览仍由 Rust 侧产出。
 *
 * 实现要点：`<textarea>` 上面叠一层**逐字符等长**的高亮层（`pre > code`）。
 * 这要求两边字体度量、内边距、空白折叠规则完全一致，且高亮层不许增删一个字符
 * （`mdhl.ts` 的不变量由 `npm run test:logic` 逐字符守）。textarea 文字本身透明、
 * 只留 `caret-color`，于是"看到的是高亮层、编辑的是 textarea"。
 *
 * 为什么不用 CodeMirror：见 §24.2（D-M4-2）—— 要的是高亮 + 辅助，不是编辑器运行时；
 * 且 overlay 高亮是纯函数，能在 Node 里直接断言。代价写在 §24.5 边界里（没有多光标、
 * 没有折叠；超大笔记退化为无高亮的纯文本编辑）。
 */
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { highlightMd } from '../mdhl'
import {
  continueFence,
  continueList,
  continueTable,
  indentSelection,
  runAction,
  type EditResult,
  type MdAction,
  type Snapshot,
} from '../mdedit'

const props = defineProps<{ modelValue: string }>()
const emit = defineEmits<{
  (e: 'update:modelValue', v: string): void
  (e: 'input'): void
}>()

/**
 * 超过这个规模就关掉高亮（退化为纯文本编辑，但**仍然可编辑**、预览照旧）。
 *
 * 阈值不是拍的：M4 在 headless Chrome 上量过"敲一个字符"的完整代价
 * （`highlightMd` 计算 + `v-html` 整段替换 DOM），曲线近似线性 ——
 *
 * | 正文规模 | 高亮 span | 计算 | DOM 替换 | 每次按键 |
 * |---|---|---|---|---|
 * | 2.0 万字符 | 1,099 | 2.8 ms | 3.5 ms | **6.3 ms** |
 * | 6.0 万字符 | 3,359 | 4.2 ms | 6.0 ms | **10.2 ms** |
 * | 12.0 万字符 | 6,582 | 6.9 ms | 12.2 ms | **19.1 ms** |
 * | 19.1 万字符 | 10,709 | 9.2 ms | 19.0 ms | **28.2 ms** |
 *
 * 6 万 ≈ 10 ms/键，刚好是一帧的预算，故选它；`~/wiz-lib-md` 1780 篇里只有 **27 篇**
 * （1.52%）会被切到纯文本模式（p50 = 2,764、p90 = 22,590、p99 = 69,627、max = 190,903）。
 * 宁可让 1.5% 的笔记少个高亮，也不要让它们打字掉帧。
 */
const HL_LIMIT = 60_000

const ta = ref<HTMLTextAreaElement | null>(null)
const hl = ref<HTMLPreElement | null>(null)
const line = ref(1)
const col = ref(1)
const selLen = ref(0)

const highlightOn = computed(() => props.modelValue.length <= HL_LIMIT)

const html = computed(() => {
  if (!highlightOn.value) return ''
  const h = highlightMd(props.modelValue)
  // `<pre>` 紧邻的首个换行会被 HTML 解析器丢掉；正文若以空行开头，覆盖层就会
  // 比 textarea 少一行、从第二行起全部错位。用字符实体写成同一个换行，规避这条规则。
  return h.startsWith('\n') ? '&#10;' + h.slice(1) : h
})

function snapshot(): Snapshot {
  const el = ta.value
  return { text: el ? el.value : props.modelValue, start: el?.selectionStart ?? 0, end: el?.selectionEnd ?? 0 }
}

function updateCaret() {
  const el = ta.value
  if (!el) return
  const upto = el.value.slice(0, el.selectionStart)
  const rows = upto.split('\n')
  // 用码点数而不是 UTF-16 长度：中文一个字应算一列
  col.value = Array.from(rows[rows.length - 1]).length + 1
  line.value = rows.length
  selLen.value = el.selectionEnd - el.selectionStart
}

function syncScroll() {
  const el = ta.value
  const layer = hl.value
  if (!el || !layer) return
  layer.scrollTop = el.scrollTop
  layer.scrollLeft = el.scrollLeft
}

/**
 * 覆盖层尺寸跟随 textarea 的**内容盒**（`clientWidth/clientHeight`）。
 *
 * 实测（headless Chrome，M4 验收）：textarea 内容超出可视高度后出现竖滚动条，
 * `clientWidth` 比覆盖层窄 **15px** ⇒ 折行位置提前 ⇒ 同一篇 25 行 vs 26 行，
 * 光标从第一处折行起就与高亮文字错开。macOS 上滚动条是否为"仅滚动时显示"由
 * 系统设置决定（WKWebView 跟随系统），所以这条不是"某种配置下才出现"的边角。
 *
 * 不能用 `ResizeObserver` 顶替：它报的是 border-box，滚动条出现/消失不会改变它。
 * 故在"内容变化 / 窗口尺寸变化"时主动对齐，用 rAF 让浏览器先完成滚动条布局。
 */
function syncBox() {
  const el = ta.value
  const layer = hl.value
  if (!el || !layer) return
  layer.style.width = `${el.clientWidth}px`
  layer.style.height = `${el.clientHeight}px`
  syncScroll()
}

let rafId = 0
function scheduleSyncBox() {
  if (rafId) window.cancelAnimationFrame(rafId)
  rafId = window.requestAnimationFrame(() => {
    rafId = 0
    syncBox()
  })
}

/** 求"最小改动区间"，供 `execCommand('insertText')` 用 —— 它保留原生撤销栈 */
function diffRange(oldText: string, newText: string) {
  let p = 0
  const maxP = Math.min(oldText.length, newText.length)
  while (p < maxP && oldText[p] === newText[p]) p += 1
  let s = 0
  const maxS = Math.min(oldText.length - p, newText.length - p)
  while (s < maxS && oldText[oldText.length - 1 - s] === newText[newText.length - 1 - s]) s += 1
  return { from: p, to: oldText.length - s, insert: newText.slice(p, newText.length - s) }
}

/**
 * 套用一次编辑。
 *
 * 走 `execCommand('insertText')` 而不是直接改 `value`：原生 textarea 的撤销栈只在
 * 这条路径上连续（直接赋值会把 Cmd+Z 整段作废）。失败或结果不符就退回直接赋值 ——
 * 文本正确永远优先于撤销粒度。
 */
function applyEdit(res: EditResult | null) {
  const el = ta.value
  if (!el || !res) return
  el.focus()
  if (res.text !== el.value) {
    const d = diffRange(el.value, res.text)
    let ok = false
    try {
      el.setSelectionRange(d.from, d.to)
      ok = document.execCommand('insertText', false, d.insert)
    } catch {
      ok = false
    }
    if (!ok || el.value !== res.text) el.value = res.text
  }
  el.setSelectionRange(res.selStart, res.selEnd)
  emit('update:modelValue', el.value)
  emit('input')
  scheduleSyncBox()
  updateCaret()
}

/** 工具栏入口（抽屉里那排按钮调它） */
function applyAction(action: MdAction) {
  applyEdit(runAction(snapshot(), action))
}

function onKeydown(e: KeyboardEvent) {
  const el = ta.value
  if (!el) return
  const snap: Snapshot = { text: el.value, start: el.selectionStart, end: el.selectionEnd }
  const meta = e.metaKey || e.ctrlKey

  if (e.key === 'Tab') {
    e.preventDefault()
    applyEdit(indentSelection(snap, e.shiftKey ? -1 : 1))
    return
  }
  if (e.key === 'Enter' && !e.shiftKey && !meta && snap.start === snap.end) {
    const r = continueList(snap) ?? continueFence(snap) ?? continueTable(snap)
    if (r) {
      e.preventDefault()
      applyEdit(r)
    }
    return
  }
  if (!meta) return

  // 数字键用 `e.code`：Shift+7 在美式键盘上 `e.key` 是 `&`
  const digit = /^Digit([0-9])$/.exec(e.code)?.[1]
  if (digit) {
    e.preventDefault()
    if (e.shiftKey) {
      if (digit === '7') applyAction('ol')
      else if (digit === '8') applyAction('ul')
      else if (digit === '9') applyAction('task')
    } else if (digit === '0') {
      applyAction('h0')
    } else if (digit >= '1' && digit <= '4') {
      applyAction(`h${digit}` as MdAction)
    }
    return
  }
  switch (e.key.toLowerCase()) {
    case 'b':
      e.preventDefault()
      applyAction('bold')
      break
    case 'i':
      e.preventDefault()
      applyAction('italic')
      break
    case 'e':
      e.preventDefault()
      applyAction('code')
      break
    case 'k':
      e.preventDefault()
      applyAction(e.shiftKey ? 'image' : 'link')
      break
    case 'l':
      if (e.shiftKey) {
        e.preventDefault()
        applyAction('table')
      }
      break
    default:
      break
  }
}

/** 输入变化（含中文输入法组合结束）后刷新状态栏与滚动同步 */
function onInput() {
  emit('update:modelValue', ta.value?.value ?? '')
  emit('input')
  scheduleSyncBox()
  updateCaret()
}

function focus() {
  ta.value?.focus()
}

defineExpose({ applyAction, focus })

watch(
  () => props.modelValue,
  () => scheduleSyncBox()
)

onMounted(() => {
  updateCaret()
  syncBox()
  window.addEventListener('resize', scheduleSyncBox)
})

onBeforeUnmount(() => {
  window.removeEventListener('resize', scheduleSyncBox)
  if (rafId) window.cancelAnimationFrame(rafId)
})
</script>

<template>
  <div class="md-editor">
    <div class="wrap">
      <pre ref="hl" class="hl" aria-hidden="true"><code v-html="html"></code></pre>
      <textarea
        ref="ta"
        class="input"
        :class="{ plain: !highlightOn }"
        :value="modelValue"
        spellcheck="false"
        autocapitalize="off"
        autocomplete="off"
        @input="onInput"
        @keydown="onKeydown"
        @scroll="syncScroll"
        @click="updateCaret"
        @keyup="updateCaret"
        @select="updateCaret"
      ></textarea>
    </div>
    <div class="status">
      <span
        v-if="!highlightOn"
        class="warn"
        :title="`笔记超过 ${HL_LIMIT.toLocaleString()} 字符，已关闭语法高亮以保输入流畅（仍可正常编辑）`"
      >
        纯文本模式（笔记过大，已关高亮）
      </span>
      <span class="dim">行 {{ line }} · 列 {{ col }}</span>
      <span v-if="selLen" class="dim">选中 {{ selLen }} 字符</span>
      <span class="spacer"></span>
      <span class="dim">{{ modelValue.length }} 字符 · LF · UTF-8</span>
    </div>
  </div>
</template>

<style scoped>
.md-editor {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  /* 语法色板：浅色主题 */
  --hl-mark: #8250df;
  --hl-code: #0a3069;
  --hl-code-bg: rgba(120, 120, 120, 0.12);
  --hl-link: #0969da;
  --hl-quote: #57606a;
  --hl-dim: rgba(120, 120, 120, 0.55);
}
@media (prefers-color-scheme: dark) {
  :global(:root.theme-system) .md-editor {
    --hl-mark: #c4a7f5;
    --hl-code: #9ecbff;
    --hl-code-bg: rgba(200, 200, 200, 0.1);
    --hl-link: #79b8ff;
    --hl-quote: #9aa1ac;
  }
}
:global(:root.theme-dark) .md-editor {
  --hl-mark: #c4a7f5;
  --hl-code: #9ecbff;
  --hl-code-bg: rgba(200, 200, 200, 0.1);
  --hl-link: #79b8ff;
  --hl-quote: #9aa1ac;
}
.wrap {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: hidden;
  background: var(--bg);
}
/* 两层共用同一套字体度量：任何一项不同都会让覆盖层与光标错位 */
.hl,
.input,
.wrap {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12.5px;
  line-height: 1.55;
  letter-spacing: 0;
  word-spacing: 0;
  tab-size: 2;
  font-variant-ligatures: none;
  white-space: pre-wrap;
  overflow-wrap: break-word;
  word-break: normal;
  margin: 0;
  padding: 10px 12px;
  border: 0;
}
.hl {
  position: absolute;
  inset: 0;
  overflow: hidden;
  color: var(--text);
  pointer-events: none;
}
.input {
  position: absolute;
  inset: 0;
  z-index: 1;
  width: 100%;
  height: 100%;
  border: 0;
  outline: none;
  resize: none;
  background: transparent;
  color: transparent;
  caret-color: var(--text);
  overflow: auto;
}
/* 高亮关闭时 textarea 自己显示文字 */
.input.plain {
  color: var(--text);
}
.input::selection {
  /* 半透明：选中区下面还要透出高亮层的文字，否则一片色块看不清 */
  background: rgba(80, 140, 255, 0.28);
}
.status {
  flex: none;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 3px 10px;
  font-size: 11px;
  color: var(--text-2);
  border-top: 1px solid var(--border);
  background: var(--bg-2);
}
.status .spacer {
  flex: 1;
}
.status .dim {
  color: var(--text-2);
}
.status .warn {
  color: #a85c00;
}
</style>

<style>
/* 高亮层的类名（v-html 产生的节点拿不到 scoped 属性，只能写成全局样式）。
   前缀统一 `md-`，与组件名一致，冲突面小、可检索。 */
.md-editor .hl code {
  font: inherit;
}
.md-editor .md-delim {
  opacity: 0.55; /* 定界符调暗，突出内容 */
}
.md-editor .md-h1,
.md-editor .md-h2,
.md-editor .md-h3,
.md-editor .md-h4,
.md-editor .md-h5,
.md-editor .md-h6 {
  color: var(--hl-mark);
  font-weight: 700;
}
.md-editor .md-hc {
  font-weight: 600;
}
.md-editor .md-quote {
  color: var(--hl-quote);
}
.md-editor .md-quote-c {
  color: var(--hl-quote);
  font-style: italic;
}
.md-editor .md-list {
  color: var(--hl-link);
  font-weight: 700;
}
.md-editor .md-task-box {
  color: var(--hl-mark);
}
.md-editor .md-hr {
  color: var(--hl-quote);
}
.md-editor .md-tbl {
  color: var(--hl-code);
}
.md-editor .md-fence {
  color: var(--hl-mark);
  font-weight: 600;
}
.md-editor .md-code {
  color: var(--hl-code);
}
.md-editor .md-ic {
  color: var(--hl-code);
  background: var(--hl-code-bg);
  border-radius: 3px;
}
.md-editor .md-strong {
  color: var(--hl-mark);
  font-weight: 700;
}
.md-editor .md-em {
  color: var(--hl-mark);
  font-style: italic;
}
.md-editor .md-strike {
  text-decoration: line-through;
}
.md-editor .md-link {
  color: var(--hl-link);
  text-decoration: underline;
}
.md-editor .md-url {
  color: var(--hl-dim);
}
.md-editor .md-esc {
  color: var(--hl-dim);
}
</style>
