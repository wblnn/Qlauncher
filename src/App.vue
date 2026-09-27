<script setup>
import { ref, onMounted, onUnmounted } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'


const games = ref([])
const loading = ref(false)
const runningGames = ref({})
const remotes = ref({})
const progress = ref({})
const officialInfo = ref({})
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

function pct(gameId) {
  const p = progress.value[gameId]
  if (!p || !p.total) return 0
  return Math.min(100, (p.downloaded / p.total) * 100)
}

// P0：只有 fresh(未安装) 或 ahead(有更新) 才允许下载
function canDownload(game) {
  const r = remotes.value[game.id]
  if (!r) return false
  return r.version_relation === 'fresh' || r.version_relation === 'ahead'
}

// 按钮文案：差分可用时显示差分大小，否则整包大小
function downloadLabel(game) {
  const r = remotes.value[game.id]
  if (!r) return '⬇ 下载'
  if (r.version_relation === 'ahead' && r.patch_from && r.patch_from === game.local_version) {
    return `⬇ 差分 ${fmtBytes(r.patch_size)}`
  }
  return `⬇ 整包 ${fmtBytes(r.package_size)}`
}

function relationTip(game) {
  const r = remotes.value[game.id]
  if (!r) return ''
  if (r.version_relation === 'equal') return '｜✅ 已是最新'
  if (r.version_relation === 'behind') return `｜⚠️ 接口整包滞后(本地 v${r.local_version} 更新)，禁下载`
  return ''
}

async function loadGames() {
  loading.value = true
  const baseGames = [
    { id: 'test_notepad', name: '🛠️ 测试：记事本', status: '已安装', installed: true, platform_level: 'Full', launcher_uri: null }
  ]
  try {
    const installedGames = await invoke('get_installed_games')
    games.value = [...baseGames, ...installedGames.map(g => ({
      ...g,
      status: g.installed ? '已安装' : '未安装'
    }))]
    await loadOfficialInfo()
  } catch (error) {
    console.error('后端调用失败:', error)
  }
  loading.value = false
}

async function updateRunningStatus() {
  try { runningGames.value = await invoke('get_running_games') } catch (e) {}
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

async function startDownload(gameId) {
  const dest = await open({ directory: true, multiple: false, title: '选择下载保存目录' })
  if (!dest) return
  const game = games.value.find(g => g.id === gameId)
  const r = remotes.value[gameId]
  const isPatch = r && r.patch_from && r.patch_from === game?.local_version
  const warn = isPatch
    ? `差分更新约 ${fmtBytes(r.patch_size)}，确认开始？`
    : `整包下载约 ${fmtBytes(r?.package_size || 0)}，确认硬盘空间足够！`
  if (!confirm(warn)) return
  try {
    progress.value[gameId] = { downloaded: 0, total: isPatch ? (r?.patch_size || 0) : (r?.package_size || 0), status: 'downloading' }
    await invoke('start_download', { gameId, dest, usePatch: isPatch })
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
    if (p.status === 'downloading') progress.value[p.game_id] = p
    else if (p.status.startsWith('done_test:')) {
      delete progress.value[p.game_id]
      alert('🧪 保命线生效！已安全截断，硬盘安全。文件在：' + p.status.slice(10))
    }
    else if (p.status.startsWith('done:')) { delete progress.value[p.game_id]; alert('下载完成：' + p.status.slice(5)) }
    else { delete progress.value[p.game_id]; alert('下载结束：' + p.status.replace('error:', '')) }
  })
})

async function loadOfficialInfo() {
  const ids = games.value.filter(g => g.id !== 'test_notepad').map(g => g.id)
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
  try {
    const plan = await invoke('apply_patch', { gameId, patchDir: dir, dryRun: true })
    if (!confirm('预演计划:\n' + plan + '\n\n确认执行真实合成？\n(原地升级，不可回滚；失败可用官启"修复")')) return
    const msg = await invoke('apply_patch', { gameId, patchDir: dir, dryRun: false })
    alert(msg)
    await loadGames()
  } catch (e) { alert('应用差分失败: ' + e) }
}

onUnmounted(() => { if (timer) clearInterval(timer) })
</script>

<template>
  <div class="container">
    <header>
      <h1>🎮 模块化游戏聚合器 (L0-L3)</h1>
      <button class="action-btn mini info" @click="probeApi">🔬 API 自检</button>
      
    </header>
    <main>
      <div v-if="loading">加载中...</div>
      <div v-else class="game-list">
        <div v-for="game in games" :key="game.id" class="game-card">
          <div class="game-icon">
            {{ game.platform_level === 'Aggregate' ? '📦' : game.platform_level === 'DirectLaunch' ? '🚀' : '💎' }}
          </div>
          <div class="game-info">
            <h3>{{ game.name }}</h3>
            <span class="status">
              <span class="level-badge" :class="game.platform_level === 'Aggregate' ? 'l0' : game.platform_level === 'DirectLaunch' ? 'l1' : 'l3'">
                {{ game.platform_level }}
              </span>
              {{ game.status }}
              <template v-if="game.local_version">｜本地 v{{ game.local_version }}</template>
              <template v-if="remotes[game.id]">｜远程 v{{ remotes[game.id].latest_version }}</template>
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
              <button v-if="game.status === '已安装'" class="action-btn l3" :disabled="runningGames[game.id]" @click.stop="launchGame(game.id, false)">
                {{ runningGames[game.id] ? '运行中...' : '▶ 启动' }}
              </button>
              <button v-else class="action-btn bind" @click.stop="bindGame(game.id)">📁 绑定目录</button>
              <button v-if="game.id !== 'test_notepad'" class="action-btn mini info" @click.stop="checkRemote(game.id)">📡 查版本</button>
              <button v-if="canDownload(game) && !progress[game.id]" class="action-btn mini dl" @click.stop="startDownload(game.id)">{{ downloadLabel(game) }}</button>
              <button v-if="progress[game.id]" class="action-btn mini cancel" @click.stop="cancelDownload(game.id)">✖ 取消</button>
              <button v-if="game.status === '已安装' && game.platform === 'mihoyo'" class="action-btn mini" @click.stop="applyPatch(game.id)">🔧 应用差分</button>
            </template>
          </div>
          <div v-if="progress[game.id]" class="progress-wrap">           
            <div class="progress-stats">
              <span class="size">{{ fmtBytes(progress[game.id].downloaded) }} / {{ fmtBytes(progress[game.id].total) }}</span>
              <div class="progress-bar"><div class="progress-fill" :style="{ width: pct(game.id) + '%' }"></div></div>
              <span class="speed">⚡ {{ fmtSpeed(progress[game.id].speed) }}</span>
              <span class="eta">⏳ {{ fmtEta(progress[game.id].eta_seconds) }}</span>
            </div>
          </div>
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
.progress-wrap { width: 100%; display: flex; flex-direction: column; gap: 6px; margin-top: 10px; }
.progress-bar { width: 30%; height: 8px; background: #333; border-radius: 4px; overflow: hidden; }
.progress-fill { height: 100%; background: #4caf50; transition: width 0.3s; }
.progress-stats { display: flex; justify-content: space-between; font-size: 12px; color: #aaa; width: 100%; }
.progress-stats .speed { color: #4a9eff; font-weight: bold; }
.progress-stats .eta { color: #ffb74d; }
</style>