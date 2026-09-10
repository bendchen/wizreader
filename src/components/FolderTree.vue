<script setup lang="ts">
import { ref, onMounted, watch } from 'vue'
import { api, type TreeNode } from '../api'
import TreeItem from './TreeItem.vue'

const props = defineProps<{ selected: string }>()
const emit = defineEmits<{ (e: 'select', path: string): void }>()

const tree = ref<TreeNode[]>([])
const collapsed = ref<Set<string>>(new Set())
const loading = ref(true)

onMounted(load)

async function load() {
  loading.value = true
  try {
    tree.value = await api.getTree()
  } finally {
    loading.value = false
  }
}

function toggle(path: string) {
  const s = new Set(collapsed.value)
  if (s.has(path)) s.delete(path)
  else s.add(path)
  collapsed.value = s
}

defineExpose({ load })

// 折叠状态持久化（FR-03.1）
const KEY = 'wizreader.folder_state'
watch(collapsed, (s) => localStorage.setItem(KEY, JSON.stringify([...s])), { deep: true })
onMounted(() => {
  try {
    const saved = localStorage.getItem(KEY)
    if (saved) collapsed.value = new Set(JSON.parse(saved))
  } catch {
    /* ignore */
  }
})
</script>

<template>
  <div class="panel panel-tree">
    <div class="panel-head">目录</div>
    <div class="tree-scroll tree-node">
      <div v-if="loading" class="empty-hint">加载中…</div>
      <template v-else>
        <!-- 顶层"全部笔记"虚拟节点（FR-03.5） -->
        <div
          class="tree-row"
          :class="{ active: props.selected === '' }"
          @click="emit('select', '')"
        >
          <span class="tree-toggle"></span>
          <span>📚 全部笔记</span>
          <span class="cnt">{{
            tree.reduce((a, n) => a + (n.path !== '/Deleted Items/' ? n.total_count : 0), 0)
          }}</span>
        </div>
        <!-- 系统目录分组（P14） -->
        <div v-if="tree.some((n) => n.is_system)" class="tree-group-label">系统目录</div>
        <TreeItem
          v-for="node in tree.filter((n) => n.is_system)"
          :key="node.path"
          :node="node"
          :selected="props.selected"
          :collapsed="collapsed"
          @select="(p) => emit('select', p)"
          @toggle="toggle"
        />
        <div class="tree-group-label">我的目录</div>
        <TreeItem
          v-for="node in tree.filter((n) => !n.is_system)"
          :key="node.path"
          :node="node"
          :selected="props.selected"
          :collapsed="collapsed"
          @select="(p) => emit('select', p)"
          @toggle="toggle"
        />
      </template>
    </div>
  </div>
</template>
