<script setup lang="ts">
import { ref, onMounted, computed, nextTick } from 'vue'
import { api, type Settings, type NoteItem } from './api'
import { exportNotesTo, exportNotesAsZips, exportNoteAs } from './exporter'
import FolderTree from './components/FolderTree.vue'
import NoteList from './components/NoteList.vue'
import Reader from './components/Reader.vue'
import SearchResults from './components/SearchResults.vue'

const settings = ref<Settings | null>(null)
const folder = ref('')
const currentGuid = ref<string | null>(null)
const currentNote = ref<NoteItem | null>(null)
const view = ref<'browse' | 'search'>('browse')
const searchKw = ref('')
const searchInput = ref<HTMLInputElement | null>(null)
const history = ref<string[]>([])
const treeRef = ref<InstanceType<typeof FolderTree> | null>(null)
const bootError = ref('')

onMounted(async () => {
  settings.value = await api.getSettings()
  applyTheme()
  // 检查索引与数据源：无数据源时引导设置
  if (!settings.value.data_dir) {
    view.value = 'browse'
    bootError.value = '尚未设置数据源目录，请点击右上角“设置数据源”'
  }
  loadHistory()
  // 重建索引进度事件（顶栏进度条）
  const { listen } = await import('@tauri-apps/api/event')
  listen<{ done: number; total: number }>('index-progress', (e) => {
    pickProgress.value = e.payload
    pickTip.value = `重建索引中 ${e.payload.done}/${e.payload.total} 篇`
  })
})

async function loadHistory() {
  try {
    const h = await api.getSearchHistory()
    history.value = h.map(([kw]) => kw)
  } catch {
    history.value = []
  }
}

const themeClass = computed(() =>
  settings.value?.theme === 'dark'
    ? 'theme-dark'
    : settings.value?.theme === 'light'
      ? 'theme-light'
      : 'theme-system'
)
function applyTheme() {
  document.documentElement.className = themeClass.value
}

function openNote(guid: string, item?: NoteItem) {
  currentGuid.value = guid
  if (item) currentNote.value = item
}

// 信息栏"目录"点击跳转
window.addEventListener('wiz-jump-folder', (e) => {
  const loc = (e as CustomEvent).detail as string
  view.value = 'browse'
  folder.value = loc
})

function doSearch() {
  if (!searchKw.value.trim()) return
  view.value = 'search'
  loadHistory()
}

async function onHistoryClick(kw: string) {
  searchKw.value = kw
  doSearch()
}

// 快捷键（NFR-5）：Cmd+F 检索 / Esc 返回 / ↑↓ 切换笔记
window.addEventListener('keydown', async (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'f') {
    e.preventDefault()
    view.value = 'search'
    await nextTick()
    searchInput.value?.focus()
  } else if (e.key === 'Escape') {
    view.value = 'browse'
  } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    if (view.value !== 'browse') return
    e.preventDefault()
    // 列表内切换（简易实现：由 NoteList 重新加载后按序切换）
    const items = document.querySelectorAll('.note-item')
    if (!items.length) return
    const idx = Array.from(items).findIndex((el) => el.classList.contains('active'))
    const next = e.key === 'ArrowDown' ? Math.min(idx + 1, items.length - 1) : Math.max(idx - 1, 0)
    ;(items[next] as HTMLElement)?.click()
  }
})

const showSettings = ref(false)
const picking = ref(false)
const pickTip = ref('')
// 重建索引进度（index-progress 事件，done/total 篇）
const pickProgress = ref<{ done: number; total: number } | null>(null)
const pickPct = computed(() =>
  pickProgress.value && pickProgress.value.total > 0
    ? Math.round((pickProgress.value.done / pickProgress.value.total) * 100)
    : 0
)

// 顶端入口：直接选择为知数据源目录，选完自动重建索引
async function pickDataDir() {
  if (picking.value) return
  const [{ open }, { homeDir, join }] = await Promise.all([
    import('@tauri-apps/plugin-dialog'),
    import('@tauri-apps/api/path'),
  ])  // 为知 macOS 数据目录 ~/.wiznote/<账号>/data 为多层隐藏目录，面板中难以导航：
  // 优先自动探测候选，直接定位；探测不到则回退 ~/.wiznote，面板内可按 ⌘⇧. 显示隐藏目录
  let def: string | undefined
  try {
    def = (await api.detectWiznoteDir()) ?? undefined
  } catch {
    /* 探测失败不阻塞手动选择 */
  }
  if (!def) {
    try {
      def = await join(await homeDir(), '.wiznote')
    } catch {
      /* path 权限受限时忽略 */
    }
  }
  const dir = await open({
    directory: true,
    title: '选择为知数据源目录（含 notes/ 与 index.db；隐藏目录按 ⌘⇧. 显示）',
    defaultPath: def,
  })
  if (typeof dir !== 'string') return
  picking.value = true
  pickTip.value = '校验数据源…'
  try {
    await api.pickDefaultDataDir(dir)
    settings.value = await api.getSettings()
    bootError.value = ''
    pickTip.value = '重建索引中…'
    const r = await api.buildIndex()
    await treeRef.value?.load()
    alert(
      `数据源已设置：${dir}\n索引重建完成：${r.note_count} 篇，耗时 ${r.elapsed_ms} ms` +
        (r.ok ? '' : '\n⚠️ 存在偏离项，可在 ⚙ 设置页查看报告')
    )
  } catch (e) {
    alert(String(e))
  } finally {
    picking.value = false
    pickTip.value = ''
    pickProgress.value = null
  }
}

// 顶栏导出菜单（FR-08）：仅导出功能；设置入口独立在旁边的「设置」按钮
const showExportMenu = ref(false)
const exporting = ref(false)
async function runExport(fn: () => Promise<boolean>) {
  if (exporting.value) return
  showExportMenu.value = false
  exporting.value = true
  try {
    await fn()
  } finally {
    exporting.value = false
  }
}
function exportSelFolder() {
  runExport(() => exportNotesTo(folder.value))
}
// 批量导出为「每篇一个 zip」（FR-08.1 / FR-02 瘦身可选）
function exportSelFolderZips() {
  runExport(() => exportNotesAsZips(folder.value))
}
// 当前笔记信息优先用列表已知的 NoteItem，拿不到（如历史记录进入）再取详情
async function currentNoteMeta() {
  const g = currentGuid.value
  if (!g) return null
  if (currentNote.value?.guid === g) return currentNote.value
  return api.getNoteDetail(g)
}
async function exportCurrentNote(kind: 'zip' | 'html') {
  const meta = await currentNoteMeta()
  if (!meta) return
  runExport(() => exportNoteAs(kind, meta.guid, meta.title))
}
async function exportCurrentFolder() {
  const meta = await currentNoteMeta()
  if (!meta) return
  runExport(() => exportNotesTo(meta.location))
}

function toggleSettings() {
  showSettings.value = !showSettings.value
  if (!showSettings.value) {
    // 设置可能已变更（数据源/主题）
    api.getSettings().then((s) => {
      settings.value = s
      applyTheme()
      treeRef.value?.load()
    })
  }
}
// 导出入口统一在顶栏「导出」菜单（exporter.ts），设置页与 Reader 不再重复放置
</script>

<template>
  <div class="layout" :class="themeClass">
    <div class="topbar">
      <strong style="margin-right: 8px">WizReader</strong>
      <input
        ref="searchInput"
        v-model="searchKw"
        type="text"
        placeholder="全文检索（≥3 字符走 FTS5，支持中文/代码/IP）…  ⌘F"
        @keydown.enter="doSearch"
      />
      <button @click="doSearch">检索</button>
      <button :class="{ active: view === 'browse' }" @click="view = 'browse'">浏览</button>
      <span v-if="pickTip" class="pick-tip">{{ pickTip }}</span>
      <span v-if="picking && pickProgress" class="progress-bar"
        ><i :style="{ width: pickPct + '%' }"></i
      ></span>
      <button :disabled="picking" @click="pickDataDir">设置数据源</button>
      <!-- 导出菜单（FR-08）：仅导出功能；设置按钮独立在右侧 -->
      <div class="export-menu">
        <button :disabled="exporting" @click="showExportMenu = !showExportMenu">
          导出 {{ exporting ? '…' : '▾' }}
        </button>
        <template v-if="showExportMenu">
          <div class="menu-backdrop" @click="showExportMenu = false"></div>
          <div class="menu-pop">
            <!-- 两种批量格式（FR-08）：通用文件= 目录树还原可双击浏览；每篇 zip= 继承为知原生格式 -->
            <a @click="exportSelFolder">
              {{ folder ? `导出目录「${folder}」（通用文件）…` : '导出全部笔记（通用文件）…' }}
            </a>
            <a @click="exportSelFolderZips">
              {{ folder ? `导出目录「${folder}」（每篇 zip）…` : '导出全部笔记（每篇 zip）…' }}
            </a>
            <template v-if="currentGuid">
              <div class="menu-sep"></div>
              <a @click="exportCurrentNote('zip')">当前笔记：导出 zip…</a>
              <a @click="exportCurrentNote('html')">当前笔记：导出 HTML…</a>
              <a @click="exportCurrentFolder">当前笔记所在目录…</a>
            </template>
          </div>
        </template>
      </div>
      <button @click="toggleSettings">设置</button>
    </div>

    <!-- 设置页 -->
    <template v-if="showSettings">
      <div class="main" style="flex-direction: column">
        <SettingsView />
      </div>
    </template>

    <!-- 三栏主界面 -->
    <template v-else>
      <div v-if="bootError" class="empty-hint" style="flex: 1">
        {{ bootError }}
      </div>
      <div class="main">
        <FolderTree ref="treeRef" :selected="folder" @select="folder = $event" />

        <template v-if="view === 'browse'">
          <NoteList :folder="folder" @open="openNote" />
          <Reader :guid="currentGuid" :settings="settings!" />
        </template>

        <template v-else>
          <div class="panel" style="width: 220px; border-right: 1px solid var(--border)">
            <div class="panel-head">检索范围</div>
            <div style="padding: 8px; font-size: 12px; color: var(--text-2)">
              范围：{{ folder ? folder : '全部目录' }}
            </div>
            <div class="panel-head">历史（最近 20）</div>
            <div class="list-scroll">
              <div v-for="h in history" :key="h" class="note-item" @click="onHistoryClick(h)">
                <div class="note-title">{{ h }}</div>
              </div>
            </div>
          </div>
          <SearchResults :kw="searchKw" :folder="folder" @open="openNote" />
          <Reader :guid="currentGuid" :settings="settings!" />
        </template>
      </div>
    </template>
  </div>
</template>

<script lang="ts">
import { defineComponent } from 'vue'
import SettingsView from './components/SettingsView.vue'
export default defineComponent({
  components: { SettingsView },
})
</script>

<style scoped>
/* 顶栏导出菜单（原 ⚙ 位置） */
.export-menu {
  position: relative;
}
.menu-backdrop {
  position: fixed;
  inset: 0;
  z-index: 40;
}
.menu-pop {
  position: absolute;
  right: 0;
  top: calc(100% + 6px);
  z-index: 41;
  min-width: 240px;
  padding: 6px;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 8px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.18);
  display: flex;
  flex-direction: column;
}
.menu-pop a {
  padding: 7px 10px;
  border-radius: 6px;
  cursor: pointer;
  color: var(--text);
  text-decoration: none;
  font-size: 13px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 380px;
}
.menu-pop a:hover {
  background: var(--bg-2);
}
.menu-sep {
  height: 1px;
  background: var(--border);
  margin: 4px 2px;
}
</style>
