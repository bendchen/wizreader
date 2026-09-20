<script setup lang="ts">
/**
 * 目录树节点。S2 起同时是**移动笔记的投放目标**（T6）：
 * 拖动笔记列表项到本节点 → 高亮 → 松手 → `emit('drop-note', path)`。
 * 系统目录（`_trash/` 等保留区）不接受投放：往里移笔记等于把笔记变孤儿。
 */
import { computed, ref } from 'vue'
import type { TreeNode } from '../api'

const props = defineProps<{
  node: TreeNode
  selected: string
  collapsed: Set<string>
  /** 是否接受拖放（库上下文才接受；为知视图只读） */
  droppable: boolean
}>()
const emit = defineEmits<{
  (e: 'select', path: string): void
  (e: 'toggle', path: string): void
  (e: 'drop-note', path: string): void
}>()

const isCollapsed = computed(() => props.collapsed.has(props.node.path))
const over = ref(false)
const canDrop = computed(() => props.droppable && !props.node.is_system)

function onDragOver(e: DragEvent) {
  if (!canDrop.value) return
  e.preventDefault()
  if (e.dataTransfer) e.dataTransfer.dropEffect = 'move'
  over.value = true
}
function onDragLeave() {
  over.value = false
}
function onDrop(e: DragEvent) {
  if (!canDrop.value) return
  e.preventDefault()
  over.value = false
  emit('drop-note', props.node.path)
}
</script>

<template>
  <div>
    <div
      class="tree-row"
      :class="{ active: selected === node.path, system: node.is_system, 'drop-over': over }"
      :title="node.path"
      @click="emit('select', node.path)"
      @dragover="onDragOver"
      @dragleave="onDragLeave"
      @drop="onDrop"
    >
      <span class="tree-toggle" @click.stop="emit('toggle', node.path)">
        {{ node.children.length ? (isCollapsed ? '▸' : '▾') : '' }}
      </span>
      <span>{{ node.is_system ? '' : node.has_attachment ? '📎 ' : '' }}{{ node.name }}</span>
      <span class="cnt">{{ node.total_count }}</span>
    </div>
    <div v-if="!isCollapsed && node.children.length" class="tree-children">
      <TreeItem
        v-for="c in node.children"
        :key="c.path"
        :node="c"
        :selected="selected"
        :collapsed="collapsed"
        :droppable="droppable"
        @select="(p) => emit('select', p)"
        @toggle="(p) => emit('toggle', p)"
        @drop-note="(p) => emit('drop-note', p)"
      />
    </div>
  </div>
</template>

<style scoped>
.tree-row.drop-over {
  background: rgba(22, 119, 255, 0.18);
  outline: 1px dashed #1677ff;
  outline-offset: -1px;
}
</style>

