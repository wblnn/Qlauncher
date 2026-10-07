<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'

// ===== 区服：同一个游戏只显示一张卡，"国服 / 国际服"作为可选项 =====
// 后端按 (游戏, 区服) 各给一个 id，前端把成对的合成一张卡，切换时用对应的 id 调后端。
const REGION_PAIRS = {
  genshin: { cn: 'genshin', global: 'genshin_global' },
  starrail: { cn: 'starrail', global: 'starrail_global' },
  zenless: { cn: 'zenless', global: 'zenless_global' },
  wuthering_waves: { cn: 'wuthering_waves', global: 'wuthering_waves_global' },
  pgr: { cn: 'pgr' }
}
const REGION_LABEL = { cn: '国服', global: '国际服' }
// 每个"基名"当前选中的区服，记住上次选择
const regionSel = ref(JSON.parse(localStorage.getItem('qlauncher_regions') || '{}'))
function baseOf(id) {
  for (const base of Object.keys(REGION_PAIRS)) {
    const pair = REGION_PAIRS[base]
    if (Object.values(pair).includes(id)) return base
  }
  return id
}
function setRegion(base, region) {
  regionSel.value = { ...regionSel.value, [base]: region }
  localStorage.setItem('qlauncher_regions', JSON.stringify(regionSel.value))
}
const rawGames = ref([])
// 合并后的卡片列表：每张卡用"当前区服对应的那个后端 id"，其余字段原样
const games = computed(() => {
  const byId = {}
  for (const g of rawGames.value) byId[g.id] = g
  const out = []
  const seen = new Set()
  for (const g of rawGames.value) {
    const base = baseOf(g.id)
    if (seen.has(base)) continue
    seen.add(base)
    const pair = REGION_PAIRS[base]
    const regions = pair ? Object.keys(pair).filter(r => byId[pair[r]]) : []
    const region = regions.includes(regionSel.value[base]) ? regionSel.value[base] : (regions[0] || 'cn')
    const active = pair && pair[region] && byId[pair[region]] ? byId[pair[region]] : g
    out.push({ ...active, base_id: base, region, regions })
  }
  return out
})
const loading = ref(false)
const runningGames = ref({})
const remotes = ref({})
const progress = ref({})
const officialInfo = ref({})
const patchProgress = ref({})
const zombies = ref([])
const zombieScanned = ref(false)
const patchState = ref({})
const patchBusy = ref('')
const patchBusyStart = ref(0)
const nowTick = ref(Date.now())
let timer = null

function fmtBytes(n) {
  if (!n) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB']
  let i = 0, v = n
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++ }
  return v.toFixed(1) + ' ' + units[i]
}

function fmtSpeed(bps) {
  if (!bps) return '0 B/s'
  const units = ['B/s', 'KB/s', 'MB/s', 'GB/s']
  let i = 0, v = bps
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++ }
  return v.toFixed(1) + ' ' + units[i]
}

function fmtEta(sec) {
  if (sec === null || sec === undefined) return '计算中'
  if (sec === 0) return '即将完成'
  if (sec < 60) return sec + 's'
  if (sec < 3600) return Math.floor(sec / 60) + 'm ' + (sec % 60) + 's'
  return Math.floor(sec / 3600) + 'h ' + Math.floor((sec % 3600) / 60) + 'm'
}

const STEP_ORDER = ['preflight', 'synthesize', 'replace', 'delete', 'verify', 'commit']
const curPhase = computed(() => (patchState.value[patchBusy.value] || {}).phase || '')

function stepLabel(s) {
  return { preflight: '预检', synthesize: '合成', replace: '替换', delete: '删除', verify: '校验', commit: '提交' }[s] || s
}
function stepDone(s) {
  const st = patchState.value[patchBusy.value]
  if (!st) return false
  const cur = st.phase === 'done' ? 'commit' : st.phase
  const i = STEP_ORDER.indexOf(cur), j = STEP_ORDER.indexOf(s)
  return i > -1 && j > -1 && j < i
}
function fmtElapsed() {
  const s = Math.max(0, Math.floor((nowTick.value - patchBusyStart.value) / 1000))
  return s < 60 ? s + 's' : Math.floor(s / 60) + 'm ' + (s % 60) + 's'
}

function pct(gameId) {
  const p = progress.value[gameId]
  if (!p || !p.total) return 0
  return Math.min(100, (p.downloaded / p.total) * 100)
}

// P0：只有 fresh(未安装) 或 ahead(有更新) 才允许下载
// 🌟 新增：原神/星铁的 announced 状态也允许下载（走 Sophon 引擎）
function canDownload(game) {
  const r = remotes.value[game.id]
  if (!r) return false
  if (r.version_relation === 'fresh' || r.version_relation === 'ahead') return true
  if (r.version_relation === 'announced' && (game.id === 'genshin' || game.id === 'starrail')) return true
  return false
}

// 按钮文案：差分可用时显示差分大小，否则整包大小
function downloadLabel(game) {
  const r = remotes.value[game.id]
  if (!r) return '⬇ 下载'
  
  // 🌟 原神/星铁走 Sophon 引擎
  if (r.version_relation === 'announced' && (game.id === 'genshin' || game.id === 'starrail')) {
    return `⬇ 增量更新 (Sophon)`
  }
  
  if (r.version_relation === 'announced') return `⬇ 旧版整包 v${r.latest_version}`
  if (r.version_relation === 'ahead' && r.patch_from && r.patch_from === game.local_version) {
    return `⬇ 差分 ${fmtBytes(r.patch_size)}`
  }
  return `⬇ 整包 ${fmtBytes(r.package_size)}`
}

function relationTip(game) {
  const r = remotes.value[game.id]
  if (!r) return ''
  const at = r.checked_at ? `（检查于 ${new Date(r.checked_at * 1000).toLocaleTimeString().slice(0, 5)}·来源 ${r.version_source || '整包'}）` : ''
  
  // 🌟 原神/星铁 announced 提示
  if (r.version_relation === 'announced' && (game.id === 'genshin' || game.id === 'starrail')) {
    return `｜🚀 官宣 v${r.announced_version}，已启用 Sophon 增量引擎` + at
  }
  
  if (r.version_relation === 'equal') return '｜✅ 已是最新' + at
  if (r.version_relation === 'behind') return `｜⚠️ 接口整包滞后(本地 v${r.local_version} 更新)，禁下载` + at
  if (r.version_relation === 'announced') return `｜⚠️ 官宣 v${r.announced_version}（推断），接口整包只到 v${r.latest_version} → 请用『🩺 官方更新』` + at
  return at
}

async function loadGames() {
  loading.value = true
  const baseGames = [
    { id: 'test_notepad', name: '🛠️ 测试：记事本', status: '已安装', installed: true, platform_level: 'Full', launcher_uri: null }
  ]
  try {
    const installedGames = await invoke('get_installed_games')
    rawGames.value = [...baseGames, ...installedGames.map(g => ({
      ...g,
      status: g.installed ? '已安装' : '未安装'
    }))]
    await loadOfficialInfo()
    await loadPatchState()
  } catch (error) {
    console.error('后端调用失败:', error)
  }
  loading.value = false
}

async function updateRunningStatus() {
  try { runningGames.value = await invoke('get_running_games') } catch (e) {}
  nowTick.value = Date.now()
  if (patchBusy.value) {
    try {
      const st = await invoke('patch_status', { gameId: patchBusy.value })
      if (st) patchState.value[patchBusy.value] = st
    } catch (e) {}
  }
}

async function launchGame(gameId, useOfficial = false) {
  try {
    console.log(await invoke('launch_game', { id: gameId, useOfficial }))
  } catch (e) {
    alert('操作失败: ' + e)
  }
}

async function bindGame(gameId) {
  const dir = await open({ directory: true, multiple: false, title: '选择游戏安装文件夹' })
  if (!dir) return
  try {
    await invoke('bind_game', { id: gameId, dir })
    await loadGames()
  } catch (e) { alert('绑定失败: ' + e) }
}

async function checkRemote(gameId) {
  try { remotes.value[gameId] = await invoke('check_remote', { gameId }) }
  catch (e) { alert('获取远程信息失败: ' + e) }
}

async function probeApi() {
  try {
    const report = await invoke('probe_api')
    console.log('===== API 自检报告 =====')
    report.forEach(line => console.log(line))
    alert('自检完成！按 Ctrl+Shift+I 打开控制台查看报告，找 retcode:0 的行')
  } catch (e) { alert('自检失败: ' + e) }
}

async function startDownload(gameId, forceFull = false) {
  const game = games.value.find(g => g.id === gameId)
  const r = remotes.value[gameId]
  // 🌟 原神/星铁走 Sophon：写的是已绑定游戏目录，不需要（也不该）让用户再选下载目录
  const isSophon = (gameId === 'genshin' || gameId === 'starrail')
  let dest = ''
  if (isSophon) {
    if (!confirm(`🚀 ${game.name} 已启用 Sophon 增量更新引擎。\n\n会直接对账已安装目录，只下载差异文件块（不需要整包压缩包，也不需要选下载目录）。\n\n远程版本：v${r?.announced_version || r?.latest_version || '?'}\n\n确认开始更新？`)) return
  } else {
    dest = await open({ directory: true, multiple: false,
      title: game?.platform === 'kuro'
        ? '选择《鸣潮》安装根目录（会在里面生成 Wuthering Waves.exe / Client/…）'
        : '选择下载保存目录' })
    if (!dest) return
  }
  const isKuro = game?.platform === 'kuro'
  // ⚠️ 必须显式转布尔：r.patch_from 可能是 null，&& 链会把 null 原样传给后端（Tauri 会拒 null）
  const isPatch = !!(r && r.patch_from && r.patch_from === game?.local_version) && !forceFull && !isSophon
  let allowOld = false
  if (!isSophon) {
    if (r && r.version_relation === 'announced' && !forceFull) {
      if (!confirm(`⚠️ 官方已发布 v${r.announced_version}（来源：${r.version_source}），但接口整包只到 v${r.latest_version}。\n\n继续只会下载【旧版 v${r.latest_version}】，不会让你变成 v${r.announced_version}。\n要更新到最新版请用『🩺 官方更新』（拉起官方启动器）。\n\n仍要下载旧版整包吗？`)) return
      allowOld = true
    } else {
      const warn = isKuro
        ? (isPatch
            ? `《鸣潮》更新：只补本地对不上的文件，约 ${fmtBytes(r?.patch_size || 0)}（官方 md5 校验，直接落盘、无需解压）。\n\n确认继续？`
            : `《鸣潮》整包安装：需要下载约 ${fmtBytes(r?.package_size || 0)}（官方清单 v${r?.latest_version || '?'}，按文件裸传、直接落盘、无需解压）。\n\n⚠️ 目标盘要留出这么多空间。确认继续？`)
        : (isPatch
            ? `差分更新：下载约 ${fmtBytes(r.patch_size)}（另需临时空间解压），确认开始？`
            : `整包安装：下载约 ${fmtBytes(r?.package_size || 0)}，解压后安装约 ${fmtBytes(r?.install_size || 0)}。\n\n⚠️ 目标盘需要同时容纳两者（约 ${fmtBytes((r?.package_size || 0) + (r?.install_size || 0))}），确认继续？`)
      if (!confirm(warn)) return
    }
  }
  try {
    // Sophon 不预知总字节：给占位 0，靠 status 里的 chunking 显示进度
    const totalBytes = isSophon ? 0 : (isPatch ? (r?.patch_size || 0) : (r?.package_size || 0))
    progress.value[gameId] = { downloaded: 0, total: totalBytes, status: 'downloading' }
    await invoke('start_download', { gameId, dest, usePatch: isPatch, allowOld })
  } catch (e) { delete progress.value[gameId]; alert('启动下载失败: ' + e) }
}

async function cancelDownload(gameId) {
  try { await invoke('cancel_download', { gameId }) } catch (e) {}
}

onMounted(() => {
  loadGames()
  updateRunningStatus()
  timer = setInterval(updateRunningStatus, 2000)
  listen('download-progress', (event) => {
    const p = event.payload
    if (p.status === 'downloading' || p.status === 'repairing' || p.status === 'verifying' || p.status === 'extracting') {
      progress.value[p.game_id] = p
    } 
    // 🌟 新增：处理 Sophon chunking 状态
    else if (p.status.startsWith('chunking:')) {
      const match = p.status.match(/chunking:(\d+)\/(\d+)\|(.*)/)
      if (match) {
        progress.value[p.game_id] = {
          ...p,                                  // downloaded/total 保留字节数
          chunk_done: parseInt(match[1]),
          chunk_total: parseInt(match[2]),
          current_file: match[3],
          status: 'chunking',
        }
      } else {
        progress.value[p.game_id] = p
      }
    }
    else if (p.status.startsWith('done_test:')) {
      delete progress.value[p.game_id]
      alert('🧪 保命线生效！已安全截断，硬盘安全。文件在：' + p.status.slice(10))
    }
    else if (p.status.startsWith('done:')) { 
      delete progress.value[p.game_id]
      delete patchProgress.value[p.game_id]
      alert('下载/更新完成：' + p.status.slice(5)) 
      loadGames() // 🌟 完成后刷新游戏列表，更新本地版本号
    }
    else { 
      delete progress.value[p.game_id]
      alert('下载结束：' + p.status.replace('error:', '')) 
    }
  })
  listen('patch-progress', (event) => {
    const p = event.payload
    if (p.status === 'patching') {
      patchProgress.value[p.game_id] = p.current || `${p.done}/${p.total}`
    } else if (p.status.startsWith('done:')) {
      patchProgress.value[p.game_id] = ''
      delete progress.value[p.game_id]
      patchBusy.value = ''
      alert('完成：' + p.status.slice(5))
      loadGames()
    } else if (p.status.startsWith('error:')) {
      patchProgress.value[p.game_id] = ''
      delete progress.value[p.game_id]
      patchBusy.value = ''
      alert('失败：' + p.status.slice(6))
      loadPatchState()
    }
  })
  listen('verify-progress', (event) => {
    const p = event.payload
    patchProgress.value[p.game_id] = `${p.done}/${p.total} ${p.current}`
  })
})

async function loadOfficialInfo() {
  // 用 rawGames（所有区服都要拿到官启/官网信息），否则切区后按钮会消失
  const ids = rawGames.value.filter(g => g.id !== 'test_notepad').map(g => g.id)
  const res = await Promise.all(ids.map(id =>
    invoke('official_info', { gameId: id }).catch(() => null)
  ))
  ids.forEach((id, i) => { if (res[i]) officialInfo.value[id] = res[i] })
}

async function openOfficial(gameId, mode) {
  try { console.log(await invoke('open_official', { gameId, mode })) }
  catch (e) { alert('打开失败: ' + e) }
}

async function applyPatch(gameId) {
  const dir = await open({ directory: true, multiple: false, title: '选择差分包目录 (patch_x_y)' })
  if (!dir) return
  patchBusy.value = gameId
  patchBusyStart.value = Date.now()
  patchProgress.value[gameId] = '预检：进程占用 / 文件可写性 / 磁盘空间…'
  try {
    const plan = await invoke('apply_patch', { gameId, patchDir: dir, dryRun: true })
    if (!confirm('预演计划:\n' + plan + '\n\n确认执行真实合成？\n(可逆：提交点之前都能「▶ 继续更新」或「↩ 回滚」)')) {
      patchBusy.value = ''
      delete patchProgress.value[gameId]
      await loadPatchState()
      return
    }
    alert(await invoke('apply_patch', { gameId, patchDir: dir, dryRun: false }))
  } catch (e) {
    alert('应用差分失败: ' + e)
    patchBusy.value = ''
    delete patchProgress.value[gameId]
    await loadPatchState()
  }
}

async function killGame(gameId) {
  try {
    const msg = await invoke('kill_game', { gameId })
    console.log(msg)
    await updateRunningStatus()
  } catch (e) {
    alert('关闭失败: ' + e)
  }
}

async function scanZombies() {
  try {
    zombies.value = await invoke('list_zombie_games')
    zombieScanned.value = true
  } catch (e) { alert('扫描失败: ' + e) }
}

async function killZombie(pid) {
  try {
    console.log(await invoke('kill_process', { pid }))
    await scanZombies()
  } catch (e) { alert('杀死僵尸进程失败: ' + e) }
}

async function loadPatchState() {
  const ids = rawGames.value.filter(g => g.id !== 'test_notepad').map(g => g.id)
  const res = await Promise.all(ids.map(id => invoke('patch_status', { gameId: id }).catch(() => null)))
  ids.forEach((id, i) => { if (res[i]) patchState.value[id] = res[i]; else delete patchState.value[id] })
}

async function resumePatch(gameId) {
  if (!confirm('继续上次未完成的更新？\n从断点接着做，已完成的部分会自动跳过。')) return
  patchBusy.value = gameId
  patchBusyStart.value = Date.now()
  try { alert(await invoke('resume_patch', { gameId })) } catch (e) { patchBusy.value = ''; alert('继续失败: ' + e) }
}

async function rollbackPatch(gameId) {
  const st = patchState.value[gameId] || {}
  if (!confirm(`回滚到 ${st.from_version || '补丁前状态'}？\n会还原已改动的文件、删掉本次新增的文件、复原 config.ini。`)) return
  try {
    alert(await invoke('rollback_patch', { gameId }))
    await loadPatchState()
    await loadGames()
  } catch (e) { alert('回滚失败: ' + e) }
}

async function verifyRepair(gameId) {
  const game = games.value.find(g => g.id === gameId)
  const isKuro = game?.platform === 'kuro'
  const isSophon = (gameId === 'genshin' || gameId === 'starrail')
  // Sophon / 库洛：校验都是"拉官方清单对账"，没有快/深之分
  const deep = (isSophon || isKuro) ? true
    : !confirm('校验方式：\n\n【确定】快速校验 —— 只比文件大小，几秒出结果\n【取消】深度校验 —— 逐文件算 md5，慢（几十 GB 要几分钟）但更准\n\n（选哪个都会先给你一份报告，不会直接动手）')
  patchBusy.value = gameId
  patchBusyStart.value = Date.now()
  patchProgress.value[gameId] = isSophon ? 'Sophon 对账：拉官方分块清单…'
    : isKuro ? '库洛对账：拉官方清单…'
    : (deep ? '深度校验：逐文件算 md5…' : '快速校验：比对文件大小…')
  progress.value[gameId] = { downloaded: 0, total: 0, speed: 0, eta_seconds: null, status: 'verifying' }
  try {
    const r = await invoke('verify_game_files', { gameId, deep })
    patchBusy.value = ''
    delete patchProgress.value[gameId]
    delete progress.value[gameId]
    const head = `清单来源：${r.manifest}\n共 ${r.total} 项 → 正常 ${r.ok} ｜ 缺失 ${r.missing} ｜ 大小不符 ${r.size_bad} ｜ md5 不符 ${r.md5_bad}\n` +
      `需要补下：${fmtBytes(r.broken_bytes)}\n本地 v${r.local_version} ｜ 接口 v${r.remote_version} ｜ 版本${r.version_match ? '一致 ✅' : '不一致 ⚠️'}\n` +
      `散列地址：${r.res_list_url || (r.sophon_managed ? '（Sophon 分块体系 → 走分块补全）' : isKuro ? '（库洛官方清单 → 差异文件重下）' : '（无 → 无法单文件补全）')}`
    const list = r.sample.length ? `\n\n前 ${r.sample.length} 条：\n` + r.sample.join('\n') : ''
    const broken = r.missing + r.size_bad + r.md5_bad
    // Sophon 体系：不走逐文件 URL 补全（那会用旧整包地址把新版本文件降级），直接引导到分块更新
    if (r.sophon_managed) {
      if (broken === 0) { alert(head + '\n\n✅ 与官方分块清单完全一致。'); return }
      if (!confirm(head + list + `\n\n原神/星铁走 Sophon 分块体系：这 ${broken} 个差异文件要由分块引擎补（逐文件补全会用旧整包地址把文件搞乱，已禁用）。\n\n现在就开始分块更新吗？`)) return
      return startDownload(gameId)
    }
    // 库洛（鸣潮）：官方清单对账 → 差异文件重下（分块续传 + 官方 md5 校验，作用在已绑定目录）
    if (isKuro) {
      if (broken === 0) { alert(head + '\n\n✅ 与官方清单完全一致。'); return }
      if (!confirm(head + list + `\n\n《鸣潮》按官方清单补：这 ${broken} 个文件会重新下载（逐块 + 整文件官方 md5 校验，直接落到已绑定目录）。\n\n现在开始吗？`)) return
      return kuroRepair(gameId)
    }
    if (broken === 0) { alert(head + '\n\n✅ 没有发现问题文件。'); return }
    if (!r.res_list_url) { alert(head + list + '\n\n⚠️ 该版本没有散列文件地址，无法按单文件补全 —— 请用『🩺 官方修复』或整包。'); return }
    if (!r.version_match) { alert(head + list + '\n\n⚠️ 本地版本与接口整包版本不一致：单文件地址指向接口那个版本，直接补可能把文件搞乱，已阻止。\n→ 先用官方启动器对齐版本，或走整包。'); return }
    if (!confirm(head + list + `\n\n确认按清单【只补这 ${broken} 个文件】？`)) return
    patchBusy.value = gameId
    patchBusyStart.value = Date.now()
    alert(await invoke('repair_game_files', { gameId }))
  } catch (e) {
    patchBusy.value = ''
    delete patchProgress.value[gameId]
    delete progress.value[gameId]
    const msg = String(e)
    alert(msg.includes('已取消') ? '⏹ ' + msg : '校验/修复失败: ' + msg)
  }
}

// 库洛修复：dest 传空 → 后端用「已绑定目录」，不再弹选目录
async function kuroRepair(gameId) {
  patchBusy.value = gameId
  patchBusyStart.value = Date.now()
  progress.value[gameId] = { downloaded: 0, total: 0, speed: 0, eta_seconds: null, status: 'downloading' }
  try {
    await invoke('start_download', { gameId, dest: '', usePatch: false, allowOld: false })
  } catch (e) { delete progress.value[gameId]; alert('启动修复失败: ' + e) }
}

async function officialRepair(gameId) {
  try {
    alert(await invoke('open_official', { gameId, mode: 'auto' }) +
      '\n\n→ 在官方启动器里对该游戏点「修复」/「更新」，它会按官方清单校验并补齐缺失文件。')
  } catch (e) { alert('拉起官方启动器失败: ' + e) }
}

async function downloadFull(gameId) {
  // 原神/星铁没有可下的整包（接口整包停在旧版本），这条路对它们只会误导
  if (gameId === 'genshin' || gameId === 'starrail') {
    alert('原神/星铁已改为 Sophon 分块体系：接口整包停在旧版本，没有可下载的"完整整包"。\n\n· 分块更新只作用于【已绑定的游戏目录】（会自动对账补齐缺失/损坏文件）\n· 想换安装目录：用官方启动器的「添加已有游戏」指向新目录')
    return
  }
  if (!confirm('把【完整整包】下载到一个新目录？\n不会改动当前安装；下完后可用官方启动器的「添加已有游戏」指向新目录。')) return
  await startDownload(gameId, true)
}

onUnmounted(() => { if (timer) clearInterval(timer) })
</script>

<template>
  <div class="container">
    <header>
      <h1>🎮 模块化游戏聚合器 (L0-L3)</h1>
      <button class="action-btn mini info" @click="probeApi">🔬 API 自检</button>
      <button class="action-btn mini" @click="scanZombies">🧟 僵尸扫描</button>
    </header>
    <div v-if="zombies.length" class="zombie-panel">
      <h4>🧟 检测到 {{ zombies.length }} 个残留进程（已死但占坑）</h4>
      <div v-for="z in zombies" :key="z.pid" class="zombie-row">
        <span>{{ z.game_id }} · PID {{ z.pid }} · 工作集 {{ fmtBytes(z.memory) }}</span>
        <button class="action-btn mini cancel" @click="killZombie(z.pid)">清理</button>
      </div>
    </div>
    <div v-else-if="zombieScanned" class="zombie-panel">✅ 干净，无残留进程</div>
    <div v-if="Object.keys(patchState).length" class="patch-panel">
      <h4>⚠️ 有未完成的更新（可续跑 / 可回滚）</h4>
      <div v-for="(st, gid) in patchState" :key="gid" class="patch-row">
        <span>
          <b>{{ gid }}</b> · 阶段 {{ st.phase }} · 合成 {{ st.diff_done }}/{{ st.diff_total }} · 替换 {{ st.replace_done }}/{{ st.replace_total }}
          · 已备份 {{ (st.backups || []).length }} 个<template v-if="st.from_version"> （{{ st.from_version }} → {{ st.to_version }}）</template>
          <template v-if="st.message"><br><span class="patch-msg">{{ st.message }}</span></template>
        </span>
        <span class="patch-actions">
          <button class="action-btn mini dl" @click="resumePatch(gid)">▶ 继续更新</button>
          <button class="action-btn mini cancel" @click="rollbackPatch(gid)">↩ 回滚</button>
          <button class="action-btn mini official" @click="officialRepair(gid)">🩺 官方修复</button>
          <button class="action-btn mini" @click="downloadFull(gid)">⬇ 整包到新目录</button>
        </span>
      </div>
      <div class="patch-note">顺序：继续 → 回滚 → 官方修复 → 整包到新目录。续跑和回滚都只依赖本地已下好的补丁与备份，不需要重新下载。</div>
    </div>
    <div v-if="patchBusy" class="run-panel">
      <h4>🔧 正在更新 <b>{{ patchBusy }}</b> · 已用 {{ fmtElapsed() }}</h4>
      <div class="steps">
        <span v-for="s in STEP_ORDER" :key="s" class="step"
              :class="{ on: curPhase === s, done: stepDone(s) }">{{ stepLabel(s) }}</span>
      </div>
      <div class="run-current">{{ patchProgress[patchBusy] || '准备中…' }}</div>
      <div class="run-hint">任务在后台线程执行，窗口不会卡；可以最小化。中途关掉程序也不要紧 —— 重启后可用『▶ 继续更新』或『↩ 回滚』兜住。</div>
    </div>
    <main>
      <div v-if="loading">加载中...</div>
      <div v-else class="game-list">
        <div v-for="game in games" :key="game.id" class="game-card">
          <div class="game-icon">
            {{ game.platform_level === 'Aggregate' ? '📦' : game.platform_level === 'DirectLaunch' ? '🚀' : '💎' }}
          </div>
          <div class="game-info">
            <h3>{{ game.name }}<span v-if="game.regions && game.regions.length > 1" class="region-switch">
              <button v-for="rg in game.regions" :key="rg"
                      :class="{ on: game.region === rg }"
                      :title="'切换到' + REGION_LABEL[rg]"
                      @click.stop="setRegion(game.base_id, rg)">{{ REGION_LABEL[rg] }}</button>
            </span></h3>
            <span class="status">
              <span class="level-badge" :class="game.platform_level === 'Aggregate' ? 'l0' : game.platform_level === 'DirectLaunch' ? 'l1' : 'l3'">
                {{ game.platform_level }}
              </span>
              {{ game.status }}
              <template v-if="game.local_version">｜本地 v{{ game.local_version }}</template>
              <template v-if="remotes[game.id]">｜远程 v{{ remotes[game.id].latest_version }}<template v-if="remotes[game.id].announced_version && remotes[game.id].announced_version !== remotes[game.id].latest_version">｜官宣 v{{ remotes[game.id].announced_version }}（推断）</template></template>
              <span class="tip">{{ relationTip(game) }}</span>
            </span>
          </div>
          <div class="actions">
            <button v-if="officialInfo[game.id]" class="action-btn official"
                    :title="officialInfo[game.id].local ? '本地官启: ' + officialInfo[game.id].local : '未识别到本地官启，将打开兜底地址'"
                    @click.stop="openOfficial(game.id, 'auto')">📦 官启</button>
            <button v-if="officialInfo[game.id] && officialInfo[game.id].web" class="action-btn web"
                    :title="officialInfo[game.id].web"
                    @click.stop="openOfficial(game.id, 'web')">🌐 官网</button>
            
            <template v-if="game.platform_level === 'DirectLaunch'">
              <button class="action-btn l1" @click.stop="launchGame(game.id, false)">▶ 直启 (协议)</button>
            </template>
            <template v-if="game.platform_level === 'Full' || game.platform_level === 'Download'">
              <template v-if="game.status === '已安装'">
                <button v-if="runningGames[game.id]" class="action-btn kill" @click.stop="killGame(game.id)">
                  ⏹ 强制关闭
                </button>
                <button v-else class="action-btn l3" @click.stop="launchGame(game.id, false)">
                  ▶ 启动
                </button>
              </template>
              <button v-else class="action-btn bind" @click.stop="bindGame(game.id)">📁 绑定目录</button>
              <button v-if="game.id !== 'test_notepad'" class="action-btn mini info" @click.stop="checkRemote(game.id)">📡 查版本</button>
              <button v-if="canDownload(game) && !progress[game.id]" class="action-btn mini dl" @click.stop="startDownload(game.id)">{{ downloadLabel(game) }}</button>
              <button v-if="progress[game.id]" class="action-btn mini cancel" @click.stop="cancelDownload(game.id)">✖ 取消</button>
              <button v-if="game.status === '已安装' && game.platform === 'mihoyo'" class="action-btn mini" @click.stop="applyPatch(game.id)">🔧 应用差分</button>
              <button v-if="game.status === '已安装' && (game.platform === 'mihoyo' || game.platform === 'kuro')" class="action-btn mini" @click.stop="verifyRepair(game.id)">🩹 校验修复</button>
            </template>
          </div>
          <div v-if="progress[game.id]" class="progress-wrap">           
            <div class="progress-stats">
              <span class="size">
                <template v-if="progress[game.id].status === 'extracting'">📦 解压中 </template>
                <template v-else-if="progress[game.id].status === 'chunking'">
                  🧩 组装中: {{ progress[game.id].current_file }}（块 {{ progress[game.id].chunk_done || 0 }}/{{ progress[game.id].chunk_total || 0 }}）
                </template>
                <template v-else>
                  {{ fmtBytes(progress[game.id].downloaded) }} / {{ fmtBytes(progress[game.id].total) }}（{{ pct(game.id).toFixed(1) }}%）
                </template>
              </span>
              <div class="progress-bar"><div class="progress-fill" :style="{ width: pct(game.id) + '%' }"></div></div>
              <span class="speed">⚡ {{ fmtSpeed(progress[game.id].speed) }}</span>
              <span class="eta">⏳ {{ fmtEta(progress[game.id].eta_seconds) }}</span>
            </div>
          </div>
          <div v-if="patchProgress[game.id]" class="patch-progress">🔧 {{ patchProgress[game.id] }}</div>
        </div>
      </div>
    </main>

  </div>
</template>

<style scoped>
.container { max-width: 900px; margin: 0 auto; padding: 20px; font-family: Arial, sans-serif; background: #0f0f1a; min-height: 100vh; color: #fff; }
header { text-align: center; margin-bottom: 30px; padding-bottom: 20px; border-bottom: 2px solid #4a9eff; }
h1 { color: #4a9eff; margin: 0 0 10px 0; }
.game-list { display: flex; flex-direction: column; gap: 15px; }
.game-card { display: flex; align-items: center; gap: 15px; padding: 15px; background: #1a1a2e; border-radius: 10px; flex-wrap: wrap; }
.game-icon { font-size: 36px; }
.game-info { flex: 1; min-width: 200px; }
.game-info h3 { margin: 0 0 5px 0; color: #fff; }
.status { color: #888; font-size: 13px; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.tip { color: #ffb74d; font-size: 12px; }
.level-badge { padding: 2px 6px; border-radius: 4px; font-size: 11px; font-weight: bold; color: #fff; }
.l0 { background: #607d8b; }
.l1 { background: #ff9800; }
.l3 { background: #4caf50; }
.actions { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.action-btn { padding: 8px 16px; color: white; border: none; border-radius: 5px; cursor: pointer; font-weight: bold; font-size: 13px; }
.action-btn:hover:not(:disabled) { filter: brightness(1.2); }
.action-btn:disabled { opacity: 0.6; cursor: not-allowed; }
.action-btn.l0 { background: #607d8b; }
.action-btn.l1 { background: #ff9800; }
.action-btn.l3 { background: #4caf50; }
.action-btn.bind { background: #2196f3; }
.action-btn.mini { padding: 6px 10px; font-size: 12px; background: #333; color: #ccc; }
.action-btn.info { background: #00bcd4; color: #fff; }
.action-btn.dl { background: #2e7d32; color: #fff; }
.action-btn.cancel { background: #c62828; color: #fff; }
.action-btn.official { background: #455a64; }
.action-btn.web { background: #6a4c93; }
.action-btn.kill { background: #d32f2f; }
.action-btn.kill:hover { background: #b71c1c; }
.progress-wrap { width: 100%; display: flex; flex-direction: column; gap: 6px; margin-top: 10px; }
.progress-bar { width: 45%; height: 8px; background: #333; border-radius: 4px; overflow: hidden; }
.region-switch { margin-left: 10px; display: inline-flex; gap: 2px; vertical-align: middle; }
.region-switch button {
  font-size: 12px; padding: 1px 8px; border-radius: 10px; cursor: pointer;
  background: #2a2a33; color: #aaa; border: 1px solid #3a3a44;
}
.region-switch button.on { background: #2d6cdf; color: #fff; border-color: #2d6cdf; }
.progress-fill { height: 100%; background: #4caf50; transition: width 0.3s; }
.progress-stats { display: flex; justify-content: space-between; font-size: 12px; color: #aaa; width: 100%; }
.progress-stats .speed { color: #4a9eff; font-weight: bold; }
.progress-stats .eta { color: #ffb74d; }
.zombie-panel { background: #2a1a1a; border: 1px solid #c62828; border-radius: 8px; padding: 12px; margin-bottom: 20px; }
.zombie-panel h4 { margin: 0 0 10px 0; color: #ff8a80; font-size: 14px; }
.zombie-row { display: flex; justify-content: space-between; align-items: center; padding: 6px 0; border-top: 1px dashed #553333; font-size: 13px; color: #ddd; }
.patch-panel { background: #2a2418; border: 1px solid #ffb74d; border-radius: 8px; padding: 12px; margin-bottom: 20px; }
.patch-panel h4 { margin: 0 0 10px 0; color: #ffb74d; font-size: 14px; }
.patch-row { display: flex; justify-content: space-between; align-items: center; gap: 10px; padding: 8px 0; border-top: 1px dashed #554433; font-size: 13px; color: #ddd; flex-wrap: wrap; }
.patch-actions { display: flex; gap: 6px; flex-wrap: wrap; }
.patch-msg { color: #ff8a80; font-size: 12px; }
.patch-note { margin-top: 10px; font-size: 12px; color: #999; }
.patch-progress { margin-top: 8px; font-size: 12px; color: #ffb74d; }
.run-panel { background: #182430; border: 1px solid #4a9eff; border-radius: 8px; padding: 12px; margin-bottom: 20px; }
.run-panel h4 { margin: 0 0 10px 0; color: #4a9eff; font-size: 14px; }
.steps { display: flex; gap: 6px; flex-wrap: wrap; margin-bottom: 8px; }
.step { padding: 3px 8px; border-radius: 10px; font-size: 12px; background: #24333f; color: #8899a6; }
.step.done { background: #2e7d32; color: #fff; }
.step.on { background: #4a9eff; color: #fff; font-weight: bold; }
.run-current { font-size: 12px; color: #ddd; word-break: break-all; }
.run-hint { margin-top: 8px; font-size: 12px; color: #999; }
</style>