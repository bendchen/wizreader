<script setup lang="ts">
import { ref } from 'vue'
import { api, formatSize, type AttachmentItem, type AttachmentPreview } from '../api'

const props = defineProps<{ attachment: AttachmentItem }>()
const emit = defineEmits<{ (e: 'close'): void }>()

const preview = ref<AttachmentPreview | null>(null)
const error = ref('')
const loaded = ref(false)

load()

async function load() {
  error.value = ''
  try {
    preview.value = await api.previewAttachment(props.attachment.file_path)
  } catch (e) {
    error.value = String(e)
  } finally {
    loaded.value = true
  }
}

async function openExternal() {
  try {
    await api.openAttachmentExternal(props.attachment.file_path)
  } catch (e) {
    error.value = String(e)
  }
}
async function reveal() {
  try {
    await api.revealInFinder(props.attachment.file_path)
  } catch (e) {
    error.value = String(e)
  }
}
async function saveAs() {
  const { save } = await import('@tauri-apps/plugin-dialog')
  const dest = await save({ defaultPath: props.attachment.display_name })
  if (!dest) return
  try {
    await api.saveAttachmentAs(props.attachment.file_path, dest)
  } catch (e) {
    error.value = String(e)
  }
}
async function copyAll() {
  if (preview.value?.content) {
    await navigator.clipboard.writeText(preview.value.content)
  }
}
</script>

<template>
  <div class="modal-mask" @click.self="emit('close')">
    <div class="modal">
      <div class="modal-head">
        <span class="title">📎 {{ attachment.display_name }}</span>
        <span class="attach-origin">{{ attachment.origin }}</span>
        <span style="color: var(--text-2); font-size: 12px">{{
          formatSize(attachment.size)
        }}</span>
        <span class="grow"></span>
        <button v-if="preview?.is_text" @click="copyAll">复制全文</button>
        <button v-if="attachment.exists" @click="saveAs">另存为</button>
        <button v-if="attachment.exists" @click="openExternal">系统程序打开</button>
        <button v-if="attachment.exists" @click="reveal">Finder 中显示</button>
        <button @click="emit('close')">关闭</button>
      </div>
      <div class="modal-body">
        <div v-if="error" class="empty-hint">{{ error }}</div>
        <template v-else-if="loaded && preview">
          <pre v-if="preview.is_text">{{ preview.content }}</pre>
          <div v-else class="empty-hint">此类型不支持应用内预览，请用系统程序打开</div>
        </template>
        <div v-else class="empty-hint">加载中…</div>
      </div>
    </div>
  </div>
</template>
