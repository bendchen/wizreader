<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { api, formatSize, type Settings, type BuildReport } from '../api'
import type { AttachmentItem } from '../api'
import AttachmentModal from './AttachmentModal.vue'

const previewAtt = ref<AttachmentItem | null>(null)

const settings = ref<Settings | null>(null)
const building = ref(false)
const progress = ref('')
const report = ref<BuildReport | null>(null)
const unlinked = ref<AttachmentItem[]>([])
const showUnlinked = ref(false)
const confirmRebuild = ref(false)
const exporting = ref(false)

onMounted(async () => {
  settings.value = await api.getSettings()
  if (await api.checkIndex()) {
    /* 索引在位 */
  }
})

// 索引进度事件
const { listen } = await import('@tauri-apps/api/event')
listen<{ done: number; total: number }>('index-progress', (e) => {
  progress.value = `索引进度: ${e.payload.done}/${e.payload.total} 篇`
})

async function chooseDataDir() {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({ directory: true })
  if (typeof dir !== 'string') return
  try {
    await api.pickDefaultDataDir(dir)
    settings.value = await api.getSettings()
    await rebuild(true)
  } catch (e) {
    alert(String(e))
  }
}

async function rebuild(skipConfirm = false) {
  if (!skipConfirm && !confirmRebuild.value) {
    confirmRebuild.value = true
    return
  }
  confirmRebuild.value = false
  building.value = true
  report.value = null
  progress.value = ''
  try {
    report.value = await api.buildIndex()
  } catch (e) {
    alert(String(e))
  } finally {
    building.value = false
    progress.value = ''
  }
}

async function toggleUnlinked() {
  showUnlinked.value = !showUnlinked.value
  if (showUnlinked.value) {
    unlinked.value = await api.getUnlinked()
  }
}

async function exportFolder(location: string) {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({ directory: true })
  if (typeof dir !== 'string') return
  exporting.value = true
  try {
    const r = await api.exportFolder(location, dir)
    alert(
      `导出完成：${r.notes_exported} 篇 / ${r.attachments_exported} 个附件，耗时 ${r.elapsed_ms} ms` +
        (r.skipped.length ? `\n跳过 ${r.skipped.length} 项` : '')
    )
  } catch (e) {
    alert(String(e))
  } finally {
    exporting.value = false
  }
}

async function update(patch: Partial<Settings>) {
  if (!settings.value) return
  settings.value = { ...settings.value, ...patch }
  await api.updateSettings(settings.value)
}
</script>

<template>
  <div class="settings-page">
    <h2>设置</h2>

    <div class="settings-row">
      <label>数据源目录</label>
      <span>{{ settings?.data_dir || '（未设置）' }}</span>
      <button @click="chooseDataDir">选择…</button>
    </div>

    <div class="settings-row">
      <label>索引目录</label>
      <span>~/.wizreader/</span>
    </div>

    <div class="settings-row">
      <label>字号</label>
      <select :value="settings?.font_size" @change="update({ font_size: Number(($event.target as HTMLSelectElement).value) })">
        <option :value="14">14px</option>
        <option :value="16">16px</option>
        <option :value="18">18px</option>
        <option :value="20">20px</option>
      </select>
    </div>

    <div class="settings-row">
      <label>主题</label>
      <select :value="settings?.theme" @change="update({ theme: ($event.target as HTMLSelectElement).value })">
        <option value="system">跟随系统</option>
        <option value="light">浅色</option>
        <option value="dark">深色</option>
      </select>
    </div>

    <div class="settings-row">
      <label>阅读区最大宽度</label>
      <input
        type="number"
        :value="settings?.read_width"
        min="600"
        max="1400"
        step="20"
        @change="update({ read_width: Number(($event.target as HTMLInputElement).value) })"
      />
      <span>px</span>
    </div>

    <div class="settings-row">
      <label>允许加载远程图片</label>
      <input
        type="checkbox"
        :checked="settings?.allow_remote"
        @change="update({ allow_remote: ($event.target as HTMLInputElement).checked })"
      />
      <span style="color: var(--text-2); font-size: 12px">默认关：加载远程图片会向第三方泄漏阅读行为</span>
    </div>

    <div class="settings-row">
      <label>MinIO / 自动同步</label>
      <span style="color: var(--text-2); font-size: 12px">云同步功能暂缓（后续版本提供）</span>
    </div>

    <div class="settings-row">
      <label>重建索引</label>
      <button :disabled="building || !settings?.data_dir" @click="rebuild()">
        {{ building ? '构建中…' : confirmRebuild ? '确认重建？（再次点击）' : '重建索引' }}
      </button>
      <span class="progress-text">{{ progress }}</span>
    </div>
    <div v-if="report" class="report-box">
      校验：{{ report.ok ? '✅ 全部通过' : '⚠️ 存在偏离项' }}
      笔记数：{{ report.note_count }}（源库 {{ report.source_note_count }} / 包 {{ report.package_count }}）
      附件 Tier：T1={{ report.tier1 }} T2={{ report.tier2 }} T3={{ report.tier3 }} T4={{ report.tier4 }} DB缺失={{ report.db_missing }}
      耗时：{{ report.elapsed_ms }} ms
      <template v-if="report.warnings.length">
        {{ '\n' }}警告：
        <div v-for="w in report.warnings" :key="w" style="color: var(--danger)">- {{ w }}</div>
      </template>
    </div>

    <div class="settings-row" style="margin-top: 20px">
      <label>未关联附件</label>
      <button @click="toggleUnlinked">{{ showUnlinked ? '收起' : '查看' }}</button>
    </div>
    <div v-if="showUnlinked" class="report-box">
      <div v-for="a in unlinked" :key="a.file_path" class="attach-chip" style="margin: 2px" @click="previewAtt = a">
        📎 {{ a.display_name }}（{{ formatSize(a.size) }}）
      </div>
      <div style="color: var(--text-2); margin-top: 6px">
        点击可预览；程序不做自动归属（严禁自动绑定）。
      </div>
    </div>
    <AttachmentModal v-if="previewAtt" :attachment="previewAtt" @close="previewAtt = null" />

    <div class="settings-row" style="margin-top: 20px">
      <label>导出（逃生舱）</label>
      <button :disabled="exporting" @click="exportFolder('')">导出全库…</button>
      <span style="color: var(--text-2); font-size: 12px">
        按目录还原文件树，含物化代码块与附件；单篇导出在阅读区右上角菜单
      </span>
    </div>
    <div v-if="exporting" class="progress-text">导出中…（全库约需数分钟）</div>
  </div>
</template>
