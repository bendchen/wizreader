<script setup lang="ts">
import { ref, watch, computed } from 'vue'
import { api, formatSize, type NoteDetail, type AttachmentItem } from '../api'
import AttachmentModal from './AttachmentModal.vue'

const props = defineProps<{
  guid: string | null
  settings: { font_size: number; read_width: number; allow_remote: boolean }
}>()

const detail = ref<NoteDetail | null>(null)
const error = ref('')
const showAttachment = ref<AttachmentItem | null>(null)

watch(
  () => props.guid,
  (g) => {
    detail.value = null
    showAttachment.value = null
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

async function openOriginal() {
  if (detail.value?.url) {
    await api.openExternalUrl(detail.value.url)
  }
}

// 单篇导出（FR-08）：zip / 自包含 HTML
async function exportZip() {
  if (!detail.value) return
  const { save } = await import('@tauri-apps/plugin-dialog')
  const dest = await save({ defaultPath: `${detail.value.title}.zip` })
  if (!dest) return
  try {
    await api.exportNoteZip(detail.value.guid, dest)
    alert('导出完成（含物化代码块与附件）')
  } catch (e) {
    alert(String(e))
  }
}
async function exportHtml() {
  if (!detail.value) return
  const { save } = await import('@tauri-apps/plugin-dialog')
  const dest = await save({ defaultPath: `${detail.value.title}.html` })
  if (!dest) return
  try {
    await api.exportNoteHtml(detail.value.guid, dest)
    alert('导出完成（自包含 HTML）')
  } catch (e) {
    alert(String(e))
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
          <a @click="jumpFolder" :title="detail.location">📁 {{ detail.location }}</a>
          <span>创建 {{ detail.created }}</span>
          <span>修改 {{ detail.data_modified }}</span>
          <span>{{ formatSize(detail.package_size) }}</span>
          <a v-if="detail.url" @click="openOriginal">查看原文 ↗</a>
          <span class="grow"></span>
          <a @click="exportZip">导出 zip</a>
          <a @click="exportHtml">导出 HTML</a>
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
            :key="props.guid || ''"
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
