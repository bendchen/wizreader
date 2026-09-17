<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { api, formatSize, safeFileName, type Settings, type BuildReport, type VerifyReport, type VerifyCheck } from '../api'
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
const verifying = ref(false)
const verifyProgress = ref('')
const verifyReport = ref<VerifyReport | null>(null)
const withBench = ref(true)

onMounted(async () => {
  settings.value = await api.getSettings()
  if (await api.checkIndex()) {
    /* 索引在位 */
  }
})

// 索引 / 巡检进度事件
const { listen } = await import('@tauri-apps/api/event')
listen<{ done: number; total: number }>('index-progress', (e) => {
  progress.value = `索引进度: ${e.payload.done}/${e.payload.total} 篇`
})
listen<{ stage: string; done: number; total: number }>('verify-progress', (e) => {
  verifyProgress.value = `${e.payload.stage}: ${e.payload.done}/${e.payload.total}`
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

/** T4.1–T4.4：在应用内跑全库巡检（源数据仍为只读） */
async function runVerify() {
  verifying.value = true
  verifyReport.value = null
  verifyProgress.value = ''
  try {
    verifyReport.value = await api.runVerify(withBench.value, false)
  } catch (e) {
    alert(String(e))
  } finally {
    verifying.value = false
    verifyProgress.value = ''
  }
}

async function saveVerifyReport() {
  if (!verifyReport.value) return
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({ directory: true })
  if (typeof dir !== 'string') return
  try {
    const paths = await api.saveVerifyReport(`${dir}/${safeFileName('M4 验收报告')}.md`, verifyReport.value)
    alert('报告已写入：\n' + paths.join('\n'))
  } catch (e) {
    alert(String(e))
  }
}

const VERIFY_GROUPS: [string, keyof VerifyReport][] = [
  ['T4.1 全库巡检（10 项）', 'inspection'],
  ['T4.0 导出产物自检', 'export_check'],
  ['T4.2 源数据零写入', 'zero_write'],
  ['T4.3 安全项', 'security'],
  ['T4.4 性能基准', 'bench'],
]

function groupOf(r: VerifyReport, k: keyof VerifyReport): VerifyCheck[] {
  return (r[k] as VerifyCheck[]) ?? []
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
      <label>验收巡检</label>
      <button :disabled="verifying || !settings?.data_dir" @click="runVerify">
        {{ verifying ? '巡检中…' : '运行全库巡检' }}
      </button>
      <label class="inline-check">
        <input v-model="withBench" type="checkbox" /> 含性能基准
      </label>
      <button v-if="verifyReport" @click="saveVerifyReport">保存报告…</button>
      <span class="progress-text">{{ verifyProgress }}</span>
    </div>
    <div v-if="verifyReport" class="report-box">
      <div :class="verifyReport.ok ? 'verify-ok' : 'verify-bad'">
        {{ verifyReport.ok ? '✅ 全部通过' : '⚠️ 存在未通过项' }}・{{ verifyReport.elapsed_ms }} ms・{{ verifyReport.data_dir }}
      </div>
      <div v-for="[title, key] in VERIFY_GROUPS" :key="key" class="verify-group">
        <div class="verify-title">{{ title }}</div>
        <div v-for="c in groupOf(verifyReport, key)" :key="c.id" class="verify-item">
          <span class="verify-mark">{{ c.skipped ? '－' : c.passed ? '✅' : '❌' }}</span>
          <span><b>[{{ c.id }}]</b> {{ c.name }}</span>
          <div class="verify-actual">实测：{{ c.actual }}</div>
          <div class="verify-actual">期望：{{ c.expected }}</div>
          <div v-for="s in c.samples" :key="s" class="verify-sample">· {{ s }}</div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.inline-check {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 12px;
  color: var(--text-2);
}
.verify-ok {
  color: #2e7d32;
  font-weight: 600;
}
.verify-bad {
  color: var(--danger);
  font-weight: 600;
}
.verify-group {
  margin-top: 10px;
}
.verify-title {
  font-weight: 600;
  margin-bottom: 4px;
}
.verify-item {
  display: grid;
  grid-template-columns: 20px 1fr;
  column-gap: 6px;
  font-size: 12px;
  margin-bottom: 6px;
}
.verify-mark {
  grid-row: span 2;
}
.verify-actual,
.verify-sample {
  grid-column: 2;
  color: var(--text-2);
}
.verify-sample {
  color: var(--danger);
}
</style>
