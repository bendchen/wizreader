<script setup lang="ts">
/**
 * 笔记正文编辑抽屉（§4.3 / T8；M3 起形态自适应，M4 起 md 包用专用编辑器）。
 *
 * 为什么仍是"源码 + 预览"而不是富文本：库内正文（无论 md 还是为知原生 HTML）都含
 * `<script>`、CodeMirror 序列化 DOM、绝对定位样式等结构；任何所见即所得编辑器都会规范
 * 用户没碰过的标签（属性顺序、空白、自闭合），保存后与原文不再逐字段一致 ——
 * 与 D0「库必须无损」冲突。源码编辑是唯一不引入生成歧义的做法。
 *
 * M4 的分工：
 * - **md 包** → [`MdSourceEditor`]（语法高亮 + 编辑辅助 + 状态栏；逻辑在 `mdedit.ts`/`mdhl.ts`）
 * - **native 包** → 仍是 `<textarea>`：`index.html` 是机器序列化的 DOM，给它做 HTML
 *   语法辅助只会鼓励用户手改结构（那才是真的会写坏），保持"纯文本编辑"最诚实。
 *
 * **形态由后端告知、不由前端选**：`getNoteSource` 返回 `format`（`md` = 包内是
 * `note.md`、`html` = 为知原生包），保存走 `saveNoteSource`（后端按包内实况自动分派）。
 *
 * 预览走 `wiznote://{guid}/index.html?draft=<token>`（不是 srcdoc）：
 * 相对资源 `index_files/…` 照常从 zip 解析、CSP 与兼容层注入与阅读态完全一致，
 * 而草稿只在该 token 的请求上生效，不会污染普通阅读；md 草稿由后端渲染后返回。
 */
import { ref, computed, watch, onMounted, onBeforeUnmount, nextTick } from 'vue'
import { api, writeErrorText, type NoteWriteReport } from '../api'
import MdSourceEditor from './MdSourceEditor.vue'
import type { MdAction } from '../mdedit'
import {
  buildPastePayload,
  bytesToBase64,
  dataUriToBase64,
  decidePaste,
  extractDataUris,
  imageSnippet,
  attachmentSnippet,
  replaceDataUris,
  type PastePayload,
} from '../noteimage'

const props = defineProps<{ guid: string; title: string; location: string }>()
const emit = defineEmits<{
  (e: 'close'): void
  (e: 'saved', rep: NoteWriteReport): void
}>()

/** 正文源码（md 包 → Markdown；native 包 → HTML） */
const src = ref('')
const original = ref('')
/** 包内正文形态：决定编辑控件与页脚提示（保存口径由后端按包决定，与它一致） */
const format = ref<'md' | 'html'>('html')
const token = ref('')
const seq = ref(0)
const loading = ref(true)
const saving = ref(false)
const loadError = ref('')
const saveError = ref('')
const result = ref<NoteWriteReport | null>(null)
/** 预览可见性：长文写作时"只看源码"能让编辑区宽一倍 */
const showPreview = ref(true)
const editor = ref<InstanceType<typeof MdSourceEditor> | null>(null)
/** native 包的纯文本 textarea（html 形态没有专用编辑器，插图/粘贴直接操作它） */
const nativeTa = ref<HTMLTextAreaElement | null>(null)
/** 图片上传中（选图/粘贴共用；期间按钮置灰防重复入包） */
const imgBusy = ref(false)
let previewTimer: number | null = null

const dirty = computed(() => src.value !== original.value)
const previewStale = computed(() => !token.value)
const isMd = computed(() => format.value === 'md')
/** 包内正文档名（标签与页脚共用，避免两处各写一份） */
const entryName = computed(() => (isMd.value ? 'note.md' : 'index.html'))
const sourceLabel = computed(() =>
  isMd.value ? 'Markdown 源码（note.md）' : 'HTML 源码（index.html）'
)

/** 工具栏：`act` 即 `mdedit.ts` 的动作名，快捷键与按钮共用同一份口径 */
const TOOLS: { act: MdAction; label: string; hint: string }[] = [
  { act: 'h1', label: 'H1', hint: '一级标题（⌘1）' },
  { act: 'h2', label: 'H2', hint: '二级标题（⌘2）' },
  { act: 'h3', label: 'H3', hint: '三级标题（⌘3）' },
  { act: 'h0', label: '正文', hint: '取消标题（⌘0）' },
  { act: 'bold', label: 'B', hint: '加粗（⌘B）' },
  { act: 'italic', label: 'I', hint: '斜体（⌘I）' },
  { act: 'strike', label: 'S', hint: '删除线' },
  { act: 'code', label: '</>', hint: '行内代码（⌘E）' },
  { act: 'quote', label: '❝', hint: '引用' },
  { act: 'ul', label: '•', hint: '无序列表（⇧⌘8）' },
  { act: 'ol', label: '1.', hint: '有序列表（⇧⌘7）' },
  { act: 'task', label: '☑', hint: '待办列表（⇧⌘9）' },
  { act: 'fence', label: '```', hint: '代码块' },
  { act: 'link', label: '🔗', hint: '链接（⌘K）' },
  { act: 'image', label: '🖼', hint: '图片（⇧⌘K）' },
  { act: 'table', label: '▦', hint: '表格（⇧⌘L）' },
  { act: 'hr', label: '—', hint: '水平线' },
  { act: 'outdent', label: '⇤', hint: '减少缩进（⇧Tab）' },
]

watch(
  () => props.guid,
  async (g) => {
    if (!g) return
    loading.value = true
    loadError.value = ''
    saveError.value = ''
    result.value = null
    try {
      const s = await api.getNoteSource(g)
      format.value = s.format
      src.value = s.text
      original.value = s.text
      await refreshPreview()
    } catch (e) {
      loadError.value = writeErrorText(e)
    } finally {
      loading.value = false
    }
  },
  { immediate: true }
)

/** 把当前文本登记为草稿并让预览 iframe 重新指向带 token 的地址 */
async function refreshPreview() {
  try {
    token.value = await api.setNoteDraft(props.guid, src.value)
    seq.value += 1
  } catch (e) {
    token.value = ''
    saveError.value = writeErrorText(e)
  }
}

/** 输入即预览（防抖 350ms）：预览与保存是两条路径，预览失败不影响继续编辑 */
function onInput() {
  if (previewTimer) window.clearTimeout(previewTimer)
  previewTimer = window.setTimeout(refreshPreview, 350)
}

function act(action: MdAction) {
  // M4：图片按钮不再是"敲语法骨架"，而是真的选一张图入包（⇧⌘K 在编辑器里同一条路）
  if (action === 'image') {
    void insertImageFromFile()
    return
  }
  editor.value?.applyAction(action)
}

// ---- M4 图片插入（文件选择器 / 粘贴，两条入口汇到这里） ----

/** 系统对话框选图 → 后端读文件写入包内 index_files/ → 在光标处插入引用 */
async function insertImageFromFile() {
  if (imgBusy.value || loading.value || loadError.value) return
  imgBusy.value = true
  saveError.value = ''
  try {
    const { open } = await import('@tauri-apps/plugin-dialog')
    const sel = await open({
      multiple: false,
      title: '选择要插入的图片',
      filters: [{ name: '图片', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg'] }],
    })
    if (typeof sel !== 'string' || !sel) return
    const rep = await api.addNoteImageFile(props.guid, sel)
    insertImageRef(rep.entry, rep.entry)
  } catch (e) {
    saveError.value = writeErrorText(e)
  } finally {
    imgBusy.value = false
  }
}

/** M4 最小版：选任意文件作为附件入包（attachments/），正文插链接引用（md [名](…)；html <a>） */
async function insertAttachmentFromFile() {
  if (imgBusy.value || loading.value || loadError.value) return
  imgBusy.value = true
  saveError.value = ''
  try {
    const { open } = await import('@tauri-apps/plugin-dialog')
    const sel = await open({ multiple: false, title: '选择要插入的附件' })
    if (typeof sel !== 'string' || !sel) return
    const rep = await api.addNoteAttachment(props.guid, sel)
    const name = sel.split('/').pop() || rep.entry
    insertIntoBody(attachmentSnippet(rep.entry, name, isMd.value))
  } catch (e) {
    saveError.value = writeErrorText(e)
  } finally {
    imgBusy.value = false
  }
}

/** 按 dt 入库返回的 entry 在光标处插入引用片段（md → ![]()；html → <img>） */
function insertImageRef(entry: string, alt: string) {
  const snippet = imageSnippet(entry, alt, isMd.value)
  if (isMd.value) {
    editor.value?.insertSnippet(snippet)
  } else {
    insertIntoTextarea(snippet)
  }
  onInput()
}

/** native 包 textarea 的光标插入（走 v-model 数据源；光标手动复位） */
function insertIntoTextarea(snippet: string) {
  const ta = nativeTa.value
  const cur = src.value
  const s = ta?.selectionStart ?? cur.length
  const e = ta?.selectionEnd ?? s
  src.value = cur.slice(0, s) + snippet + cur.slice(e)
  const at = s + snippet.length
  void nextTick(() => {
    if (!ta) return
    ta.focus()
    ta.setSelectionRange(at, at)
  })
}

/** 粘贴处理（md 编辑器 emit 过来 / native textarea 直呼）：有图就拦截入库 */
async function onPasteImage(payload: PastePayload) {
  const kind = decidePaste(payload)
  if (kind === 'none' || imgBusy.value) return
  imgBusy.value = true
  saveError.value = ''
  try {
    if (kind === 'files') {
      // 截图 / 复制的图片文件：逐张入包（一张失败不挡后续，错误都摆出来）
      for (const f of payload.files) {
        try {
          const buf = await f.arrayBuffer()
          const b64 = bytesToBase64(new Uint8Array(buf))
          const rep = await api.addNoteImageData(props.guid, b64, f.name || null)
          insertImageRef(rep.entry, f.name || rep.entry)
        } catch (e) {
          saveError.value = writeErrorText(e)
        }
      }
      return
    }
    // 文本/富文本里的内嵌图（data: URI）：抽出入库 → 引用替换回正文
    const raw = kind === 'text-data' ? payload.text : payload.html
    const uris = extractDataUris(raw)
    const map: Record<string, string> = {}
    for (const uri of uris) {
      try {
        const rep = await api.addNoteImageData(props.guid, dataUriToBase64(uri), null)
        map[uri] = rep.entry
      } catch (e) {
        saveError.value = writeErrorText(e)
      }
    }
    if (Object.keys(map).length === 0) return
    // 富文本来源没有纯文本可要（有也就没图了）→ 原样保留标记结构，只换图的地址
    insertIntoBody(replaceDataUris(raw, map))
  } finally {
    imgBusy.value = false
  }
}

/** 大段替换文本的插入入口（md 走编辑器保留撤销栈；html 走 textarea） */
function insertIntoBody(text: string) {
  if (isMd.value) editor.value?.insertSnippet(text)
  else insertIntoTextarea(text)
  onInput()
}

/** native textarea 的 paste 钩子（同步抽取 → 异步入库） */
function onPasteNative(e: ClipboardEvent) {
  const payload = buildPastePayload(e)
  if (decidePaste(payload) === 'none') return
  e.preventDefault()
  void onPasteImage(payload)
}

async function save() {
  if (saving.value) return
  saving.value = true
  saveError.value = ''
  try {
    const rep = await api.saveNoteSource(props.guid, src.value)
    result.value = rep
    original.value = src.value
    token.value = ''
    emit('saved', rep)
  } catch (e) {
    // 保存失败要把**具体错误码**摆出来（T8），并说明这一篇在库内没有被改动
    saveError.value = `${writeErrorText(e)}\n\n（库内该篇未做任何改动）`
  } finally {
    saving.value = false
  }
}

function revert() {
  src.value = original.value
  refreshPreview()
}

async function close() {
  if (dirty.value && !confirm('正文有未保存的改动，放弃并关闭？')) return
  await api.clearNoteDraft().catch(() => undefined)
  emit('close')
}

// ---- macOS 窗口红绿灯（编辑抽屉不是独立窗口，三个钮均落在抽屉头部最左）----
// 红 = 关闭编辑器（与原「关闭」同一条路径，未保存会先确认）；
// 黄 = 最小化主窗口；绿 = 最大化 / 还原主窗口（经 Tauri 窗口 API）。
async function winMinimize() {
  try {
    const { getCurrentWindow } = await import('@tauri-apps/api/window')
    await getCurrentWindow().minimize()
  } catch {
    /* 非 Tauri 环境（纯浏览器 dev）忽略 */
  }
}
async function winToggleMax() {
  try {
    const { getCurrentWindow } = await import('@tauri-apps/api/window')
    await getCurrentWindow().toggleMaximize()
  } catch {
    /* 非 Tauri 环境忽略 */
  }
}

/** ⌘S 保存 / ⌘Enter 保存：编辑器不接管这两个键，抽屉统一收口 */
function onKeydown(e: KeyboardEvent) {
  if ((e.metaKey || e.ctrlKey) && (e.key.toLowerCase() === 's' || e.key === 'Enter')) {
    e.preventDefault()
    if (!saving.value && !loading.value) void save()
  }
}

onMounted(() => window.addEventListener('keydown', onKeydown, true))
onBeforeUnmount(() => {
  window.removeEventListener('keydown', onKeydown, true)
  if (previewTimer) window.clearTimeout(previewTimer)
  api.clearNoteDraft().catch(() => undefined)
})

const previewSrc = computed(() =>
  token.value ? `wiznote://${props.guid}/index.html?draft=${token.value}&n=${seq.value}` : ''
)
const saved = computed(() => result.value !== null && !saveError.value)
</script>

<template>
  <div class="drawer-backdrop" @click.self="close">
    <div class="drawer">
      <div class="drawer-head">
        <!-- macOS 通用窗口按钮：关闭 / 最小化 / 最大化 -->
        <div class="tl-group">
          <button class="tl tl-close" title="关闭编辑器" @click="close"></button>
          <button class="tl tl-min" title="最小化窗口" @click="winMinimize"></button>
          <button class="tl tl-max" title="最大化 / 还原窗口" @click="winToggleMax"></button>
        </div>
        <strong class="drawer-title" :title="title">编辑正文：{{ title }}</strong>
        <span class="drawer-loc" :title="location">{{ location }}</span>
        <span class="chip chip-fmt">{{ isMd ? 'Markdown' : 'HTML' }}</span>
        <span v-if="dirty" class="chip chip-warn">未保存</span>
        <span v-else-if="saved" class="chip chip-ok">已保存</span>
        <span class="spacer"></span>
        <button :disabled="saving || loading" @click="save" title="保存（⌘S）">
          {{ saving ? '保存中…' : '保存' }}
        </button>
        <button :disabled="saving || !dirty" @click="revert">撤销改动</button>
        <button :disabled="saving" @click="showPreview = !showPreview">
          {{ showPreview ? '隐藏预览' : '显示预览' }}
        </button>
        <button :disabled="saving" @click="close">关闭</button>
      </div>

      <!-- 工具栏只在 md 形态出现：native 包不做 HTML 语法辅助（见文件头注释） -->
      <div v-if="isMd && !loading && !loadError" class="toolbar">
        <button
          v-for="t in TOOLS"
          :key="t.act"
          class="tool"
          :title="t.act === 'image' ? '插入图片（⇧⌘K）：选择图片文件写入笔记包' : t.hint"
          :disabled="saving || imgBusy"
          @click="act(t.act)"
        >
          {{ t.act === 'image' && imgBusy ? '…' : t.label }}
        </button>
        <button
          class="tool"
          title="插入附件：选择任意文件写入笔记包 attachments/，正文插入链接（上限 50 MB）"
          :disabled="saving || imgBusy"
          @click="insertAttachmentFromFile"
        >
          📎
        </button>
        <span class="spacer"></span>
        <span class="tool-hint">
          Tab 缩进 · Enter 续列表/引用 · 围栏与表格自动续行 · ⌘S 保存
        </span>
      </div>

      <div v-if="loadError" class="drawer-err">{{ loadError }}</div>
      <div v-else-if="loading" class="empty-hint" style="padding: 20px">读取正文中…</div>

      <div v-else class="drawer-body" :class="{ single: !showPreview }">
        <div class="pane">
          <div class="pane-head">
            {{ sourceLabel }}（可直接改；保存前会校验空正文与宿主引用）
          </div>
          <!-- md 包：专用源码编辑器（高亮 + 编辑辅助）；插图/粘贴有图 → 抽屉统一上传 -->
          <MdSourceEditor
            v-if="isMd"
            ref="editor"
            v-model="src"
            @input="onInput"
            @paste-image="onPasteImage"
            @image-request="insertImageFromFile"
          />
          <!-- native 包：纯文本编辑，不做 HTML 语法辅助；粘贴有图同样入包 -->
          <textarea
            v-else
            ref="nativeTa"
            v-model="src"
            class="src"
            spellcheck="false"
            @input="onInput"
            @paste="onPasteNative"
          ></textarea>
        </div>
        <div v-if="showPreview" class="pane">
          <div class="pane-head">
            实时预览（与阅读态同一条协议；笔记内脚本仍被 CSP 阻断）
            <span v-if="previewStale" class="pane-warn">预览未就绪</span>
          </div>
          <iframe v-if="previewSrc" class="preview" :src="previewSrc"></iframe>
          <div v-else class="empty-hint" style="padding: 16px">预览暂不可用，可直接编辑源码</div>
        </div>
      </div>

      <div v-if="saveError" class="drawer-err pre">{{ saveError }}</div>
      <div v-if="result" class="drawer-result">
        已保存：{{ result.exported_path }}（{{ result.exported_size }} B，rev {{ result.revision }}）
        <span v-if="!result.index_updated" class="chip chip-warn">索引未同步，建议重建索引</span>
        <ul v-if="result.warnings.length" class="warn-list">
          <li v-for="(w, i) in result.warnings" :key="i">{{ w }}</li>
        </ul>
      </div>
      <div class="drawer-foot">
        提示：保存只替换 zip 内 <code>{{ entryName }}</code>；插入图片会追加进包内
        <code>index_files/</code>，插入附件（📎）会追加进 <code>attachments/</code>
        并在正文插链接（均用相对路径引用，清单 MD5 / 体积 / 修订号随之更新）。
        <span v-if="isMd">编辑器只插入你按下的字符，不做任何自动整理。</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.drawer-backdrop {
  position: fixed;
  inset: 0;
  z-index: 60;
  background: rgba(0, 0, 0, 0.35);
  display: flex;
  justify-content: center;
  align-items: stretch;
  padding: 24px;
}
.drawer {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 10px;
  overflow: hidden;
}
.drawer-head {
  flex: none;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 12px;
  border-bottom: 1px solid var(--border);
}
/* macOS 红绿灯：红=关编辑器，黄=最小化窗口，绿=最大化/还原窗口；悬停整组时显示符号 */
.tl-group {
  flex: none;
  display: flex;
  align-items: center;
  gap: 8px;
  margin-right: 4px;
}
.tl {
  width: 12px;
  height: 12px;
  padding: 0;
  border-radius: 50%;
  border: 1px solid rgba(0, 0, 0, 0.15);
  cursor: pointer;
  position: relative;
  flex: none;
}
.tl-close {
  background: #ff5f57;
}
.tl-min {
  background: #febc2e;
}
.tl-max {
  background: #28c840;
}
.tl::after {
  content: '';
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 9px;
  line-height: 1;
  color: rgba(0, 0, 0, 0.55);
  opacity: 0;
}
.tl-group:hover .tl::after {
  opacity: 1;
}
.tl-close::after {
  content: '×';
}
.tl-min::after {
  content: '−';
}
.tl-max::after {
  content: '+';
}
.tl:active {
  filter: brightness(0.9);
}
.drawer-title {
  font-size: 14px;
  max-width: 40%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.drawer-loc {
  font-size: 12px;
  color: var(--text-2);
  max-width: 24%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.spacer {
  flex: 1;
}
.toolbar {
  flex: none;
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 6px 10px;
  border-bottom: 1px solid var(--border);
  background: var(--bg-2);
  flex-wrap: wrap;
}
.tool {
  min-width: 26px;
  height: 24px;
  padding: 0 6px;
  font-size: 12px;
  line-height: 1;
  border: 1px solid var(--border);
  border-radius: 5px;
  background: var(--bg);
  color: var(--text);
  cursor: pointer;
}
.tool:hover:not(:disabled) {
  border-color: var(--accent);
  color: var(--accent);
}
.tool-hint {
  font-size: 11px;
  color: var(--text-2);
}
.chip {
  font-size: 11px;
  padding: 1px 8px;
  border-radius: 999px;
  white-space: nowrap;
}
.chip-warn {
  background: rgba(232, 137, 12, 0.18);
  color: #a85c00;
}
.chip-fmt {
  background: rgba(120, 120, 120, 0.16);
  color: var(--text-2);
}
.chip-ok {
  background: rgba(46, 125, 50, 0.15);
  color: #2e7d32;
}
.drawer-body {
  flex: 1;
  min-height: 0;
  display: flex;
  gap: 1px;
  background: var(--border);
}
.pane {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  background: var(--bg);
}
.pane-head {
  flex: none;
  padding: 6px 10px;
  font-size: 12px;
  color: var(--text-2);
  border-bottom: 1px solid var(--border);
  display: flex;
  gap: 8px;
}
.pane-warn {
  color: var(--danger);
}
.src {
  flex: 1;
  min-height: 0;
  border: none;
  outline: none;
  resize: none;
  padding: 10px 12px;
  background: var(--bg);
  color: var(--text);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12.5px;
  line-height: 1.55;
  tab-size: 2;
}
.preview {
  flex: 1;
  min-height: 0;
  border: none;
  background: #fff;
}
.drawer-err {
  flex: none;
  margin: 8px 12px;
  padding: 8px 12px;
  border-radius: 8px;
  background: rgba(192, 57, 43, 0.08);
  border: 1px solid rgba(192, 57, 43, 0.35);
  color: var(--danger);
  font-size: 12.5px;
}
.drawer-err.pre {
  white-space: pre-wrap;
}
.drawer-result {
  flex: none;
  margin: 8px 12px;
  padding: 8px 12px;
  border-radius: 8px;
  background: var(--bg-2);
  font-size: 12.5px;
  color: var(--text-2);
}
.warn-list {
  margin: 6px 0 0;
  padding-left: 18px;
}
.drawer-foot {
  flex: none;
  padding: 8px 12px;
  border-top: 1px solid var(--border);
  font-size: 11.5px;
  color: var(--text-2);
}
</style>
