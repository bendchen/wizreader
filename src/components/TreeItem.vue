<script setup lang="ts">
import { computed } from 'vue'
import type { TreeNode } from '../api'

const props = defineProps<{
  node: TreeNode
  selected: string
  collapsed: Set<string>
}>()
const emit = defineEmits<{ (e: 'select', path: string): void; (e: 'toggle', path: string): void }>()

const isCollapsed = computed(() => props.collapsed.has(props.node.path))
</script>

<template>
  <div>
    <div
      class="tree-row"
      :class="{ active: selected === node.path, system: node.is_system }"
      :title="node.path"
      @click="emit('select', node.path)"
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
        @select="(p) => emit('select', p)"
        @toggle="(p) => emit('toggle', p)"
      />
    </div>
  </div>
</template>
