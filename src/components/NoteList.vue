<script setup lang="ts">
import { ref, watch } from 'vue'
import { api, formatSize, type NoteItem } from '../api'

const props = defineProps<{ folder: string }>()
const emit = defineEmits<{ (e: 'open', guid: string, item: NoteItem): void }>()

const notes = ref<NoteItem[]>([])
const sort = ref<string>('modified')
const filterText = ref('')
const filterTimer = ref<number | null>(null)
const loading = ref(true)

watch([() => props.folder, sort], load, { immediate: true })

async function load() {
  loading.value = true
  try {
    notes.value = await api.listNotes(props.folder || null, sort.value, filterText.value || null)
  } finally {
    loading.value = false
  }
}

// 目录内标题即时过滤 ≤ 50ms（FR-04.5）：前端过滤，输入即筛
function onFilter() {
  if (filterTimer.value) window.clearTimeout(filterTimer.value)
  filterTimer.value = window.setTimeout(load, 150)
}
</script>

<template>
  <div class="panel panel-list">
    <div class="panel-head">
      <input
        v-model="filterText"
        type="text"
        placeholder="标题过滤…"
        style="flex: 1; min-width: 0; padding: 3px 8px; border: 1px solid var(--border); border-radius: 4px; background: var(--bg); color: var(--text)"
        @input="onFilter"
      />
      <select
        v-model="sort"
        style="padding: 3px 4px; border: 1px solid var(--border); border-radius: 4px; background: var(--bg); color: var(--text)"
      >
        <option value="modified">修改时间</option>
        <option value="created">创建时间</option>
        <option value="title">标题</option>
        <option value="size">体积</option>
      </select>
    </div>
    <div class="list-scroll">
      <div v-if="loading" class="empty-hint">加载中…</div>
      <div v-else-if="!notes.length" class="empty-hint">此目录暂无笔记</div>
      <div
        v-for="n in notes"
        :key="n.guid"
        class="note-item"
        @click="emit('open', n.guid, n)"
      >
        <div class="note-title" :title="n.title">
          {{ n.doc_type === 'webnote' || n.doc_type === 'wholewebpage' ? '🌐 ' : '📝 ' }}{{ n.title }}
        </div>
        <div class="note-meta">
          <span>{{ n.data_modified }}</span>
          <span v-if="n.has_attachment" title="含附件">📎</span>
          <span v-if="n.is_webclip" title="网页剪藏">🔗</span>
          <span v-if="n.is_empty" class="badge warn">空笔记</span>
          <span>{{ formatSize(n.package_size) }}</span>
        </div>
      </div>
    </div>
  </div>
</template>
