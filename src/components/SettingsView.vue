<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { api, formatSize, safeFileName, writeErrorText, type Settings, type BuildReport, type VerifyReport, type VerifyCheck, type ViewContext } from '../api'
import type {
  AttachmentItem, SyncConfigView, SyncSettings as SyncCfg,
  SyncStatusView, SyncReport, TestConnectionResult, TrashItem, TrashStats,
} from '../api'
import AttachmentModal from './AttachmentModal.vue'

const previewAtt = ref<AttachmentItem | null>(null)

const settings = ref<Settings | null>(null)
const viewContext = ref<ViewContext>('none')
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

// 上下文三态文案（none = 未打开任何笔记）
const ctxText = computed(() =>
  viewContext.value === 'library'
    ? '笔记库'
    : viewContext.value === 'source'
      ? '为知源（只读）'
      : '未打开任何笔记'
)
// 当前上下文是否有可重建的索引（none 时无可重建对象）
const canRebuild = computed(() =>
  viewContext.value === 'library'
    ? !!settings.value?.library_dir
    : viewContext.value === 'source'
      ? !!settings.value?.source_dir
      : false
)

/** U4：本机是否只读端（reader）—— 写入口与「上行」都要按它禁用（R7） */
const isReader = computed(() => form.value.role === 'reader')
/** 同上，但取**已保存**的角色：回收站恢复等写操作的判据必须是后端真正生效的那个值 */
const savedIsReader = computed(() => syncCfg.value?.config.role === 'reader')

onMounted(async () => {
  settings.value = await api.getSettings()
  if (await api.checkIndex()) {
    /* 索引在位 */
  }
  try {
    const st = await api.getLibraryStatus()
    viewContext.value = st.context
  } catch {
    /* 忽略：默认库上下文 */
  }
  await loadSync()
  await loadTrashStats()
})

// ---------- 云同步（FR-07 阶段二） ----------
const syncCfg = ref<SyncConfigView | null>(null)
const syncStatus = ref<SyncStatusView | null>(null)
const form = ref<SyncCfg>(emptySyncForm())
const secretKey = ref('')
const syncBusy = ref('') // '' | 'test…' | 'init…' | 'up' | 'down'
const syncMsg = ref('')
const syncProgress = ref('')
const testResult = ref<TestConnectionResult | null>(null)

// ---------- 回收站（笔记库；T7 收口：根 = 库根，与云同步无关） ----------
const showTrash = ref(false)
const trashItems = ref<TrashItem[]>([])
const trashStats = ref<TrashStats | null>(null)
const trashBusy = ref(false)
const trashMsg = ref('')

async function loadTrashStats() {
  try {
    trashStats.value = await api.getTrashStats()
  } catch (e) {
    // 未设置库时不报错：回收站区整块隐藏
    trashStats.value = null
    trashMsg.value = writeErrorText(e)
  }
}

async function toggleTrash() {
  showTrash.value = !showTrash.value
  if (!showTrash.value) return
  trashBusy.value = true
  trashMsg.value = ''
  try {
    trashItems.value = await api.listTrash()
    await loadTrashStats()
  } catch (e) {
    trashMsg.value = writeErrorText(e)
  } finally {
    trashBusy.value = false
  }
}

/** 从回收站恢复：文件回原路径 + 清单行逐字段还原（写操作，错误码照实展示） */
async function restoreNote(t: TrashItem) {
  trashBusy.value = true
  trashMsg.value = ''
  try {
    const rep = await api.restoreNote(t.guid)
    trashItems.value = await api.listTrash()
    await loadTrashStats()
    trashMsg.value =
      `已恢复「${rep.title}」→ ${rep.exported_path}` +
      (rep.index_updated ? '' : '（索引未同步，建议重建索引）') +
      (rep.warnings.length ? `\n${rep.warnings.join('\n')}` : '')
  } catch (e) {
    trashMsg.value = writeErrorText(e)
  } finally {
    trashBusy.value = false
  }
}

async function purgeNow() {
  if (!confirm('立即清理保留期已过的回收站项？此操作不可撤销。')) return
  trashBusy.value = true
  try {
    const rep = await api.purgeTrash(0)
    trashMsg.value = `清理完成：删除 ${rep.removed} 项（${formatSize(rep.bytes)}），保留 ${rep.kept} 项`
    trashItems.value = await api.listTrash()
    await loadTrashStats()
    await loadSync()
  } catch (e) {
    trashMsg.value = writeErrorText(e)
  } finally {
    trashBusy.value = false
  }
}

async function openTrashDir() {
  try { await api.openTrashDir() } catch (e) { alert(writeErrorText(e)) }
}

function emptySyncForm(): SyncCfg {
  // U1：`local_root` 已废弃（同步根恒为库根），仍保留空串只为与后端结构对齐
  return {
    enabled: false, role: 'writer', local_root: '', endpoint: '', bucket: '',
    prefix: '', region: 'us-east-1', path_style: true, access_key_id: '',
    credential_user: '', concurrency: 4, auto_check_on_start: true,
    initialized: false,
  }
}

async function loadSync() {
  try {
    syncCfg.value = await api.getSyncConfig()
    form.value = { ...emptySyncForm(), ...syncCfg.value.config }
    syncStatus.value = await api.getSyncStatus()
  } catch (e) {
    syncMsg.value = String(e)
  }
}

// 初始化/同步进度事件在下方统一 listen 处注册

async function toggleSyncEnabled(e: Event) {
  const on = (e.target as HTMLInputElement).checked
  if (on && syncCfg.value) {
    form.value.enabled = true
    syncCfg.value.config.enabled = true
    syncCfg.value.config.role = syncCfg.value.config.role || 'writer'
    return
  }
  if (!syncCfg.value) return
  // 关闭：保存 enabled=false（保留配置，凭据不动）
  try {
    await api.saveSyncConfig({ ...form.value, enabled: false }, null)
    await loadSync()
  } catch (err) {
    alert(String(err))
  }
}

// `chooseSyncRoot` 已随 U1 删除：同步根恒为笔记库根，用户在「主数据目录」里选即可。

async function testConnection() {
  syncBusy.value = 'test'
  testResult.value = null
  syncMsg.value = ''
  try {
    testResult.value = await api.testCloudConnection({ ...form.value }, secretKey.value)
  } catch (e) {
    syncMsg.value = String(e)
  } finally {
    syncBusy.value = ''
  }
}

async function saveConfig() {
  syncBusy.value = 'save'
  syncMsg.value = ''
  try {
    await api.saveSyncConfig({ ...form.value }, secretKey.value || null)
    secretKey.value = '' // 保存后立即清空，不再回显
    await loadSync()
    syncMsg.value = '配置已保存'
  } catch (e) {
    syncMsg.value = String(e)
  } finally {
    syncBusy.value = ''
  }
}

async function initSync() {
  syncBusy.value = 'init'
  syncMsg.value = ''
  syncProgress.value = ''
  try {
    const rep = await api.initCloudSync(form.value.role)
    syncMsg.value = '初始化完成：' + syncReportSummary(rep)
    await loadSync()
  } catch (e) {
    syncMsg.value = String(e)
  } finally {
    syncBusy.value = ''
    syncProgress.value = ''
  }
}

async function doSync(direction: 'up' | 'down') {
  syncBusy.value = direction
  syncMsg.value = ''
  syncProgress.value = ''
  try {
    const rep = await api.runSync(direction)
    syncMsg.value = (direction === 'up' ? '上行' : '下行') + '完成：' + syncReportSummary(rep)
    await loadSync()
  } catch (e) {
    syncMsg.value = String(e)
  } finally {
    syncBusy.value = ''
    syncProgress.value = ''
  }
}

async function clearCreds() {
  if (!confirm('确定清除已存的云端凭据？云同步将停用。')) return
  try {
    await api.clearCloudCredentials()
    await loadSync()
  } catch (e) {
    alert(String(e))
  }
}

function syncReportSummary(r: SyncReport): string {
  const parts: string[] = []
  if (r.direction === 'up') parts.push(`上传 ${r.uploaded} / 跳过 ${r.skipped}`)
  else if (r.direction === 'down') parts.push(`下载 ${r.downloaded} / 移入回收站 ${r.trashed}`)
  else parts.push('无变更')
  if (r.conflicts) parts.push(`冲突留存 ${r.conflicts}`) // U5 → _conflicts/
  if (r.oversized) parts.push(`超限 ${r.oversized}`)
  if (r.failures.length) parts.push(`失败 ${r.failures.length}`)
  parts.push(`${r.elapsed_ms} ms`)
  return parts.join('，')
}


// 索引 / 巡检 / 云同步进度事件。
// ⚠️ 不能用「顶层 await」（曾把本组件变成异步 setup，而 App.vue 没有 <Suspense>，
// Vue 会永远挂起不渲染 ⇒ 设置页整块白屏且无任何报错）。统一挪进 onMounted。
const unlistenFns: Array<() => void> = []
onMounted(async () => {
  try {
    const { listen } = await import('@tauri-apps/api/event')
    unlistenFns.push(
      await listen<{ done: number; total: number }>('index-progress', (e) => {
        progress.value = `索引进度: ${e.payload.done}/${e.payload.total} 篇`
      }),
      await listen<{ stage: string; done: number; total: number }>('verify-progress', (e) => {
        verifyProgress.value = `${e.payload.stage}: ${e.payload.done}/${e.payload.total}`
      }),
      await listen<{ phase: string; done: number; total: number }>('sync-progress', (e) => {
        syncProgress.value = `${e.payload.phase}: ${e.payload.done}/${e.payload.total}`
      }),
      await listen<{ level: string; message: string }>('sync-status', (e) => {
        syncMsg.value = e.payload.message
      }),
    )
  } catch (e) {
    syncMsg.value = String(e)
  }
})
onUnmounted(() => {
  unlistenFns.forEach((u) => {
    try { u() } catch { /* 幂等 */ }
  })
})

async function chooseSourceDir() {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({ directory: true, title: '选择源数据目录（只读）' })
  if (typeof dir !== 'string') return
  try {
    await api.pickDefaultDataDir(dir)
    settings.value = await api.getSettings()
    // 源目录变更后，若当前为源上下文则重建源索引
    if (viewContext.value === 'source') await rebuild(true)
  } catch (e) {
    alert(String(e))
  }
}

async function chooseLibraryDir() {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({ directory: true, title: '选择主数据目录（笔记库根）' })
  if (typeof dir !== 'string') return
  try {
    const st = await api.pickLibraryDir(dir)
    settings.value = await api.getSettings()
    if (st.kind === 'rejected') {
      alert(`无法用作笔记库：${st.reason}`)
      return
    }
    // 上下文由后端按「清单能否打开」定：empty / no_manifest → none（不打开任何笔记）
    viewContext.value = (await api.getLibraryStatus()).context
    await loadTrashStats() // 回收站根随库根变 → 统计要跟着刷新
    if (st.kind === 'ready') {
      await rebuild(true)
    } else if (st.kind === 'no_manifest') {
      alert(
        `目录含 zip 但缺少清单（export.db）：请先用 wiz-cli manifest-rebuild 重建清单（在此之前不会打开任何笔记）。`
      )
    } else {
      alert(
        `已设置空目录为笔记库（尚无内容，暂不打开任何笔记）：可通过「读取为知笔记 ▸ 导出 ▸ 导入到我的笔记库」导入内容。`
      )
    }
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
    // 源上下文→源索引；库上下文→库索引（未打开任何笔记时按钮已禁用）
    report.value =
      viewContext.value === 'source' ? await api.buildIndex() : await api.buildLibraryIndex()
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
      <label>主数据目录（笔记库）</label>
      <span>{{ settings?.library_dir || '（未设置）' }}</span>
      <button @click="chooseLibraryDir">选择…</button>
      <span style="color: var(--text-2); font-size: 12px">当前上下文：{{ ctxText }}</span>
    </div>

    <div class="settings-row">
      <label>源数据目录（只读，仅用于导入/导出）</label>
      <span>{{ settings?.source_dir || '（未设置）' }}</span>
      <button @click="chooseSourceDir">选择…</button>
    </div>

    <div class="settings-row">
      <label>索引目录</label>
      <span>~/.wizreader/</span>
    </div>

    <!-- ========== 回收站（笔记库；T7：根 = 库根，不依赖云同步） ========== -->
    <template v-if="settings?.library_dir">
      <div class="settings-row" style="margin-top: 20px">
        <label>回收站（笔记库）</label>
        <button :disabled="trashBusy" @click="toggleTrash">
          {{
            showTrash
              ? '收起'
              : `查看（${trashStats?.items ?? 0} 项 / ${formatSize(trashStats?.bytes ?? 0)}）`
          }}
        </button>
        <button :disabled="trashBusy" @click="openTrashDir">打开目录</button>
        <span style="color: var(--text-2); font-size: 12px">
          保留 {{ trashStats?.retention_days ?? 30 }} 天；位置：{{ trashStats?.root }}/_trash
        </span>
      </div>
      <div v-if="showTrash" class="report-box trash-box">
        <div v-for="t in trashItems" :key="t.guid" class="settings-row">
          <span style="flex: 1">
            {{ t.title }}
            <span style="color: var(--text-2)">
              （{{ formatSize(t.size) }}，移除于 {{ t.removed_at }}，剩 {{ t.days_left }} 天）
            </span>
            <span v-if="!t.restorable" class="trash-bad">· 不可恢复：{{ t.reason }}</span>
            <span v-else-if="!t.has_snapshot" class="trash-warn">
              · 旧墓碑（无恢复载荷）：只能恢复文件，不能还原清单行
            </span>
          </span>
          <button
            :disabled="trashBusy || !t.restorable || savedIsReader"
            :title="savedIsReader ? '只读端不接受本地写入（含恢复），请到写入端操作' : ''"
            @click="restoreNote(t)"
          >
            恢复
          </button>
        </div>
        <div v-if="!trashItems.length" style="color: var(--text-2)">回收站为空</div>
        <div class="settings-row" style="margin-top: 6px">
          <button :disabled="trashBusy" @click="purgeNow">立即清理超期项</button>
        </div>
        <div v-if="trashMsg" class="trash-msg">{{ trashMsg }}</div>
      </div>
    </template>

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

    <!-- ========== 云同步（FR-07 阶段二） ========== -->
    <div class="settings-row" style="margin-top: 20px">
      <label>云同步（MinIO / S3）</label>
      <input
        type="checkbox"
        :checked="syncCfg?.config.enabled"
        @change="toggleSyncEnabled($event)"
      />
      <span style="color: var(--text-2); font-size: 12px">
        secret key 只存系统钥匙串；明文 HTTP 仅限本机/内网私有 IP，公网必须 HTTPS
      </span>
    </div>

    <div v-if="syncCfg && syncCfg.config.enabled" class="report-box">
      <div class="settings-row">
        <label>本机角色</label>
        <label class="inline-check">
          <input v-model="form.role" type="radio" value="writer" /> 写入端（以本地库为准，上行覆盖云端）
        </label>
        <label class="inline-check">
          <input v-model="form.role" type="radio" value="reader" /> 只读端（以云端为准，只下行，不接受本地写入）
        </label>
      </div>
      <div class="settings-row">
        <label>同步根</label>
        <span style="color: var(--text-2); font-size: 12px">
          恒为笔记库根：<code class="path-code">{{ settings?.library_dir ?? '（尚未设置）' }}</code>
          —— 没有第二个根可挑（要换请到上方「主数据目录」改，Q18：不得等于/嵌套源数据目录）
        </span>
      </div>
      <div class="settings-row">
        <label>Endpoint</label>
        <input class="text-input" v-model="form.endpoint" placeholder="http://minio.example.local:9000" />
        <label class="inline-check"><input v-model="form.path_style" type="checkbox" /> path-style（MinIO 建议）</label>
      </div>
      <div class="settings-row">
        <label>Bucket / Prefix</label>
        <input class="text-input" v-model="form.bucket" placeholder="wizreader" style="width: 140px" />
        <input class="text-input" v-model="form.prefix" placeholder="sync/v1" style="width: 140px" />
        <input class="text-input" v-model="form.region" placeholder="region" style="width: 100px" />
      </div>
      <div class="settings-row">
        <label>Access Key</label>
        <input class="text-input" v-model="form.access_key_id" placeholder="AK…" />
        <input
          class="text-input"
          v-model="secretKey"
          type="password"
          :placeholder="syncCfg.credential_set ? '已存钥匙串（留空沿用）' : 'Secret Key…'"
        />
      </div>
      <div class="settings-row">
        <label>数据格式 / 并发</label>
        <span style="color: var(--text-2); font-size: 12px">
          native（无损：源 zip 逐字节拷贝，不可选 —— D0）
        </span>
        <input
          type="number"
          :value="form.concurrency"
          min="1"
          max="16"
          style="width: 60px"
          @change="form.concurrency = Number(($event.target as HTMLInputElement).value) || 4"
        />
        <label class="inline-check">
          <input v-model="form.auto_check_on_start" type="checkbox" /> 启动时自动检查更新
        </label>
      </div>
      <div class="settings-row">
        <label>操作</label>
        <button :disabled="syncBusy !== ''" @click="testConnection">
          {{ syncBusy.startsWith('test') ? '测试中…' : '测试连接' }}
        </button>
        <button :disabled="syncBusy !== ''" @click="saveConfig">保存配置</button>
        <button
          v-if="syncCfg.credential_set && syncCfg.config.enabled"
          :disabled="syncBusy !== ''"
          @click="initSync"
        >
          {{ syncBusy.startsWith('init') ? '初始化中…' : syncCfg.config.initialized ? '重新初始化' : '初始化' }}
        </button>
        <button :disabled="syncBusy !== ''" @click="clearCreds">清除凭据</button>
      </div>
      <div class="progress-text">{{ syncMsg }}</div>
      <div v-if="testResult" class="report-box" style="margin-top: 6px">
        {{ testResult.ok ? '✅' : '❌' }} {{ testResult.message }}（{{ testResult.latency_ms }} ms）
        读={{ testResult.can_read }} 写={{ testResult.can_write }} 前缀对象数={{ testResult.objects_under_prefix }}
      </div>
    </div>

    <!-- 同步状态与手动操作 -->
    <div v-if="syncStatus && syncStatus.enabled && syncStatus.initialized" class="report-box">
      <div>
        角色：{{ syncStatus.role === 'reader' ? '只读端' : '写入端' }}
        ・revision：{{ syncStatus.local_revision }}
        ・上次同步：{{ syncStatus.last_sync_at ?? '从未' }}
      </div>
      <div v-if="syncStatus.last_report">
        最近结果：{{ syncReportSummary(syncStatus.last_report) }}
      </div>
      <div class="settings-row" style="margin-top: 8px">
        <label>手动同步</label>
        <button
          :disabled="syncBusy !== '' || isReader"
          :title="isReader ? '只读端以云端为准，不支持上行；请到写入端上行' : ''"
          @click="doSync('up')"
        >
          {{ syncBusy === 'up' ? '上行中…' : '立即上行' }}
        </button>
        <button :disabled="syncBusy !== ''" @click="doSync('down')">{{ syncBusy === 'down' ? '下行中…' : '立即下行' }}</button>
        <span class="progress-text">{{ syncProgress }}</span>
      </div>
      <div class="settings-row">
        <label>回收站</label>
        <span style="color: var(--text-2); font-size: 12px">
          见上方「回收站（笔记库）」—— 回收站属于笔记库，与云同步开关无关
        </span>
      </div>
    </div>

    <div class="settings-row">
      <label>重建索引</label>
      <button :disabled="building || !canRebuild" @click="rebuild()">
        {{ building ? '构建中…' : confirmRebuild ? '确认重建？（再次点击）' : (viewContext === 'source' ? '重建源索引' : '重建库索引') }}
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
      <button :disabled="verifying || !settings?.source_dir" @click="runVerify">
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
.text-input {
  width: 220px;
  font-size: 12px;
  padding: 3px 6px;
}
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
/* 回收站（库根）：不可恢复 / 旧墓碑 / 操作结果 */
.trash-box {
  max-height: 320px;
  overflow: auto;
}
.trash-bad {
  color: var(--danger);
}
.trash-warn {
  color: #a85c00;
}
.trash-msg {
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px solid var(--border);
  color: var(--text-2);
  white-space: pre-wrap;
}
</style>
