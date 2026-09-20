<script setup lang="ts">
/**
 * 阅读区（FR-05）。S2 起信息栏带写操作入口（T8）：编辑正文 / 重命名 / 移动到… / 删除。
 * M4 起带**形态徽标与"复制源"**：库内正文可能是 md 包（`note.md`）或为知原生包
 * （`index.html`），阅读态要如实说明这篇是哪种，并允许把源文本整篇取走。
 *
 * - 入口仅在**库上下文**可用（`writable=false` 时整行不渲染）——为知视图只读；
 * - `version` 是写后重载计数：写成功 → App 递增 → iframe `:key` 变 → **强制重新加载**。
 *   （只靠 `guid` 当 key 的话，保存后 iframe 仍显示旧正文，是最容易被误判为"没保存成功"的坑。）
 * - 渲染仍由 Rust 侧产出（`wiznote://` → `read_note_document`）：md 包在这里被渲染成
 *   与阅读态同构的 HTML，**前端不需要知道渲染细节**，地址在 M3/M4 里一字未改。
 */
import { ref, watch, computed } from 'vue'
import { api, formatSize, writeErrorText, type NoteDetail, type AttachmentItem } from '../api'
import AttachmentModal from './AttachmentModal.vue'

type NoteRef = { guid: string; title: string; location: string }

const props = defineProps<{
  guid: string | null
  settings: { font_size: number; read_width: number; allow_remote: boolean }
  /** 是否库上下文（决定写入口是否出现） */
  writable: boolean
  /** 写后重载计数（变化即重载 iframe） */
  version: number
}>()

const emit = defineEmits<{
  (e: 'edit', n: NoteRef): void
  (e: 'rename', n: NoteRef): void
  (e: 'move', n: NoteRef): void
  (e: 'delete', n: NoteRef): void
}>()

const detail = ref<NoteDetail | null>(null)
const error = ref('')
const showAttachment = ref<AttachmentItem | null>(null)
const copyHint = ref('')
let hintTimer: number | null = null

watch(
  [() => props.guid, () => props.version],
  ([g]) => {
    detail.value = null
    showAttachment.value = null
    copyHint.value = ''
    if (g) load(g)
  },
  { immediate: true }
)

async function load(guid: string) {
  error.value = ''
  try {
    detail.value = await api.getNoteDetail(guid)
  } catch (e) {
    error.value = String(e)
  }
}

const frameSrc = computed(() =>
  props.guid ? `wiznote://${props.guid}/index.html` : ''
)

/** 正文形态（`null` = 读不到正文条目；此时不显示徽标，也不谎报成 HTML） */
const bodyFmt = computed(() => detail.value?.body_format ?? null)
const fmtLabel = computed(() =>
  bodyFmt.value === 'md' ? 'Markdown' : bodyFmt.value === 'html' ? '为知 HTML' : ''
)
const editHint = computed(() =>
  bodyFmt.value === 'md'
    ? '编辑正文（Markdown 源码 + 实时预览）'
    : '编辑正文（HTML 源码 + 实时预览）'
)

/**
 * 复制整篇源文本。先试 Clipboard API，失败退回隐藏 textarea + `execCommand('copy')`：
 * 打包后的页面跑在 `wiznote://`/`tauri://` 下，`navigator.clipboard` 的可用性
 * 跟着 webview 的"安全上下文"判定走，不能当成一定有。
 */
function copySource() {
  const d = detail.value
  if (!d) return
  api
    .getNoteSource(d.guid)
    .then((s) => {
      const text = s.text
      const done = () => {
        copyHint.value = `已复制${s.format === 'md' ? ' Markdown ' : ' HTML '}源（${Array.from(text).length} 字符）`
        if (hintTimer) window.clearTimeout(hintTimer)
        hintTimer = window.setTimeout(() => (copyHint.value = ''), 2600)
      }
      const fallback = () => {
        const ta = document.createElement('textarea')
        ta.value = text
        ta.style.position = 'fixed'
        ta.style.opacity = '0'
        document.body.appendChild(ta)
        ta.select()
        let ok = false
        try {
          ok = document.execCommand('copy')
        } catch {
          ok = false
        }
        document.body.removeChild(ta)
        if (ok) done()
        else copyHint.value = '复制失败：系统剪贴板不可用'
      }
      if (navigator.clipboard?.writeText) {
        navigator.clipboard.writeText(text).then(done, fallback)
      } else {
        fallback()
      }
    })
    .catch((e) => {
      copyHint.value = `复制失败：${writeErrorText(e)}`
    })
}

function asRef(): NoteRef | null {
  return detail.value
    ? { guid: detail.value.guid, title: detail.value.title, location: detail.value.location }
    : null
}
function act(kind: 'edit' | 'rename' | 'move' | 'delete') {
  const r = asRef()
  if (!r) return
  // 逐分支调用：重载式 emit 签名不接受联合类型实参
  if (kind === 'edit') emit('edit', r)
  else if (kind === 'rename') emit('rename', r)
  else if (kind === 'move') emit('move', r)
  else emit('delete', r)
}

async function openOriginal() {
  if (detail.value?.url) {
    await api.openExternalUrl(detail.value.url)
  }
}

function jumpFolder() {
  // 通知父组件切换目录
  if (detail.value) {
    window.dispatchEvent(
      new CustomEvent('wiz-jump-folder', { detail: detail.value.location })
    )
  }
}
</script>

<template>
  <div class="panel panel-reader">
    <template v-if="error">
      <div class="empty-hint">{{ error }}</div>
    </template>
    <template v-else-if="!detail">
      <div class="empty-hint">在左侧选择一篇笔记开始阅读<br /><br />↑↓ 可切换笔记，Cmd+F 检索</div>
    </template>
    <template v-else>
      <!-- FR-05.6 信息栏 -->
      <div class="info-bar">
        <div class="info-title" :title="detail.title">{{ detail.title }}</div>
        <div class="info-meta">
          <span v-if="fmtLabel" class="chip chip-fmt" :title="`库内正文形态：${fmtLabel}`">
            {{ fmtLabel }}
          </span>
          <a @click="jumpFolder" :title="detail.location">📁 {{ detail.location }}</a>
          <span>创建 {{ detail.created }}</span>
          <span>修改 {{ detail.data_modified }}</span>
          <span>{{ formatSize(detail.package_size) }}</span>
          <a v-if="detail.url" @click="openOriginal">查看原文 ↗</a>
          <a title="把整篇正文源文本复制到剪贴板" @click="copySource">复制源</a>
          <span v-if="copyHint" class="copy-hint">{{ copyHint }}</span>
          <!-- 写操作入口（仅库上下文；为知视图只读，不出现） -->
          <span v-if="writable" class="rw-actions">
            <button class="rw-btn primary" :title="editHint" @click="act('edit')">✎ 编辑正文</button>
            <button class="rw-btn" @click="act('rename')">重命名</button>
            <button class="rw-btn" @click="act('move')">移动到…</button>
            <button class="rw-btn danger" @click="act('delete')">删除</button>
          </span>
        </div>
      </div>
      <!-- 附件区（FR-05.6） -->
      <div v-if="detail.attachments.length" class="attach-bar">
        <span style="font-size: 12px; color: var(--text-2)">附件（{{ detail.attachments.length }}）：</span>
        <span
          v-for="a in detail.attachments"
          :key="a.file_path"
          class="attach-chip"
          :class="{ missing: !a.exists }"
          @click="a.exists && (showAttachment = a)"
        >
          📎 {{ a.display_name }}
          <span class="attach-origin">（{{ a.exists ? formatSize(a.size) : a.origin }}）</span>
        </span>
      </div>
      <!-- 阅读区：iframe 加载 wiznote:// 协议（沙箱由 CSP 保证） -->
      <div class="reader-scroll">
        <div
          class="reader-frame-wrap"
          :style="{ fontSize: settings.font_size + 'px' }"
        >
          <iframe
            :key="(props.guid || '') + ':' + props.version"
            class="reader-frame"
            :src="frameSrc"
            :style="{ maxWidth: settings.read_width + 'px' }"
          ></iframe>
        </div>
      </div>
    </template>

    <AttachmentModal
      v-if="showAttachment"
      :attachment="showAttachment"
      @close="showAttachment = null"
    />
  </div>
</template>

<style scoped>
.chip-fmt {
  font-size: 11px;
  padding: 1px 8px;
  border-radius: 999px;
  background: rgba(120, 120, 120, 0.16);
  color: var(--text-2);
  white-space: nowrap;
}
.copy-hint {
  font-size: 11px;
  color: var(--accent);
}
</style>
