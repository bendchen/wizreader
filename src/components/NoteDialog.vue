<script setup lang="ts">
/**
 * 写操作的小弹窗集合（T8）：重命名输入 / 危险操作确认 / 移动目标目录选择。
 *
 * 为什么不用 `window.prompt`：WKWebView **不实现** `prompt()`（直接返回 null），
 * 在 macOS 上等于功能不可用；`confirm()` 可用但没法带输入框。
 * 故统一走本组件，样式与项目其余部分一致（沿用 `--bg/--border/--text` 变量）。
 */
import { ref, computed, watch, nextTick } from 'vue'

const props = defineProps<{
  kind: 'prompt' | 'confirm' | 'folder' | 'new'
  title: string
  message: string
  /** prompt/new：初值；confirm：确认按钮文案 */
  initial?: string
  confirmText?: string
  danger?: boolean
  /** folder/new：可选目录（拍平后的树节点） */
  folders?: { path: string; name: string; depth: number }[]
  current?: string
}>()
const emit = defineEmits<{
  /** 第二参数仅 `new`/`folder` 有意义：选中的目录路径（其余调用方可忽略） */
  (e: 'confirm', value: string, folder: string): void
  (e: 'cancel'): void
}>()

const value = ref(props.initial ?? '')
const picked = ref(props.current ?? '')
const input = ref<HTMLInputElement | null>(null)

watch(
  () => props.title,
  async () => {
    value.value = props.initial ?? ''
    picked.value = props.current ?? ''
    await nextTick()
    input.value?.focus()
    input.value?.select()
  },
  { immediate: true }
)

const canConfirm = computed(() => {
  if (props.kind === 'prompt') return value.value.trim().length > 0
  if (props.kind === 'folder') return picked.value !== '' && picked.value !== props.current
  if (props.kind === 'new') return value.value.trim().length > 0 && picked.value !== ''
  return true
})

function ok() {
  if (!canConfirm.value) return
  const folder = props.kind === 'folder' || props.kind === 'new' ? picked.value : ''
  const value0 = props.kind === 'prompt' || props.kind === 'new' ? value.value.trim() : picked.value
  emit('confirm', value0, folder)
}
</script>

<template>
  <div class="dlg-backdrop" @click.self="emit('cancel')">
    <div class="dlg" @keydown.enter="ok">
      <div class="dlg-title">{{ title }}</div>
      <div class="dlg-msg">{{ message }}</div>

      <input
        v-if="kind === 'prompt' || kind === 'new'"
        ref="input"
        v-model="value"
        type="text"
        class="dlg-input"
        placeholder="笔记标题"
      />

      <div v-if="kind === 'folder' || kind === 'new'" class="dlg-folders">
        <label
          v-for="f in folders"
          :key="f.path"
          class="dlg-folder"
          :class="{ active: picked === f.path }"
          :style="{ paddingLeft: 8 + f.depth * 14 + 'px' }"
        >
          <input type="radio" :value="f.path" v-model="picked" />
          <span class="dlg-folder-name">{{ f.name }}</span>
          <span class="dlg-folder-path">{{ f.path }}</span>
        </label>
        <div v-if="!folders?.length" class="empty-hint">没有可选目录</div>
      </div>

      <div class="dlg-actions">
        <button @click="emit('cancel')">取消</button>
        <button class="primary" :class="{ danger }" :disabled="!canConfirm" @click="ok">
          {{ confirmText ?? '确定' }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.dlg-backdrop {
  position: fixed;
  inset: 0;
  z-index: 70;
  background: rgba(0, 0, 0, 0.35);
  display: flex;
  align-items: center;
  justify-content: center;
}
.dlg {
  width: 460px;
  max-width: 92vw;
  max-height: 80vh;
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 16px;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 10px;
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.25);
}
.dlg-title {
  font-size: 15px;
  font-weight: 600;
}
.dlg-msg {
  font-size: 13px;
  color: var(--text-2);
  line-height: 1.5;
  word-break: break-all;
  white-space: pre-wrap;
}
.dlg-input {
  padding: 8px 10px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--bg);
  color: var(--text);
  font-size: 13px;
}
.dlg-folders {
  flex: 1;
  min-height: 0;
  overflow: auto;
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 4px;
}
.dlg-folder {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 8px;
  border-radius: 4px;
  cursor: pointer;
  font-size: 12.5px;
}
.dlg-folder:hover {
  background: var(--bg-2);
}
.dlg-folder.active {
  background: var(--bg-3);
}
.dlg-folder-name {
  white-space: nowrap;
}
.dlg-folder-path {
  color: var(--text-2);
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.dlg-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.dlg-actions button {
  padding: 6px 16px;
  border-radius: 6px;
}
.dlg-actions .primary {
  background: #1677ff;
  border-color: #1677ff;
  color: #fff;
  font-weight: 600;
}
.dlg-actions .primary.danger {
  background: var(--danger, #c0392b);
  border-color: var(--danger, #c0392b);
}
.dlg-actions .primary:disabled {
  opacity: 0.5;
}
</style>
