# qlauncher

一个基于 **Tauri 2 + Vue 3 + TypeScript** 的跨平台游戏启动器 / 下载管理器。它把多个发行平台（米哈游、Steam、Riot）统一进一个界面：自动扫描本机已安装的游戏，检查远程版本，支持整包与差分（补丁）下载、限速与低资源占用流式下载，并一键启动游戏。

> 当前后端大量依赖 Windows 注册表（`winreg`），主要在 **Windows** 上开发与验证。

📖 English documentation: [README.en.md](./README.en.md)

## 功能特性

- **多平台聚合**：以统一的 `GamePlatform` trait 抽象各平台能力，按能力分级展示
  - `Aggregate`（仅聚合入口，如 Riot）
  - `DirectLaunch`（可直接启动，如 Steam）
  - `Download` / `Full`（可查版本 + 下载 + 启动，如米哈游）
- **已安装游戏扫描**：读取 Windows 卸载注册表项 + 平台自有目录规则，识别安装路径、exe 与本地版本号；云游戏入口自动过滤
- **手动绑定**：注册表扫不到时，可通过目录选择对话框手动绑定游戏目录（exe 候选名由平台自行声明）
- **官方启动器识别**：从注册表定位本机官方启动器（HoYoPlay / Steam / Riot Client），一键"官启"拉起；识别不到时自动打开官网兜底，也可强制只开"官网"
- **版本守卫（P0）**：语义化版本比较，只有 `fresh`（未安装）或 `ahead`（远程有更新）才允许下载；接口整包滞后于本地版本时标记 `behind` 并禁止下载，避免"越更新越旧"
- **整包 / 差分下载**：优先使用差分包（显示差分体积，本地版本与差分源不匹配时拒绝下载），无差分则回退整包；整包与差分均支持多分卷（`parts`），差分 `.7z` 下完自动解压到 `extracted/`
- **低占用流式下载**：可配置流式缓冲区（`download_buffer_mb`，越大 CPU 占用越低）、基于让出 CPU 时间片的平滑限速（`download_speed_limit_mbps`）、磁盘占用保护线（`download_limit_mb`）
- **断点友好**：重启下载时自动跳过大小已匹配的已完成分卷，无需从头重下
- **实时进度**：约每 500ms 回传一次已下载 / 总量 / 瞬时速度 / ETA，随时取消并清理半截文件
- **运行状态检测**：基于 `sysinfo` 轮询进程，界面上区分"运行中 / 已安装 / 未安装"
- **接口自检**：内置 🔬 自检命令，输出 HYP 接口连通性、`launcher_id` 来源、保命线与包信息解析结果，便于排查接口失效问题

## 技术栈

| 层 | 技术 |
| --- | --- |
| 前端 | Vue 3 `<script setup>`、TypeScript、Vite |
| 桌面框架 | Tauri 2（`@tauri-apps/api` v2） |
| 插件 | `tauri-plugin-dialog`（目录选择）、`tauri-plugin-opener`（打开链接/程序） |
| 后端 | Rust：`reqwest`（HTTP）、`md5`（校验）、`sevenz-rust`（7z 解压）、`sysinfo`（进程）、`winreg`（注册表） |

## 环境要求

- Node.js 18+（建议 20+）与 npm
- Rust 工具链（stable）+ Tauri 2 前置依赖，参见 [Tauri 官方指南](https://tauri.app/start/prerequisites/)
- Windows：WebView2 Runtime、MSVC 生成工具
- 推荐 IDE：VS Code + Volar（Vue - Official）、tauri-vscode、rust-analyzer

## 快速开始

```bash
# 安装依赖
npm install

# 开发模式（同时启动 Vite dev server 与 Tauri 窗口）
npm run tauri dev

# 仅前端（http://localhost:1420）
npm run dev

# 类型检查 + 前端构建
npm run build

# 打包安装包（targets = all：msi / exe / nsis 等）
npm run tauri build
```

Vite 固定端口 `1420`（`strictPort`），并忽略对 `src-tauri/` 的文件监听；如需真机 / 远程 HMR，设置环境变量 `TAURI_DEV_HOST`。

## 项目结构

```
.
├── index.html                     # 前端入口
├── vite.config.ts                 # Vite + Tauri 开发配置（端口 1420 / HMR）
├── src/                           # Vue 前端
│   ├── main.ts                    # 应用挂载
│   └── App.vue                    # 主界面：游戏列表、版本对比、下载进度、官启/官网按钮
├── src-tauri/                     # Rust 后端
│   ├── tauri.conf.json            # Tauri 配置（窗口、bundle、前后端联动）
│   ├── channels.json              # 运行时配置：launcher_id、下载缓冲/限速/占用上限
│   ├── capabilities/default.json  # 权限声明（core / opener / dialog）
│   └── src/
│       ├── main.rs                # 二进制入口
│       ├── lib.rs                 # Tauri command 注册、注册表扫描、官启识别、进程检测
│       ├── platform.rs            # 平台抽象 trait 与数据结构（GameInfo / RemoteGameInfo / DownloadProgress）
│       └── platforms/
│           ├── mod.rs             # 平台注册表与 game_id → 平台 路由
│           ├── mihoyo.rs          # 米哈游 HYP 接口：版本查询、整包/差分下载、自检
│           ├── steam.rs           # Steam（协议直启）
│           └── riot.rs            # Riot（聚合入口）
└── public/                        # 静态资源
```

## 配置说明（`src-tauri/channels.json`）

```jsonc
{
  "launcher_id": "jGHBHlcOq1",      // HYP 接口 launcher_id，留空则回退内置默认值
  "download_limit_mb": 0,           // 下载磁盘占用"保命线"，0 = 不限制（达到即安全截断，用于管道测试）
  "download_buffer_mb": 4,          // 流式写入缓冲区大小（MB），默认 4，越大 CPU 占用越低
  "download_speed_limit_mbps": 10   // 下载限速（MB/s），0 或不填 = 不限速；限速通过让出 CPU 时间片实现
}
```

配置读取为三级来源：**项目源码 `src-tauri/channels.json`（开发期主配置，改文件即时生效）→ 用户数据目录覆盖（生产环境）→ 内置默认值**。

`launcher_id` 失效会导致米哈游接口返回 `retcode != 0`，此时点击界面上的 🔬 自检查看报告，并替换此字段即可。

## 下载流程细节

1. `check_remote` 拉取版本与分卷列表（`parts`：url / md5 / size），并给出 `version_relation`（fresh / ahead / equal / behind）
2. 前端根据差分可用性自动选择模式：本地版本 == `patch_from` 时走差分（确认框显示差分体积），否则整包
3. `start_download` 后端二次守卫：`equal` 拒绝（已最新）、`behind` 拒绝（防止降级）、差分版本不匹配拒绝
4. 逐卷流式写入目标目录（缓冲大小 = `download_buffer_mb`），边写边算 MD5：
   - 磁盘上大小已等于该卷完整大小的文件直接跳过（断点友好）
   - 每轮读循环检查取消标志与 `download_limit_mb` 保命线，超限即截断并回报 `done_test`
   - 启用限速时按本块字节数计算应耗时，不足则 `sleep` 让出 CPU，避免忙等占满核心
5. 每卷写完即校验 MD5，失败则删除该卷并报错，避免脏文件残留
6. 差分包下载到 `patch_<from>_<to>/`，`.7z` 完成后自动解压到其下 `extracted/` 并打印内层结构，为后续合成做准备
7. 全程约每 500ms 通过 `download-progress` 事件回传进度、瞬时速度与 ETA；取消时删除半截文件并回报 `error:已取消`

## 支持的游戏与 ID

| game_id | 名称 | 平台 | 能力级别 |
| --- | --- | --- | --- |
| `genshin` | 原神 | 米哈游 | Full |
| `starrail` | 崩坏：星穹铁道 | 米哈游 | Full |
| `zenless` | 绝区零 | 米哈游 | Full |
| `terraria` | 泰拉瑞亚 | Steam | DirectLaunch |
| `ravenfield` | Ravenfield | Steam | DirectLaunch |
| `valorant` | 无畏契约 | Riot | Aggregate |
| `test_notepad` | 测试：记事本 | — | Full（联调用） |

新增游戏只需在对应平台的 `game_ids()` 中登记 ID，并按需实现 `remote_info` / `download` 等方法。

## Tauri Command 一览

| Command | 作用 |
| --- | --- |
| `get_installed_games` | 返回全部平台已扫描到的游戏及安装状态 |
| `bind_game(id, dir)` | 手动绑定游戏目录（exe 候选名由平台声明） |
| `launch_game(id, use_official)` | 启动游戏（或唤起官方启动器协议） |
| `get_running_games` | 返回各游戏是否正在运行（`sysinfo` 轮询） |
| `official_info(game_id)` | 返回本地官启 exe 路径、官网地址与兜底地址 |
| `open_official(game_id, mode)` | `auto` 拉起本地官启（无则开兜底）/ `web` 只开官网 |
| `check_remote(game_id)` | 查询远程最新版本、包体积、差分与版本关系 |
| `start_download(game_id, dest, use_patch)` | 开始下载（整包或差分，自动跳过已完成分卷） |
| `cancel_download(game_id)` | 取消进行中的下载（清理半截文件） |
| `probe_api` | 接口自检，返回诊断报告文本数组 |

前端通过 `listen("download-progress", ...)` 接收 `DownloadProgress` 事件更新进度条、速度与剩余时间。

## 已知限制

- 注册表扫描与部分启动逻辑为 Windows 专属，macOS / Linux 需补充平台分支
- 米哈游下载依赖非公开 HYP 接口，接口变更需同步更新 `mihoyo.rs`
- 差分下载目前只做落盘与解压侦察，尚未包含自动合成 / 安装落位的一键流程
- 限速与缓冲区配置修改后需重启应用生效


