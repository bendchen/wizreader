<script setup lang="ts">
import { ref, watch } from 'vue'
import { api, formatSize, type SearchResponse } from '../api'

const props = defineProps<{ kw: string; folder: string }>()
const emit = defineEmits<{ (e: 'open', guid: string): void }>()

const resp = ref<SearchResponse | null>(null)
const expanded = ref<Set<string>>(new Set())
const loading = ref(true)

watch(
  () => [props.kw, props.folder],
  () => load(),
  { immediate: true }
)

async function load() {
  if (!props.kw) {
    resp.value = null
    loading.value = false
    return
  }
  loading.value = true
  expanded.value = new Set()
  try {
    resp.value = await api.search(props.kw, props.folder || null)
  } finally {
    loading.value = false
  }
}

function toggleDup(fp: string) {
  const s = new Set(expanded.value)
  if (s.has(fp)) s.delete(fp)
  else s.add(fp)
  expanded.value = s
}
</script>

<template>
  <div class="panel" style="flex: 1">
    <div class="panel-head">
      <template v-if="resp">
        检索 "{{ resp.kw }}"：{{ resp.notes.length }} 篇笔记 / {{ resp.attachments.length }} 个附件 ·
        {{ resp.elapsed_ms }} ms
      </template>
      <template v-else-if="loading">检索中…</template>
    </div>
    <div class="list-scroll search-results">
      <div v-if="loading" class="empty-hint">检索中…</div>
      <div v-else-if="!resp" class="empty-hint">输入关键词开始检索</div>
      <div v-else-if="!resp.notes.length && !resp.attachments.length" class="empty-hint">
        无结果
      </div>
      <template v-for="r in resp?.notes ?? []" :key="r.guid">
        <div class="result-item" @click="emit('open', r.guid)">
          <div class="result-title">
            <span v-html="r.title_hl || r.title"></span>
          </div>
          <div class="result-meta">
            {{ r.location }} · {{ r.data_modified }}
            <span v-if="r.dup_count > 1" class="dup-note" @click.stop="toggleDup(r.fingerprint)">
              另有 {{ r.dup_count - 1 }} 篇相同内容 ▾
            </span>
          </div>
          <div v-if="r.snippet" class="result-snippet" v-html="r.snippet"></div>
        </div>
        <!-- P8 指纹去重：展开显示同组（组内其余条目未参与检索排序，此处仅提示）
             完整展开需按指纹再查，此处点击直接打开代表篇 -->
      </template>
      <!-- 附件名命中 -->
      <div v-for="a in resp?.attachments ?? []" :key="a.file_path" class="result-item">
        <div class="result-title">📎 {{ a.display_name }}</div>
        <div class="result-meta">
          <template v-if="a.tier === 0">文件未随导出下载</template>
          <template v-else-if="!a.document_guid">未关联附件（FR-10 视图中可直接打开）</template>
          <template v-else>tier {{ a.tier }} · {{ formatSize(a.size) }}</template>
        </div>
      </div>
    </div>
  </div>
</template>
