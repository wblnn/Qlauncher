# qlauncher

一个基于 **Tauri 2 + Vue 3 + TypeScript** 的跨平台游戏启动器 / 下载管理器。它把多个发行平台（米哈游、库洛、Steam、Riot）统一进一个界面：自动扫描本机已安装的游戏，检查远程版本，支持整包下载、差分更新与 Sophon 分块对账更新，提供校验 / 修复 / 回滚能力，并一键启动或结束游戏。

> 当前后端大量依赖 Windows 注册表（`winreg`），主要在 **Windows** 上开发与验证。

📖 English documentation: [README.en.md](./README.en.md)

## 功能特性

- **多平台聚合**：以统一的 `GamePlatform` trait 抽象各平台能力，按能力分级展示
  - `Aggregate`（仅聚合入口，如 Riot）
  - `DirectLaunch`（协议直启，如 Steam）
  - `Download` / `Full`（可查版本 + 下载 + 启动 + 校验修复，如米哈游、库洛）
- **区服合并卡片**：同一游戏的国服 / 国际服在后端是两个 id，前端合成一张卡并可切换区服（选择记忆在 `localStorage`）
- **已安装游戏扫描**：读取 Windows 卸载注册表项 + 平台自有目录规则，识别安装路径、exe 与本地版本号；云游戏入口自动过滤
- **手动绑定**：注册表扫不到时，可通过目录选择对话框手动绑定游戏目录（exe 候选名由平台自行声明）
- **官方启动器识别**：从注册表定位本机官方启动器（HoYoPlay / Steam / Riot Client），一键"官启"拉起；识别不到时自动打开官网兜底，也可强制只开"官网"
- **版本守卫（P0）**：语义化版本比较，只有 `fresh`（未安装）或 `ahead`（远程有更新）才允许常规下载；接口整包滞后于本地版本时标记 `behind` 并禁止下载，避免"越更新越旧"；整包停更时可从公告 / 预下载推断真实版本（UI 标注"推断"）
- **三套更新引擎**：
  - **HYP 整包 / 差分**（绝区零等）：拉取分卷列表（`parts`：url / md5 / size），逐卷流式下载，差分包 `.7z` 下完自动解压，再由 `hpatchz.exe` 做单文件 hdiff 合成
  - **Sophon 分块对账**（原神 / 星铁）：解析官方 zstd + protobuf 清单（`sophon.rs`），与本地文件逐 chunk 比对，只下载变化的分块直接拼装落位，无需整包
  - **库洛官方清单对账**（鸣潮国服 / 国际服）：拉取官方文件清单（先验清单 md5，防 CDN 给旧版），按 size 快扫 + 官方分块 md5 内容级核对，只重下对不上的文件，支持多 CDN 故障切换
- **断点续跑 / 回滚（journal）**：补丁应用全程写日志（`PatchJournal`），中断可 `resume_patch` 续跑，出错可 `rollback_patch` 还原；备份采用同卷改名 `.qold`（零拷贝），临时文件后缀 `.qkuro` / `.qtmp`
- **校验与修复**：`verify_game_files` 支持快速（size）/ 深度（内容 md5）两种口径出报告；`repair_game_files` 按官方散列清单（`res_list_url`）逐文件补全差异项
- **低占用流式下载**：可配置流式缓冲区（`download_buffer_mb`，越大 CPU 占用越低）、基于让出 CPU 时间片的平滑限速（`download_speed_limit_mbps`）、磁盘占用保护线（`download_limit_mb`）与下载前空间预检（`install_size`）
- **断点友好**：重启下载时自动跳过大小已匹配的已完成分卷，无需从头重下
- **实时进度**：约每 500ms 通过 `download-progress` 事件回传已下载 / 总量 / 瞬时速度 / ETA，随时取消并清理半截文件
- **运行状态检测与进程管理**：基于 `sysinfo` 轮询进程，界面区分"运行中 / 已安装 / 未安装"；支持 `kill_game` 结束游戏、`list_zombie_games` 扫描残留僵尸进程并按 pid 逐个杀掉
- **接口自检**：内置 🔬 自检命令（`probe_api`），输出 HYP 接口连通性、`launcher_id` 来源、保命线与包信息解析结果，便于排查接口失效问题

## 技术栈

| 层 | 技术 |
| --- | --- |
| 前端 | Vue 3 `<script setup>`、TypeScript、Vite |
| 桌面框架 | Tauri 2（`@tauri-apps/api` v2） |
| 插件 | `tauri-plugin-dialog`（目录选择）、`tauri-plugin-opener`（打开链接/程序） |
| 后端 | Rust：`reqwest`（HTTP）、`md5`（校验）、`sevenz-rust` / `flate2` + `crc32fast`（解压）、`sysinfo`（进程）、`winreg`（注册表）、`prost` + `zstd`（Sophon 清单解码） |
| 外部工具 | `hpatchz.exe`（HDiffPatch，单文件差分合成，随包分发） |

## 环境要求

- Node.js 18+（建议 20+）与 npm
- Rust 工具链（stable）+ Tauri 2 前置依赖，参见 [Tauri 官方指南](https://tauri.app/start/prerequisites/)
- Windows：WebView2 Runtime、MSVC 生成工具
- 差分合成功能需 `hpatchz.exe`（放在 `qlauncher.exe` 同目录或 `src-tauri/` 下，仓库已附带）
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
│   └── App.vue                    # 主界面：区服合并卡片、版本对比、下载/补丁进度、
│                                  #   官启/官网、校验修复、僵尸进程面板、补丁续跑/回滚
├── src-tauri/                     # Rust 后端
│   ├── tauri.conf.json            # Tauri 配置（窗口、bundle、前后端联动）
│   ├── channels.json              # 运行时配置：launcher_id、下载缓冲/限速、hpatchz 路径等
│   ├── capabilities/default.json  # 权限声明（core / opener / dialog）
│   ├── hpatchz.exe                # 差分合成外部工具（随包分发）
│   └── src/
│       ├── main.rs                # 二进制入口
│       ├── lib.rs                 # Tauri command 注册、注册表扫描、官启识别、进程检测/杀进程
│       ├── platform.rs            # 平台抽象 trait 与数据结构（GameInfo / RemoteGameInfo / DownloadProgress）
│       ├── sophon.rs              # Sophon 引擎：zstd+protobuf 清单解析、本地分块 diff、分块下载拼装
│       └── platforms/
│           ├── mod.rs             # 平台注册表与 game_id → 平台 路由
│           ├── mihoyo.rs          # 米哈游 HYP：版本查询、整包/差分下载、hpatchz 合成、journal、校验修复、自检
│           ├── kuro.rs            # 库洛：官方配置/清单接口、整包裸传下载、内容级对账、多 CDN 切换
│           ├── steam.rs           # Steam（steam://run 协议直启）
│           └── riot.rs            # Riot（聚合入口）
└── public/                        # 静态资源
```

## 配置说明（`src-tauri/channels.json`）

```jsonc
{
  "launcher_id": "jGHBHlcOq1",              // HYP 接口 launcher_id，留空则回退内置默认值
  "launcher_ids": ["jGHBHlcOq1"],           // 备用 launcher_id 列表（自检/轮换用）
  "download_limit_mb": 0,                   // 下载磁盘占用"保命线"，0 = 不限制（达到即安全截断）
  "download_buffer_mb": 4,                  // 流式写入缓冲区大小（MB），默认 4，越大 CPU 占用越低
  "download_speed_limit_mbps": 10,          // 下载限速（MB/s），0 或不填 = 不限速；限速通过让出 CPU 时间片实现
  "hpatchz_path": "",                       // hpatchz.exe 路径覆盖，缺省 = exe 同目录 → src-tauri/ 兜底
  "keep_volumes_after_extract": true,       // 整包解压后是否保留原分卷
  "biz_patterns": {                         // 渠道自适应：game_id → 注册表/目录匹配关键字
    "genshin": ["hk4e", "原神"],
    "starrail": ["hkrpg", "星穹铁道"],
    "zenless": ["nap", "绝区零"]
  },
  "sophon_categories": ["游戏资源", "语音包-中文"]  // Sophon 更新要处理的分类
}
```

配置读取为三级来源：**项目源码 `src-tauri/channels.json`（开发期主配置，改文件即时生效）→ 用户数据目录覆盖（生产环境）→ 内置默认值**。

`launcher_id` 失效会导致米哈游接口返回 `retcode != 0`，此时点击界面上的 🔬 自检查看报告，并替换此字段即可。

## 更新流程细节

### HYP 整包 / 差分（绝区零等）

1. `check_remote` 拉取版本与分卷列表（`parts`：url / md5 / size），并给出 `version_relation`（fresh / ahead / equal / behind）
2. 前端根据差分可用性自动选择模式：本地版本 == `patch_from` 时走差分（确认框显示差分体积），否则整包
3. `start_download` 后端二次守卫：`equal` 拒绝（已最新）、`behind` 拒绝（防止降级）、差分版本不匹配拒绝
4. 逐卷流式写入目标目录（缓冲大小 = `download_buffer_mb`），边写边算 MD5；大小已匹配的卷直接跳过（断点友好）；每轮读循环检查取消标志与保命线；启用限速时按块字节数计算应耗时，不足则 `sleep` 让出 CPU
5. 每卷写完即校验 MD5，失败则删除该卷并报错，避免脏文件残留
6. `apply_patch` 阶段：`.7z` 解压产物逐文件调用 `hpatchz 旧文件 + 增量 → 新文件` 合成；旧文件先改名 `.qold` 备份并记入 journal，全部成功后删除备份、写 config.ini 版本号收尾

### Sophon 分块对账（原神 / 星铁）

1. 经 `getGameBranches → getBuild` 定位 manifest，zstd 解压后用 prost 解码 protobuf 清单（`sophon.rs`）
2. `diff_local` 与本地文件逐 chunk 比对（chunk md5 不一致才需要拉取），产出精确的下载队列——不预知总字节，进度以 chunk 计数回报
3. 变化的 chunk 流式下载并直接拼装到目标文件（`.qtmp` 临时名落位），全程可取消
4. 整包停更时版本来自公告 / 预下载推断（`announced_version`，UI 明确标注），此时仍允许走分块更新
5. 校验走 `verify_sophon`：以 Sophon 清单为权威（本地 pkg_version 在分块更新后已过期），且不对分块体系启用逐文件 URL 补全，防止旧整包地址造成降级

### 库洛官方清单对账（鸣潮）

1. 拉取官方 launcher 配置得到版本、CDN 与 `indexFile`，再拉文件清单并先验清单自身 md5（不符 = CDN 还是旧版，宁可失败也不按旧清单下）
2. 本地对账两档：size 快扫（查版本用，不为它去读 80GB）与官方分块 md5 内容级核对（下载 / 校验用）
3. 只下载对不上的文件（官方按文件裸传，落盘即最终大小，无解压步骤），Range 续传 + 多 CDN 故障切换（换 CDN 前重拉清单核对）
4. 私有增量格式 `.krpdiff` 不逆向，统一改写为"同一份整包清单 + 差异文件重下"策略

## 支持的游戏与 ID

| game_id | 名称 | 平台 | 能力级别 | 更新引擎 |
| --- | --- | --- | --- | --- |
| `genshin` / `genshin_global` | 原神（国服/国际服） | 米哈游 | Full | Sophon 分块 |
| `starrail` / `starrail_global` | 崩坏：星穹铁道（国服/国际服） | 米哈游 | Full | Sophon 分块 |
| `zenless` / `zenless_global` | 绝区零（国服/国际服） | 米哈游 | Full | HYP 整包/差分 |
| `wuthering_waves` / `wuthering_waves_global` | 鸣潮（国服/国际服） | 库洛 | Full | 官方清单对账 |
| `pgr` | 战双帕弥什 | 库洛 | Full | 官方清单对账 |
| `terraria` | 泰拉瑞亚 | Steam | DirectLaunch | —（`steam://run`） |
| `ravenfield` | Ravenfield | Steam | DirectLaunch | —（`steam://run`） |
| `valorant` | 无畏契约 | Riot | Aggregate | —（官启/官网兜底） |
| `test_notepad` | 测试：记事本 | — | Full（联调用） | — |

新增游戏：米哈游 / 库洛在各自 `*_GAMES` 表加一行（id、biz/game_id、exe 候选名、匹配关键字、官网），Steam / Riot 在对应平台的列表登记；`game_ids()` 自动从表生成，按需实现 `remote_info` / `download` 等方法即可。

## Tauri Command 一览

| Command | 作用 |
| --- | --- |
| `get_installed_games` | 返回全部平台已扫描到的游戏及安装状态 |
| `bind_game(id, dir)` | 手动绑定游戏目录（exe 候选名由平台声明） |
| `launch_game(id, use_official)` | 启动游戏（或唤起官方启动器协议） |
| `kill_game(game_id)` | 结束该游戏的全部进程 |
| `get_running_games` | 返回各游戏是否正在运行（`sysinfo` 轮询） |
| `list_zombie_games` / `kill_process(pid)` | 扫描游戏残留进程 / 按 pid 定点清除 |
| `official_info(game_id)` | 返回本地官启 exe 路径、官网地址与兜底地址 |
| `open_official(game_id, mode)` | `auto` 拉起本地官启（无则开兜底）/ `web` 只开官网 |
| `check_remote(game_id)` | 查询远程最新版本、包体积、差分与版本关系（含版本来源解释） |
| `start_download(game_id, dest, use_patch, allow_old)` | 开始下载（整包 / 差分 / 分块对账，自动跳过已完成分卷） |
| `cancel_download(game_id)` | 取消进行中的下载（清理半截文件） |
| `apply_patch(game_id, patch_dir, dry_run)` | 合成补丁（hpatchz / Sophon 拼装），支持试运行 |
| `patch_status(game_id)` | 查询未完成补丁的 journal（断点依据） |
| `resume_patch(game_id)` / `rollback_patch(game_id)` | 续跑中断的补丁 / 按 journal 回滚还原 |
| `verify_game_files(game_id, deep)` | 校验本地文件（快速 size 口径 / 深度内容口径），返回报告 |
| `repair_game_files(game_id)` | 按官方清单补全差异文件 |
| `probe_api` | 接口自检，返回诊断报告文本数组 |

前端通过 `listen("download-progress", ...)` 接收 `DownloadProgress` 事件更新进度条、速度与剩余时间；补丁合成阶段同样复用该事件（chunk 计数口径）。

## 已知限制

- 注册表扫描与部分启动逻辑为 Windows 专属，macOS / Linux 需补充平台分支
- 米哈游 / 库洛下载依赖非公开接口，接口变更需同步更新 `mihoyo.rs` / `kuro.rs` / `sophon.rs`
- 差分合成依赖外置 `hpatchz.exe`，缺失时相关命令会明确报错提示放置位置
- 库洛私有增量格式 `.krpdiff` 未支持，采用整包清单差异重下替代
- 限速与缓冲区配置修改后需重启应用生效
