<script setup lang="ts">
import { ref, onMounted, computed, nextTick, watch } from 'vue'
import { api, writeErrorText, type Settings, type NoteItem, type ViewContext, type NoteWriteReport, type TreeNode, type SyncReport } from './api'
import { exportNotesTo, exportNotesAsZips, exportNoteAs } from './exporter'
import FolderTree from './components/FolderTree.vue'
import NoteList from './components/NoteList.vue'
import Reader from './components/Reader.vue'
import SearchResults from './components/SearchResults.vue'
import NoteEditDrawer from './components/NoteEditDrawer.vue'
import NoteDialog from './components/NoteDialog.vue'

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

// ---------- 三栏宽度可调（目录 / 列表 / 阅读区，拖拽分隔条改宽度并持久化）----------
const noteListRef = ref<InstanceType<typeof NoteList> | null>(null)
const searchPaneRef = ref<HTMLElement | null>(null)
/** 阅读区最小宽度：拖宽左/中栏时保证它不被挤没 */
const MIN_READER = 360
type PaneKey = 'tree' | 'list' | 'search'
const PANE_MIN: Record<PaneKey, number> = { tree: 160, list: 200, search: 140 }
let paneDrag: { key: PaneKey; el: HTMLElement; startX: number; startW: number; max: number } | null =
  null

function paneEl(key: PaneKey): HTMLElement | null {
  if (key === 'tree') return (treeRef.value?.$el as HTMLElement) ?? null
  if (key === 'list') return (noteListRef.value?.$el as HTMLElement) ?? null
  return searchPaneRef.value
}

function onSplitDown(key: PaneKey, e: PointerEvent) {
  const el = paneEl(key)
  if (!el) return
  // 拖某栏时另一栏宽度不变、阅读区（flex:1）吸收变化：
  // max = 窗口宽 - 另一栏当前宽 - 阅读区最小宽
  const otherKey: PaneKey | null = key === 'tree' ? 'list' : key === 'list' ? 'tree' : null
  const otherW = otherKey ? (paneEl(otherKey)?.getBoundingClientRect().width ?? 0) : 0
  let max = window.innerWidth - MIN_READER - otherW
  if (key === 'search') max = Math.min(max, window.innerWidth * 0.4) // 检索范围栏别拖太宽
  paneDrag = { key, el, startX: e.clientX, startW: el.getBoundingClientRect().width, max }
  ;(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId)
  document.body.classList.add('col-resizing')
}
function onSplitMove(e: PointerEvent) {
  if (!paneDrag) return
  const w = Math.min(
    paneDrag.max,
    Math.max(PANE_MIN[paneDrag.key], paneDrag.startW + e.clientX - paneDrag.startX)
  )
  paneDrag.el.style.width = `${Math.round(w)}px`
}
function onSplitUp() {
  if (!paneDrag) return
  const { key, el } = paneDrag
  paneDrag = null
  document.body.classList.remove('col-resizing')
  try {
    const saved = JSON.parse(localStorage.getItem('wizreader.pane_widths') ?? '{}')
    saved[key] = el.getBoundingClientRect().width
    localStorage.setItem('wizreader.pane_widths', JSON.stringify(saved))
  } catch {
    /* 持久化失败不影响功能 */
  }
}

/** 把持久化的栏宽应用到当前视图（启动 / 视图与上下文切换后都会调） */
async function applyPaneWidths() {
  await nextTick()
  let saved: Record<string, number> = {}
  try {
    saved = JSON.parse(localStorage.getItem('wizreader.pane_widths') ?? '{}')
  } catch {
    return
  }
  for (const key of ['tree', 'list', 'search'] as PaneKey[]) {
    const w = saved[key]
    if (typeof w !== 'number' || !Number.isFinite(w)) continue
    const el = paneEl(key)
    if (el) el.style.width = `${Math.round(Math.max(PANE_MIN[key], w))}px`
  }
}

// ---------- 视图上下文（未打开 / 库 / 为知源）----------
// none = 未打开任何笔记：未设置数据目录，或数据目录的清单（export.db）不可读。
// 此时**不展示任何笔记**，也绝不静默改用为知源（源视图只能由用户显式进入）。
const viewContext = ref<ViewContext>('none')
const libraryDir = ref<string | null>(null)
const sourceDir = ref<string | null>(null)
const libraryReadable = ref(false)
const libraryError = ref('')
/** 库准入分类：unset / empty / no_manifest / rejected / ready（空态页据此分流引导） */
const libraryKind = ref('unset')

const isLibrary = computed(() => viewContext.value === 'library')
const isSource = computed(() => viewContext.value === 'source')
const noNotes = computed(() => viewContext.value === 'none')
const canSource = computed(() => !!sourceDir.value)

// ---- U4 / R7：本机角色（writer | reader）----
// `reader` = 只读端：库是云端镜像，本地写入必被下一次下行覆盖 ⇒ 写入口在 UI 上就置灰
//（后端 `write_targets` 还有一道 `READER_READONLY` 兜底，两侧双保险）。
const syncRole = ref('')
const isReaderRole = computed(() => syncRole.value === 'reader')
/** 写操作总闸：库上下文 **且** 不是只读端 */
const writable = computed(() => isLibrary.value && !isReaderRole.value)

// 「导入到我的笔记库」：目标是库根，只要**设置了**库目录即可（导入本身就会写清单），
// 故判据是 libraryDir 而非「清单已可读」——否则「空目录建库 → 导入」会被自己禁用。
const canImport = computed(
  () => !!libraryDir.value && canSource.value && !isReaderRole.value
)
// 空态页：未打开任何笔记（含「设了目录但清单读不了」）且不在设置页
const showEmpty = computed(() => !showSettings.value && noNotes.value)
// 上下文徽标：文案 / 配色 / 悬浮说明
const ctxLabel = computed(() =>
  isLibrary.value ? '库' : isSource.value ? '为知笔记' : '未打开'
)
const ctxClass = computed(() =>
  isLibrary.value ? 'ctx-lib' : isSource.value ? 'ctx-src' : 'ctx-none'
)
const ctxTitle = computed(() =>
  isLibrary.value
    ? `当前：我的笔记库（${libraryDir.value ?? ''}）`
    : isSource.value
      ? '当前：为知笔记原始数据（只读）'
      : '当前：未打开任何笔记'
)

// ---- 空态页 A（已设置数据目录，但没打开任何笔记）的分流文案 ----
const emptyTitle = computed(() => {
  switch (libraryKind.value) {
    case 'empty':
      return '笔记库还没有内容'
    case 'no_manifest':
      return '缺少清单（export.db）'
    case 'rejected':
      return '该目录不能用作笔记库'
    default:
      return '无法打开笔记库'
  }
})
// '' → 不显示红色错误块
const emptyReason = computed(() => {
  if (libraryKind.value === 'no_manifest') {
    return '目录内有 zip，但缺少清单 export.db —— 请先用 wiz-cli manifest-rebuild 重建清单。'
  }
  if (libraryKind.value === 'empty') return ''
  return libraryError.value || '清单（export.db）不可读'
})
const emptyHint = computed(() =>
  libraryKind.value === 'empty'
    ? '该目录已设为笔记库，但还没有内容。可直接新建笔记 / 目录开始写作，或从为知笔记导入。'
    : '为避免把为知笔记原始数据误当自有笔记，此处不会自动改用「为知笔记」——请修复数据目录后重试。'
)

onMounted(async () => {
  settings.value = await api.getSettings()
  applyTheme()
  await refreshStatus()
  loadHistory()
  void applyPaneWidths()
  // 索引 / 导出进度事件（顶栏进度条）
  const { listen } = await import('@tauri-apps/api/event')
  listen<{ done: number; total: number }>('index-progress', (e) => {
    pickProgress.value = e.payload
    if (!pickTip.value.includes('导入')) {
      pickTip.value = `重建索引中 ${e.payload.done}/${e.payload.total} 篇`
    }
  })
  listen<{ done: number; total: number }>('export-progress', (e) => {
    pickProgress.value = e.payload
    pickTip.value = `导出中 ${e.payload.done}/${e.payload.total} 篇`
  })
})

async function refreshStatus() {
  try {
    const st = await api.getLibraryStatus()
    libraryDir.value = st.library_dir
    sourceDir.value = st.source_dir
    viewContext.value = st.context
    libraryReadable.value = st.library_readable
    libraryError.value = st.library_error
    libraryKind.value = st.library_kind
  } catch (e) {
    // 状态查询失败 ⇒ 一律按「未打开任何笔记」处理：宁可空态，也不误展示为知笔记
    libraryDir.value = settings.value?.library_dir ?? null
    sourceDir.value = settings.value?.source_dir ?? null
    libraryReadable.value = false
    libraryError.value = String(e)
    libraryKind.value = 'unset'
    viewContext.value = 'none'
  }
  // U4：本机角色（读设置即可，不触网）—— 只读端要把写入口关掉（R7）
  try {
    syncRole.value = (await api.getSyncConfig()).config.role
  } catch {
    syncRole.value = '' // 读不到就不禁用（后端仍有 READER_READONLY 兜底）
  }
}

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

// ---------- 库内写操作（FR-11 P2 / S2：T6 重命名·移动 / T7 删除·恢复 / T8 编辑与错误码）----------
//
// 统一约定：
// ① 只有 `isLibrary` 上下文才给入口（为知视图与"未打开"是只读的，后端也有 WRONG_CONTEXT 兜底）；
// ② 每次写成功后走 `afterWrite()` 统一刷新 —— 目录树 / 列表 / 阅读区三者必须同时更新，
//    否则会出现"列表还是旧标题、右边还是旧正文"的错觉；
// ③ 失败一律把**具体错误码**摆出来（`writeErrorText`），绝不静默。
const editing = ref<NoteRef | null>(null)
const listVersion = ref(0)
const readerVersion = ref(0)
const toast = ref('')
let toastTimer: number | null = null

/** 写操作只需要这三样：列表项（NoteItem）与阅读区详情（NoteDetail）都满足该形状 */
type NoteRef = { guid: string; title: string; location: string }

/** 写操作结果提示（3.5s 自动消失；错误另有弹窗，不走这里） */
function showToast(msg: string) {
  toast.value = msg
  if (toastTimer) window.clearTimeout(toastTimer)
  toastTimer = window.setTimeout(() => (toast.value = ''), 3500)
}

/** 写后统一刷新：目录树 + 列表 + 阅读区 + 库状态 */
async function afterWrite() {
  listVersion.value += 1
  readerVersion.value += 1
  await Promise.all([
    treeRef.value?.load().catch(() => undefined),
    refreshStatus().catch(() => undefined),
  ])
  // 写路径统一收口 ⇒ 防抖自动同步也从这里触发（见 scheduleAutoSync）
  scheduleAutoSync()
}

function writeFailed(e: unknown, what: string) {
  alert(`${what}失败\n\n${writeErrorText(e)}`)
}

// ---- 防抖自动同步（方案 1：保存后静默数秒自动「下行对齐 → 上行」）----
// 触发点 = afterWrite()（新建/改名/移动/删除/导入/编辑器保存的统一收口）。
// 资格在**点火时**现查（而非调度时）：设置页可能刚改过云同步配置。
// 互斥：后端 run_sync 自带库根 .sync.lock（冲突时返回 SYNC_BUSY）——自动轮遇
// BUSY 一律静默让位（手动同步优先）；上行前置=先下行对齐（§7.1），失败即中止本轮。
const AUTO_SYNC_DEBOUNCE_MS = 4000
let autoSyncTimer: number | null = null
let autoSyncRunning = false
let autoSyncPending = false // 轮次进行中又有写操作 ⇒ 本轮结束后补一轮

function scheduleAutoSync() {
  if (autoSyncTimer) window.clearTimeout(autoSyncTimer)
  autoSyncTimer = window.setTimeout(() => {
    autoSyncTimer = null
    void runAutoSync()
  }, AUTO_SYNC_DEBOUNCE_MS)
}

async function runAutoSync() {
  if (autoSyncRunning) {
    autoSyncPending = true
    return
  }
  autoSyncRunning = true
  try {
    try {
      const { config } = await api.getSyncConfig()
      if (!config.enabled || !config.initialized || config.role !== 'writer') return
    } catch {
      return // 配置读不到（未设置目录等）⇒ 无从同步，跳过本轮
    }
    let down: SyncReport
    try {
      down = await api.runSync('down')
    } catch (e) {
      if (String(e).includes('SYNC_BUSY')) return
      showToast('自动同步失败（下行对齐）：' + writeErrorText(e))
      return
    }
    let up: SyncReport
    try {
      up = await api.runSync('up')
    } catch (e) {
      if (String(e).includes('SYNC_BUSY')) return
      showToast('自动同步失败（上行）：' + writeErrorText(e))
      return
    }
    const parts: string[] = []
    if (up.uploaded > 0) parts.push(`上行 ${up.uploaded} 篇`)
    const pulled = down.downloaded + down.trashed
    if (pulled > 0) parts.push(`下行 ${pulled} 项`)
    const conflicts = down.conflicts + up.conflicts
    if (conflicts > 0) parts.push(`冲突旁置 ${conflicts} 篇`)
    showToast(
      parts.length ? `自动同步完成：${parts.join('，')}` : '自动同步完成：云端已是最新',
    )
  } finally {
    autoSyncRunning = false
    if (autoSyncPending) {
      autoSyncPending = false
      scheduleAutoSync()
    }
  }
}

/** U4 / R7：只读端不得写。入口置灰已挡住绝大多数路径，这里再挡一次（防止遗留的右键菜单/快捷键路径绕过） */
function blockedByReaderRole(what: string): boolean {
  if (!isReaderRole.value) return false
  alert(
    `「${what}」不可用：本机角色是只读端（reader）\n\n` +
      '只读端以云端为准、不接受本地写入 —— 本地改动会在下一次下行时被覆盖。\n' +
      '请在写入端修改后再同步过来。'
  )
  return true
}

async function openEdit(item: NoteRef) {
  if (blockedByReaderRole('编辑正文')) return
  editing.value = item
}

async function onEditSaved(rep: NoteWriteReport) {
  showToast(
    `已保存正文：${rep.exported_path}（rev ${rep.revision}）` +
      (rep.index_updated ? '' : ' ｜ 索引未同步，建议重建索引')
  )
  await afterWrite()
}

function onRenameNote(item: NoteRef) {
  if (blockedByReaderRole('重命名')) return
  dialog.value = { kind: 'prompt', title: '重命名笔记', message: `原标题：${item.title}`, initial: item.title, item }
}

async function onDeleteNote(item: NoteRef) {
  if (blockedByReaderRole('删除')) return
  dialog.value = {
    kind: 'confirm',
    title: '删除笔记',
    message:
      `将「${item.title}」移入回收站（保留 30 天，可恢复）：\n${item.location}\n\n` +
      '删除后清单会记一条墓碑，云同步据此传播删除。',
    confirmText: '移入回收站',
    danger: true,
    item,
  }
}

async function onMoveNote(item: NoteRef) {
  if (blockedByReaderRole('移动')) return
  const folders = await folderOptions()
  dialog.value = {
    kind: 'folder',
    title: '移动到目录',
    message: `把「${item.title}」移动到：`,
    folders,
    current: item.location,
    item,
  }
}

/** 目录选择用的拍平列表（含库根）：数据来自同一棵目录树，避免另造一份口径 */
async function folderOptions(): Promise<{ path: string; name: string; depth: number }[]> {
  const out: { path: string; name: string; depth: number }[] = [{ path: '/', name: '📚 全部笔记（库根）', depth: 0 }]
  let tree: TreeNode[] = []
  try {
    tree = await api.getTree()
  } catch {
    return out
  }
  const walk = (nodes: TreeNode[], depth: number) => {
    for (const n of nodes) {
      if (n.is_system) continue // 系统保留区（_trash 等）不是移动目标
      out.push({ path: n.path, name: n.name, depth })
      if (n.children.length) walk(n.children, depth + 1)
    }
  }
  walk(tree, 1)
  return out
}

// 拖拽移动：NoteList 里按下时记住 guid，树节点松手时按 guid 提交
const dragging = ref<NoteItem | null>(null)
function onDragStart(item: NoteItem) {
  dragging.value = item
}
function onDragEnd() {
  dragging.value = null
}
async function onDropNote(location: string) {
  const item = dragging.value
  dragging.value = null
  if (!item) return
  const target = location === '' ? '/' : location
  if (target === item.location) return
  try {
    const rep = await api.moveNote(item.guid, target)
    showToast(`已移动：${rep.title} → ${rep.exported_path}`)
    await afterWrite()
  } catch (e) {
    writeFailed(e, '移动')
  }
}

/** 右键菜单：在访达中显示该篇（库内绝对路径由后端按清单解析，前端不拼路径） */
async function revealNote(item: NoteItem) {
  try {
    await api.revealInFinder(await api.getNoteFilePath(item.guid))
  } catch (e) {
    alert(writeErrorText(e))
  }
}

// ---------- 通用小弹窗（新建笔记 / 重命名输入 / 危险确认 / 目标目录选择）----------
type DialogState = {
  kind: 'prompt' | 'confirm' | 'folder' | 'new' | 'new_folder'
  title: string
  message: string
  initial?: string
  confirmText?: string
  danger?: boolean
  folders?: { path: string; name: string; depth: number }[]
  current?: string
  item?: NoteRef
  /** new_folder：在哪个目录之下创建（'/' = 库根） */
  base?: string
} | null
const dialog = ref<DialogState>(null)
/** 新建笔记进行中（防双击重复建） */
const creating = ref(false)

/** 新建笔记：标题 + 目标目录（默认当前选中目录）一次选好；确认后建「# 标题」模板并直接进编辑器 */
async function onCreateNote() {
  if (blockedByReaderRole('新建笔记')) return
  const folders = await folderOptions()
  dialog.value = {
    kind: 'new',
    title: '新建笔记',
    message: '笔记会以 Markdown 包写入笔记库，创建后直接打开编辑器写正文。',
    initial: '新建笔记',
    confirmText: '创建',
    folders,
    current: folder.value ? folder.value : '/',
  }
}

// ---------- 空库（无清单）就绪 + 新建目录入口 ----------
/** 空库就绪：无清单的空目录先建空 export.db 并切库上下文，刷新到三栏界面 */
async function ensureLibraryReady(): Promise<boolean> {
  try {
    await api.initLibraryManifest()
  } catch (e) {
    alert(writeErrorText(e))
    return false
  }
  await refreshStatus()
  bootError.value = ''
  await nextTick()
  await treeRef.value?.load()
  return true
}

/** 空态页「新建笔记」：先就绪空库，再走常规新建对话框 */
async function createNoteFromEmpty() {
  if (!(await ensureLibraryReady())) return
  await onCreateNote()
}

/** 新建目录：在 base（库根或当前选中目录）之下创建；成功后树里立即可见 */
function promptNewFolder(base: string) {
  if (blockedByReaderRole('新建目录')) return
  const b = base || '/'
  dialog.value = {
    kind: 'new_folder',
    title: '新建目录',
    message: `在「${b === '/' ? '全部笔记（库根）' : b}」下新建目录：`,
    initial: '新目录',
    confirmText: '创建',
    base: b,
  }
}

/** 空态页「新建目录」：先就绪空库，再弹目录名输入（建在库根下） */
async function createFolderFromEmpty() {
  if (!(await ensureLibraryReady())) return
  promptNewFolder('/')
}

async function onDialogConfirm(value: string, folderPicked: string) {
  const d = dialog.value
  dialog.value = null
  if (!d) return
  // 新建目录：value = 目录名，目标 = base（默认库根）之下；成功后刷新树并选中新目录
  if (d.kind === 'new_folder') {
    const base = d.base && d.base !== '/' ? d.base.replace(/\/+$/, '') : ''
    const target = `${base}/${value}`
    try {
      const loc = await api.createLibraryFolder(target)
      showToast(`已创建目录：${loc}`)
      view.value = 'browse'
      folder.value = loc
      await afterWrite()
    } catch (e) {
      writeFailed(e, '新建目录')
    }
    return
  }
  // 新建：无需 item（item 指向既有笔记）；创建成功后直接选中并进编辑器
  if (d.kind === 'new') {
    if (creating.value) return
    creating.value = true
    try {
      const md = `# ${value}\n\n`
      const rep = await api.createNote(value, folderPicked || '/', md)
      showToast(`已创建：${rep.exported_path}`)
      await afterWrite()
      // 立即选中并打开编辑抽屉 —— 新建即写作，不让用户再找一遍
      view.value = 'browse'
      currentGuid.value = rep.guid
      currentNote.value = null
      editing.value = { guid: rep.guid, title: rep.title, location: folderPicked || '/' }
    } catch (e) {
      writeFailed(e, '新建笔记')
    } finally {
      creating.value = false
    }
    return
  }
  if (!d.item) return
  const item = d.item
  try {
    let rep: NoteWriteReport
    if (d.kind === 'prompt') rep = await api.renameNote(item.guid, value)
    else if (d.kind === 'folder') rep = await api.moveNote(item.guid, value)
    else {
      rep = await api.deleteNote(item.guid)
      // 当前正打开的是被删的那篇 → 清空阅读区，免得停在一篇已删的笔记上
      if (currentGuid.value === item.guid) {
        currentGuid.value = null
        currentNote.value = null
      }
    }
    if (rep.warnings.length) {
      alert(`已完成（${rep.op}）：${rep.exported_path}\n\n注意：\n${rep.warnings.join('\n')}`)
    } else {
      showToast(`${rep.op === 'delete' ? '已移入回收站' : '已完成'}：${rep.title}`)
    }
    await afterWrite()
  } catch (e) {
    writeFailed(e, d.kind === 'prompt' ? '重命名' : d.kind === 'folder' ? '移动' : '删除')
  }
}

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
// 视图（浏览/检索）与页面形态切换后，对应面板才挂载 ⇒ 切完重放一次持久化栏宽
watch([view, viewContext, showSettings], () => void applyPaneWidths())
const picking = ref(false)
const pickTip = ref('')
// 重建索引 / 导出进度（index-progress / export-progress 事件，done/total 篇）
const pickProgress = ref<{ done: number; total: number } | null>(null)
const pickPct = computed(() =>
  pickProgress.value && pickProgress.value.total > 0
    ? Math.round((pickProgress.value.done / pickProgress.value.total) * 100)
    : 0
)

// ---------- 上下文切换 ----------
async function switchContext(ctx: ViewContext, v?: 'browse' | 'search') {
  closeMenus()
  // 切上下文即离开当前库/源：关闭编辑抽屉与弹窗，避免它们在只读视图里还挂着
  editing.value = null
  dialog.value = null
  dragging.value = null
  try {
    viewContext.value = await api.setViewContext(ctx)
    if (v) view.value = v
    folder.value = ''
    currentGuid.value = null
    currentNote.value = null
    await nextTick()
    await treeRef.value?.load()
    loadHistory()
  } catch (e) {
    alert(String(e))
  }
}

// 「返回自有笔记」：为知视图横幅 / 上下文徽标共用。
// 库可用 → 切回库视图；库不可用（未设置或清单读不了）→ 落到空态页如实说明原因，
// 而不是把用户留在为知视图里无处可去。
async function backToMyNotes() {
  closeMenus()
  if (libraryReadable.value) {
    await switchContext('library')
    return
  }
  await refreshStatus()
  showSettings.value = false
  await switchContextSilent('none')
}

// ---------- 目录选择对话框 ----------
async function pickLibraryDirDialog(): Promise<string | null> {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const dir = await open({
    directory: true,
    title: '选择主数据目录（笔记库根，可读可写）',
    defaultPath: libraryDir.value ?? undefined,
  })
  return typeof dir === 'string' ? dir : null
}

async function pickSourceDirDialog(): Promise<string | null> {
  const [{ open }, { homeDir, join }] = await Promise.all([
    import('@tauri-apps/plugin-dialog'),
    import('@tauri-apps/api/path'),
  ])
  // 为知 macOS 数据目录 ~/.wiznote/<账号>/data 为多层隐藏目录，面板中难以导航：
  // 优先自动探测候选，直接定位；探测不到则回退 ~/.wiznote，面板内可按 ⌘⇧. 显示隐藏目录
  let def: string | undefined = sourceDir.value ?? undefined
  if (!def) {
    try {
      def = (await api.detectWiznoteDir()) ?? undefined
    } catch {
      /* 探测失败不阻塞手动选择 */
    }
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
  return typeof dir === 'string' ? dir : null
}

// 顶栏「设置数据目录」/ 空态页大按钮：选择主数据目录（笔记库），按 LibraryStatus 三分支
async function pickLibraryDirFlow() {
  closeMenus()
  if (picking.value) return
  const dir = await pickLibraryDirDialog()
  if (!dir) return
  picking.value = true
  pickTip.value = '校验笔记库…'
  try {
    const st = await api.pickLibraryDir(dir)
    settings.value = await api.getSettings()
    await refreshStatus()
    if (st.kind === 'rejected') {
      alert(`无法用作笔记库：${st.reason}`)
      return
    }
    if (st.kind === 'empty') {
      bootError.value = ''
      alert(
        `已设置空目录为笔记库：${dir}\n可通过「读取为知笔记 ▸ 导出 ▸ 导入到我的笔记库」导入内容。`
      )
      return
    }
    if (st.kind === 'no_manifest') {
      alert(
        `目录含 zip 但缺少清单（export.db）：${dir}\n请先用 wiz-cli manifest-rebuild 重建清单后再设为笔记库。`
      )
      return
    }
    // ready → 建索引后进入
    pickTip.value = '构建库索引中…'
    const r = await api.buildLibraryIndex()
    await refreshStatus()
    bootError.value = ''
    await nextTick()
    await treeRef.value?.load()
    alert(
      `笔记库已就绪：${dir}\n索引构建完成：${r.note_count} 篇，耗时 ${r.elapsed_ms} ms` +
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

// 已设过、且经准入判定为「可写空库」（kind === 'empty'）的库目录 ⇒ 导入流程直接复用
function reuseLibraryDir(): string | null {
  return libraryKind.value === 'empty' ? libraryDir.value : null
}

// 空态页「从为知笔记导入」：源目录 → 库目录 → 导入（组合引导）
// 库目录**已设过**（空态页 A：kind === 'empty'，即已选中的可写空库）⇒ **直接复用**，
// 不再弹第二次选择框（此前无论是否已设都要重选一遍）；仅「尚未设置」时才要求选/建目录。
async function importFromWizFlow() {
  if (picking.value) return
  const srcDir = await pickSourceDirDialog()
  if (!srcDir) return
  picking.value = true
  try {
    pickTip.value = '校验数据源…'
    await api.pickDefaultDataDir(srcDir)
    settings.value = await api.getSettings()
    await refreshStatus()
    pickTip.value = '重建源索引中…'
    await api.buildIndex()
  } catch (e) {
    alert(String(e))
    picking.value = false
    pickTip.value = ''
    pickProgress.value = null
    return
  }
  picking.value = false
  pickTip.value = ''
  pickProgress.value = null
  // 库目录：已设过 ⇒ 复用，只在「尚未设置」时才弹选择框
  const libDir = reuseLibraryDir() ?? (await pickLibraryDirDialog())
  if (!libDir) return
  picking.value = true
  try {
    pickTip.value = '校验笔记库…'
    // 复用路径不弹框，但仍重放一次准入校验：保证后端 library_dir 与界面一致（幂等）
    const st = await api.pickLibraryDir(libDir)
    if (st.kind === 'rejected') {
      alert(`无法用作笔记库：${st.reason}`)
      return
    }
    pickTip.value = '导入到笔记库中…'
    const rep = await api.importToLibrary('')
    settings.value = await api.getSettings()
    await refreshStatus()
    bootError.value = ''
    await nextTick()
    await treeRef.value?.load()
    alert(
      `导入完成：${rep.export.notes_exported} 篇，附件 ${rep.attachments_copied} 个` +
        `（缺失 ${rep.attachments_missing}），索引 ${rep.index.note_count} 篇` +
        `\n目标笔记库：${libDir}`
    )
  } catch (e) {
    alert(String(e))
  } finally {
    picking.value = false
    pickTip.value = ''
    pickProgress.value = null
  }
}

// 「读取为知笔记 ▸ 设置数据源目录…」：选择源目录并重建源索引
async function pickSourceDirFlow() {
  closeMenus()
  if (picking.value) return
  const dir = await pickSourceDirDialog()
  if (!dir) return
  picking.value = true
  pickTip.value = '校验数据源…'
  try {
    await api.pickDefaultDataDir(dir)
    settings.value = await api.getSettings()
    await refreshStatus()
    pickTip.value = '重建源索引中…'
    const r = await api.buildIndex()
    await treeRef.value?.load()
    alert(
      `数据源已设置：${dir}\n源索引重建完成：${r.note_count} 篇，耗时 ${r.elapsed_ms} ms` +
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

// 「读取为知笔记 ▸ 重建源索引」：切源上下文并重建
async function rebuildSourceIndex() {
  closeMenus()
  if (picking.value) return
  picking.value = true
  try {
    viewContext.value = await api.setViewContext('source')
    pickTip.value = '重建源索引中…'
    const r = await api.buildIndex()
    await nextTick()
    await treeRef.value?.load()
    alert(`源索引重建完成：${r.note_count} 篇，耗时 ${r.elapsed_ms} ms`)
  } catch (e) {
    alert(String(e))
  } finally {
    picking.value = false
    pickTip.value = ''
    pickProgress.value = null
  }
}

// ---------- 顶栏「读取为知笔记」菜单 + 导出二级菜单 ----------
const showWizMenu = ref(false)
const showExportSub = ref(false)
const exporting = ref(false)
function closeMenus() {
  showWizMenu.value = false
  showExportSub.value = false
}
async function runExport(fn: () => Promise<boolean>) {
  if (exporting.value) return
  closeMenus()
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
// 批量导出为「每篇一个 zip」（FR-08.1；D0：恒 native 无损，无瘦身选项）
function exportSelFolderZips() {
  runExport(() => exportNotesAsZips(folder.value))
}
// 「导入到我的笔记库」：目标固定=库根
async function importToLibraryFlow() {
  if (exporting.value) return
  if (!canImport.value) {
    closeMenus()
    alert('需先设置源数据目录与主数据目录（笔记库）')
    return
  }
  closeMenus()
  exporting.value = true
  pickTip.value = '导入到笔记库中…'
  try {
    const rep = await api.importToLibrary(folder.value)
    await refreshStatus()
    await switchContextSilent('library')
    alert(
      `导入完成：${rep.export.notes_exported} 篇，附件 ${rep.attachments_copied} 个` +
        `（缺失 ${rep.attachments_missing}），索引 ${rep.index.note_count} 篇`
    )
  } catch (e) {
    alert(String(e))
  } finally {
    exporting.value = false
    pickTip.value = ''
    pickProgress.value = null
  }
}
// 导入后回到库视图（不弹错误框，静默切换 + 重载）
async function switchContextSilent(ctx: ViewContext) {
  try {
    viewContext.value = await api.setViewContext(ctx)
    folder.value = ''
    currentGuid.value = null
    currentNote.value = null
    await nextTick()
    await treeRef.value?.load()
  } catch {
    /* 忽略 */
  }
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
    // 设置可能已变更（数据目录/主题/上下文）
    api.getSettings().then(async (s) => {
      settings.value = s
      applyTheme()
      await refreshStatus()
      treeRef.value?.load()
    })
  }
}
</script>

<template>
  <div class="layout" :class="themeClass">
    <div class="topbar">
      <strong style="margin-right: 8px">WizReader</strong>
      <!-- 上下文徽标（库 / 为知笔记 / 未打开）：点击回到自有笔记 -->
      <span class="ctx-badge" :class="ctxClass" :title="ctxTitle" @click="backToMyNotes()">{{
        ctxLabel
      }}</span>
      <input
        ref="searchInput"
        v-model="searchKw"
        type="text"
        placeholder="全文检索（≥3 字符走 FTS5，支持中文/代码/IP）…  ⌘F"
        @keydown.enter="doSearch"
      />
      <button @click="doSearch">检索</button>
      <button :class="{ active: view === 'browse' }" @click="view = 'browse'">浏览</button>
      <!-- 新建笔记（仅库上下文可写时出现；后端 write_targets 另有兜底） -->
      <button v-if="writable" :disabled="creating" @click="onCreateNote">＋ 新建笔记</button>
      <span v-if="pickTip" class="pick-tip">{{ pickTip }}</span>
      <!-- 写操作结果提示（失败另有弹窗带错误码，不在此处） -->
      <span v-if="toast" class="op-toast">{{ toast }}</span>
      <span v-if="(picking || exporting) && pickProgress" class="progress-bar"
        ><i :style="{ width: pickPct + '%' }"></i
      ></span>
      <!-- 主数据目录入口统一收进设置页（与「读取为知笔记」不同，此项不再放顶栏） -->
      <!-- 读取为知笔记 ▾（源操作 + 导出）-->
      <div class="export-menu">
        <button :disabled="exporting" @click="showWizMenu = !showWizMenu; showExportSub = false">
          读取为知笔记 {{ showWizMenu ? '▴' : '▾' }}
        </button>
        <template v-if="showWizMenu">
          <div class="menu-backdrop" @click="closeMenus"></div>
          <div class="menu-pop">
            <a @click="pickSourceDirFlow">设置数据源目录…</a>
            <a :class="{ disabled: !canSource }" @click="canSource && rebuildSourceIndex()"
              >重建源索引</a
            >
            <div class="menu-sep"></div>
            <a
              :class="{ disabled: !canSource }"
              @click="canSource && switchContext('source', 'search')"
              >检索为知笔记…</a
            >
            <a
              :class="{ disabled: !canSource }"
              @click="canSource && switchContext('source', 'browse')"
              >浏览为知笔记…</a
            >
            <div class="menu-sep"></div>
            <!-- 导出 ▸ 二级菜单 -->
            <a @click="showExportSub = !showExportSub">导出 {{ showExportSub ? '▴' : '▸' }}</a>
            <template v-if="showExportSub">
              <div class="menu-pop sub">
                <a
                  :class="{ disabled: !canImport }"
                  @click="canImport && importToLibraryFlow()"
                  >导入到我的笔记库</a
                >
                <div class="menu-sep"></div>
                <a
                  :class="{ disabled: !canSource }"
                  @click="canSource && exportSelFolderZips()"
                  >{{ folder ? `导出目录「${folder}」（每篇 zip）…` : '导出全部（每篇 zip）…' }}</a
                >
                <a :class="{ disabled: !canSource }" @click="canSource && exportSelFolder()">{{
                  folder ? `导出目录「${folder}」（通用文件）…` : '导出全部（通用文件）…'
                }}</a>
                <template v-if="currentGuid">
                  <div class="menu-sep"></div>
                  <a @click="exportCurrentNote('zip')">当前笔记：导出 zip…</a>
                  <a @click="exportCurrentNote('html')">当前笔记：导出 HTML…</a>
                  <a @click="exportCurrentFolder">当前笔记所在目录…</a>
                </template>
              </div>
            </template>
          </div>
        </template>
      </div>
      <button @click="toggleSettings">⚙ 设置</button>
    </div>

    <!-- 为知笔记视图横幅：顶侧明显标识当前数据来源 + 醒目的「返回自有笔记」 -->
    <div v-if="isSource" class="wiz-banner">
      <span class="wiz-pill">为知笔记</span>
      <span class="wiz-banner-text">
        当前访问的是<strong>为知笔记原始数据</strong>（只读）—— 不属于你的笔记库，
        编辑 / 重命名 / 删除在此视图不可用
      </span>
      <button class="back-mine" @click="backToMyNotes">
        ← 返回自有笔记{{ libraryReadable ? '' : '（尚未就绪）' }}
      </button>
    </div>

    <!-- 只读端横幅（U4 / R7）：与"为知视图"不同 —— 库还是你的库，只是本机不接受写入 -->
    <div v-if="isLibrary && isReaderRole" class="wiz-banner">
      <span class="wiz-pill">只读端</span>
      <span class="wiz-banner-text">
        本机角色是<strong>只读端（reader）</strong>：库以云端为准，只下行、不接受本地写入 ——
        编辑 / 重命名 / 移动 / 删除 / 导入已禁用（改动会被下一次下行覆盖）。请到写入端修改
      </span>
      <button class="back-mine" @click="showSettings = true">查看同步设置</button>
    </div>

    <!-- 设置页 -->
    <template v-if="showSettings">
      <div class="main" style="flex-direction: column">
        <SettingsView />
      </div>
    </template>

    <!-- 空态页 A：已设置数据目录，但未打开任何笔记（空目录 / 缺清单 / 清单损坏）-->
    <template v-else-if="showEmpty && libraryDir">
      <div class="empty-page">
        <h2>{{ emptyTitle }}</h2>
        <p class="empty-sub">
          数据目录：<code class="path-code">{{ libraryDir }}</code>
        </p>
        <p v-if="emptyReason" class="empty-err">{{ emptyReason }}</p>
        <p class="empty-sub small">{{ emptyHint }}</p>
        <div class="empty-actions">
          <button class="big-btn" :disabled="picking" @click="createNoteFromEmpty">
            新建笔记
          </button>
          <button class="big-btn" :disabled="picking" @click="createFolderFromEmpty">
            新建目录
          </button>
          <button
            v-if="libraryKind === 'empty'"
            class="big-btn"
            :disabled="picking"
            @click="importFromWizFlow"
          >
            从为知笔记导入
          </button>
          <button class="big-btn" :disabled="picking" @click="pickLibraryDirFlow">
            重新选择数据目录
          </button>
          <button
            class="big-btn"
            :disabled="!canSource || picking"
            @click="switchContext('source', 'browse')"
          >
            浏览为知笔记
          </button>
        </div>
        <span v-if="pickTip" class="pick-tip">{{ pickTip }}</span>
      </div>
    </template>

    <!-- 空态页 B：尚未设置数据目录 -->
    <template v-else-if="showEmpty">
      <div class="empty-page">
        <h2>开始使用 WizReader</h2>
        <p class="empty-sub">
          设置一个主数据目录（笔记库），或直接从为知笔记导入生成属于你的笔记库。
        </p>
        <div class="empty-actions">
          <button class="big-btn" :disabled="picking" @click="pickLibraryDirFlow">
            设置数据目录
          </button>
          <button class="big-btn" :disabled="picking" @click="importFromWizFlow">
            从为知笔记导入
          </button>
        </div>
        <span v-if="pickTip" class="pick-tip">{{ pickTip }}</span>
      </div>
    </template>

    <!-- 三栏主界面 -->
    <template v-else>
      <div v-if="bootError" class="empty-hint" style="flex: 1">
        {{ bootError }}
      </div>
      <div class="main">
        <FolderTree
          ref="treeRef"
          :selected="folder"
          :droppable="writable"
          @select="folder = $event"
          @drop-note="onDropNote"
          @create-folder="promptNewFolder(folder || '/')"
        />

        <!-- 可拖拽分栏条：拖动调整目录/列表栏宽（阅读区吸收剩余空间） -->
        <div
          class="vsplit"
          title="拖动调整目录栏宽度"
          @pointerdown="onSplitDown('tree', $event)"
          @pointermove="onSplitMove"
          @pointerup="onSplitUp"
          @pointercancel="onSplitUp"
        ></div>

        <template v-if="view === 'browse'">
          <NoteList
            ref="noteListRef"
            :folder="folder"
            :writable="writable"
            :version="listVersion"
            @open="openNote"
            @edit="openEdit"
            @rename="onRenameNote"
            @move="onMoveNote"
            @delete="onDeleteNote"
            @reveal="revealNote"
            @dragstart="onDragStart"
            @dragend="onDragEnd"
          />
          <div
            class="vsplit"
            title="拖动调整列表栏宽度"
            @pointerdown="onSplitDown('list', $event)"
            @pointermove="onSplitMove"
            @pointerup="onSplitUp"
            @pointercancel="onSplitUp"
          ></div>
          <Reader
            :guid="currentGuid"
            :settings="settings!"
            :writable="writable"
            :version="readerVersion"
            @edit="openEdit"
            @rename="onRenameNote"
            @move="onMoveNote"
            @delete="onDeleteNote"
          />
        </template>

        <template v-else>
          <div ref="searchPaneRef" class="panel search-scope">
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
          <div
            class="vsplit"
            title="拖动调整检索范围栏宽度"
            @pointerdown="onSplitDown('search', $event)"
            @pointermove="onSplitMove"
            @pointerup="onSplitUp"
            @pointercancel="onSplitUp"
          ></div>
          <SearchResults :kw="searchKw" :folder="folder" @open="openNote" />
          <Reader
            :guid="currentGuid"
            :settings="settings!"
            :writable="writable"
            :version="readerVersion"
            @edit="openEdit"
            @rename="onRenameNote"
            @move="onMoveNote"
            @delete="onDeleteNote"
          />
        </template>
      </div>
    </template>

    <!-- 编辑正文抽屉（N2 选型 ①：源码 HTML + 实时预览） -->
    <NoteEditDrawer
      v-if="editing"
      :key="editing.guid + ':' + readerVersion"
      :guid="editing.guid"
      :title="editing.title"
      :location="editing.location"
      @close="editing = null"
      @saved="onEditSaved"
    />

    <!-- 写操作小弹窗：重命名输入 / 删除确认 / 移动目标目录 -->
    <NoteDialog
      v-if="dialog"
      :kind="dialog.kind"
      :title="dialog.title"
      :message="dialog.message"
      :initial="dialog.initial"
      :confirm-text="dialog.confirmText"
      :danger="dialog.danger"
      :folders="dialog.folders"
      :current="dialog.current"
      @confirm="onDialogConfirm"
      @cancel="dialog = null"
    />
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
/* 可拖拽分栏条：视觉上只是 1px 分隔线，命中区 7px（负 margin 外扩，不占布局宽度） */
.vsplit {
  position: relative;
  z-index: 5;
  flex: none;
  width: 7px;
  margin: 0 -3px;
  cursor: col-resize;
  touch-action: none;
}
.vsplit::after {
  content: '';
  position: absolute;
  left: 3px;
  top: 0;
  bottom: 0;
  width: 1px;
  background: var(--border);
}
.vsplit:hover::after,
.vsplit:active::after {
  left: 2.5px;
  width: 2px;
  background: var(--accent);
}
/* 检索视图「检索范围」栏默认宽（可被拖拽覆盖） */
.search-scope {
  width: 220px;
  flex: none;
}
/* 上下文徽标 */
.ctx-badge {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-width: 22px;
  height: 22px;
  padding: 0 6px;
  margin-right: 6px;
  border-radius: 11px;
  font-size: 12px;
  font-weight: 600;
  cursor: pointer;
  user-select: none;
  border: 1px solid var(--border);
}
.ctx-lib {
  background: rgba(46, 125, 50, 0.15);
  color: #2e7d32;
}
.ctx-src {
  background: rgba(232, 137, 12, 0.18);
  color: #a85c00;
}
.ctx-none {
  background: var(--bg-3);
  color: var(--text-2);
}
/* 为知视图横幅（顶侧明显标识 + 醒目的返回按钮）*/
.wiz-banner {
  flex: none;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 12px;
  background: #fff4e0;
  border-bottom: 1px solid #f0b429;
  border-left: 4px solid #e8890c;
  color: #7a4a00;
  font-size: 13px;
}
.wiz-pill {
  flex: none;
  padding: 2px 10px;
  border-radius: 999px;
  background: #e8890c;
  color: #fff;
  font-weight: 700;
  font-size: 12px;
  letter-spacing: 0.5px;
}
.wiz-banner-text {
  flex: 1;
  min-width: 0;
  line-height: 1.45;
}
.back-mine {
  flex: none;
  padding: 7px 16px;
  border: none;
  border-radius: 999px;
  background: #1677ff;
  color: #fff;
  font-size: 13px;
  font-weight: 700;
  cursor: pointer;
  box-shadow: 0 2px 8px rgba(22, 119, 255, 0.4);
  white-space: nowrap;
}
.back-mine:hover {
  background: #0b5fd8;
}
.back-mine:active {
  transform: translateY(1px);
}
/* 深色主题：横幅与按钮同样保持高对比 */
:global(.theme-dark) .wiz-banner {
  background: #3a2c12;
  border-bottom-color: #7a5a1e;
  color: #ffd79a;
}
@media (prefers-color-scheme: dark) {
  :global(.theme-system) .wiz-banner {
    background: #3a2c12;
    border-bottom-color: #7a5a1e;
    color: #ffd79a;
  }
}
.empty-err {
  margin: 0;
  padding: 8px 14px;
  max-width: 640px;
  border-radius: 8px;
  background: rgba(192, 57, 43, 0.08);
  border: 1px solid rgba(192, 57, 43, 0.35);
  color: var(--danger);
  font-size: 12.5px;
  line-height: 1.5;
  word-break: break-all;
  white-space: pre-wrap;
}
.path-code {
  padding: 1px 6px;
  border-radius: 4px;
  background: var(--bg-3);
  font-size: 12.5px;
  word-break: break-all;
}
.empty-sub.small {
  font-size: 12.5px;
  max-width: 560px;
}
/* 未保存 / 写操作结果提示（顶栏） */
.op-toast {
  font-size: 12px;
  color: #2e7d32;
  background: rgba(46, 125, 50, 0.12);
  border: 1px solid rgba(46, 125, 50, 0.35);
  border-radius: 999px;
  padding: 2px 10px;
  max-width: 420px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* 空态页 */
.empty-page {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  padding: 40px;
}
.empty-page h2 {
  margin: 0;
  font-size: 22px;
}
.empty-sub {
  margin: 0 0 10px;
  color: var(--text-2);
  font-size: 14px;
  text-align: center;
  max-width: 480px;
}
.empty-actions {
  display: flex;
  gap: 16px;
}
.big-btn {
  padding: 14px 28px;
  font-size: 15px;
  border-radius: 10px;
  cursor: pointer;
}
/* 顶栏菜单 */
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
.menu-pop.sub {
  position: relative;
  top: 0;
  right: 0;
  margin: 2px 0 2px 12px;
  box-shadow: none;
  border-left: 2px solid var(--border);
  border-radius: 0 8px 8px 0;
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
.menu-pop a.disabled {
  color: var(--text-2);
  opacity: 0.5;
  cursor: not-allowed;
}
.menu-pop a.disabled:hover {
  background: transparent;
}
.menu-sep {
  height: 1px;
  background: var(--border);
  margin: 4px 2px;
}
</style>
