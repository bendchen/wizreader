<script setup lang="ts">
/**
 * 笔记列表（FR-04）。S2 起承担三个写操作入口（T6/T8）：
 * - **右键菜单**：打开 / 编辑正文 / 重命名 / 移动到… / 删除到回收站 / 在访达中显示；
 * - **拖拽**：把某篇拖到左侧目录树节点上完成移动（HTML5 DnD，dataTransfer 传 guid）；
 * - 菜单项在**非库上下文**（为知视图 / 未打开）一律禁用 —— 那两种上下文是只读的。
 */
import { ref, watch } from 'vue'
import { api, formatSize, type NoteItem } from '../api'

const props = defineProps<{
  folder: string
  /** 是否库上下文（决定写操作入口是否可用） */
  writable: boolean
  /** 外部变更（保存/改名/移动/删除后）触发的重载计数 */
  version: number
}>()
const emit = defineEmits<{
  (e: 'open', guid: string, item: NoteItem): void
  (e: 'edit', item: NoteItem): void
  (e: 'rename', item: NoteItem): void
  (e: 'move', item: NoteItem): void
  (e: 'delete', item: NoteItem): void
  (e: 'reveal', item: NoteItem): void
  (e: 'dragstart', item: NoteItem): void
  (e: 'dragend'): void
}>()

const notes = ref<NoteItem[]>([])
const sort = ref<string>('modified')
const filterText = ref('')
const filterTimer = ref<number | null>(null)
const loading = ref(true)

/** 右键菜单：位置 + 目标笔记 */
const menu = ref<{ x: number; y: number; item: NoteItem } | null>(null)

watch([() => props.folder, sort], load, { immediate: true })
watch(() => props.version, load)

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

function openMenu(e: MouseEvent, item: NoteItem) {
  e.preventDefault()
  // 菜单贴边收敛：避免在窗口右下角被裁掉
  menu.value = {
    x: Math.min(e.clientX, window.innerWidth - 200),
    y: Math.min(e.clientY, window.innerHeight - 210),
    item,
  }
}
function closeMenu() {
  menu.value = null
}

type CtxAction = 'open' | 'edit' | 'rename' | 'move' | 'delete' | 'reveal'
/** 菜单动作分发：先关菜单再派事件（避免弹窗被菜单挡住） */
function run(kind: CtxAction) {
  const it = menu.value?.item
  closeMenu()
  if (!it) return
  switch (kind) {
    case 'open':
      emit('open', it.guid, it)
      break
    case 'edit':
      emit('edit', it)
      break
    case 'rename':
      emit('rename', it)
      break
    case 'move':
      emit('move', it)
      break
    case 'delete':
      emit('delete', it)
      break
    case 'reveal':
      emit('reveal', it)
      break
  }
}

function onDragStart(e: DragEvent, item: NoteItem) {
  if (!props.writable) {
    e.preventDefault()
    return
  }
  e.dataTransfer?.setData('text/plain', item.guid)
  e.dataTransfer?.setData('application/x-wizreader-guid', item.guid)
  if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move'
  emit('dragstart', item)
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
    <div class="list-scroll" @click="closeMenu">
      <div v-if="loading" class="empty-hint">加载中…</div>
      <div v-else-if="!notes.length" class="empty-hint">此目录暂无笔记</div>
      <div
        v-for="n in notes"
        :key="n.guid"
        class="note-item"
        :class="{ draggable: writable }"
        :draggable="writable"
        @click="emit('open', n.guid, n)"
        @contextmenu="openMenu($event, n)"
        @dragstart="onDragStart($event, n)"
        @dragend="emit('dragend')"
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

    <template v-if="menu">
      <div class="ctx-backdrop" @click="closeMenu" @contextmenu.prevent="closeMenu"></div>
      <div class="ctx-menu" :style="{ left: menu.x + 'px', top: menu.y + 'px' }">
        <a @click="run('open')">打开</a>
        <a :class="{ disabled: !writable }" @click="writable && run('edit')">编辑正文…</a>
        <div class="ctx-sep"></div>
        <a :class="{ disabled: !writable }" @click="writable && run('rename')">重命名…</a>
        <a :class="{ disabled: !writable }" @click="writable && run('move')">移动到…</a>
        <a :class="{ disabled: !writable }" class="danger" @click="writable && run('delete')"
          >删除（移入回收站）</a
        >
        <div class="ctx-sep"></div>
        <a :class="{ disabled: !writable }" @click="writable && run('reveal')">在访达中显示</a>
        <div v-if="!writable" class="ctx-note">为知笔记视图 / 未打开笔记库 —— 只读</div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.note-item.draggable {
  cursor: grab;
}
.note-item.draggable:active {
  cursor: grabbing;
}
.ctx-backdrop {
  position: fixed;
  inset: 0;
  z-index: 50;
}
.ctx-menu {
  position: fixed;
  z-index: 51;
  min-width: 180px;
  padding: 6px;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 8px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.18);
  display: flex;
  flex-direction: column;
}
.ctx-menu a {
  padding: 6px 10px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 13px;
  color: var(--text);
  white-space: nowrap;
}
.ctx-menu a:hover {
  background: var(--bg-2);
}
.ctx-menu a.danger {
  color: var(--danger, #c0392b);
}
.ctx-menu a.disabled {
  color: var(--text-2);
  opacity: 0.5;
  cursor: not-allowed;
}
.ctx-menu a.disabled:hover {
  background: transparent;
}
.ctx-sep {
  height: 1px;
  background: var(--border);
  margin: 4px 2px;
}
.ctx-note {
  padding: 4px 10px;
  font-size: 11px;
  color: var(--text-2);
}
</style>
