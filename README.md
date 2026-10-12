# qlauncher

一个基于 **Tauri 2 + Vue 3 + TypeScript** 的跨平台游戏启动器 / 下载管理器。它把多个发行平台（米哈游、库洛、Steam、Riot）统一进一个界面：自动扫描本机已安装的游戏，检查远程版本，支持整包、差分（hpatchz 合成）与 Sophon 分块三种更新方式，并一键启动游戏。

> 当前后端大量依赖 Windows 注册表（`winreg`），主要在 **Windows** 上开发与验证。

📖 English documentation: [README.en.md](./README.en.md)

## 功能特性

- **多平台聚合**：以统一的 `GamePlatform` trait 抽象各平台能力，按能力分级展示
  - `Aggregate`（仅聚合入口，如 Riot）
  - `DirectLaunch`（可直接启动，如 Steam）
  - `Download` / `Full`（可查版本 + 下载 + 启动，如米哈游、库洛）
- **区服合并卡片**：同一游戏的国服 / 国际服在后端是两个 ID，前端自动合并成一张卡，可在卡内切换区服（选择记在 localStorage）
- **已安装游戏扫描**：读取 Windows 卸载注册表项 + 平台自有目录规则，识别安装路径、exe 与本地版本号；云游戏入口自动过滤
- **手动绑定**：注册表扫不到时，可通过目录选择对话框手动绑定游戏目录（exe 候选名由平台自行声明）
- **官方启动器识别**：从注册表定位本机官方启动器（HoYoPlay / Steam / Riot Client），一键"官启"拉起；识别不到时自动打开官网兜底，也可强制只开"官网"
- **版本守卫与版本源自适应**：语义化版本比较，只有 `fresh` / `ahead`（或官宣推断出的 `announced`）才允许下载；远程"最新版本"取 整包 / 预下载 / 公告·分支 tag 三者最大，界面标注来源与检查时间，接口整包滞后时标记 `behind` 并提示走官启，避免"越更新越旧"
- **三条更新引擎**：
  - **Sophon 分块引擎（原神 / 星铁国服）**：解析官方 protobuf 分块清单，逐文件 md5 对账后只下载差异 chunk（zstd 解压 → staging 拼装 → 整文件校验 → `.qlold` 备份替换），不需要整包压缩包、不受整包停更影响
  - **整包 / 差分下载（绝区零等 HYP 渠道）**：差分包下完后调用 `hpatchz.exe` 原地合成升级；整包分卷（`.zip.001…` / `.7z.001…`）通过"虚拟连续流"直接拼接解压（自研 ZIP64/deflate 读取器 + sevenz-rust），解压后可选保留或删除分卷
  - **库洛官方清单引擎（鸣潮国服 / 国际服）**：三步走官方协议（配置索引 → 文件清单 → 按文件裸传），内容级对账（大文件按官方 100MiB 分块 md5、小文件整文件 md5），只补对不上的文件/坏块，天然分块断点续传
- **可逆的差分提交（journal 事务）**：合成升级按 预检 → 合成 → 替换 → 删除 → 校验 → 提交 六阶段推进，每步写 journal；任何一步失败或断电都可「继续更新」(`resume_patch`) 或「↩ 回滚」(`rollback_patch`，还原 `.qold`、删除新增文件、复原 config.ini)，不可逆动作全部集中在提交点之后
- **🩹 校验修复**：按官方清单逐文件校验本地（快速=比大小+关键 exe md5，深度=内容级 md5），只补缺失/损坏的文件而非整包重装；原神/星铁自动切到 Sophon 清单口径，库洛走分块 md5 内容级对账
- **低占用流式下载**：可配置流式缓冲区（`download_buffer_mb`）、基于让出 CPU 时间片的平滑限速（`download_speed_limit_mbps`）、磁盘占用保护线（`download_limit_mb`）
- **实时进度**：独立上报线程固定 1Hz 回传 已下载/总量/瞬时速度（轻度平滑）/ETA，对账阶段单独报 `verifying` 进度；随时取消——已下好的分块保留在临时文件里，重跑接着下
- **运行状态与僵尸进程**：基于 `sysinfo` 轮询区分"运行中 / 已安装 / 未安装"；工作集 ≤10MB 的残留进程识别为僵尸，可按 PID 精准清理（`list_zombie_games` / `kill_process`），避免残骸锁住游戏文件
- **接口自检**：内置 🔬 自检命令，输出 HYP 接口连通性、`launcher_id` 来源、保命线与包信息解析结果，便于排查接口失效问题

## 技术栈

| 层 | 技术 |
| --- | --- |
| 前端 | Vue 3 `<script setup>`、TypeScript、Vite |
| 桌面框架 | Tauri 2（`@tauri-apps/api` v2） |
| 插件 | `tauri-plugin-dialog`（目录选择）、`tauri-plugin-opener`（打开链接/程序） |
| 后端 | Rust：`reqwest`（HTTP）、`md5`（校验）、`prost` + `zstd`（Sophon 分块清单/数据）、`sevenz-rust` + `flate2` + `crc32fast`（整包分卷解压）、`sysinfo`（进程/磁盘）、`winreg`（注册表） |
| 外部工具 | `hpatchz.exe`（HDiffPatch 官方二进制，差分合成用，需自行放到 `src-tauri/` 或在 channels.json 指定路径） |

## 环境要求

- Node.js 18+（建议 20+）与 npm
- Rust 工具链（stable）+ Tauri 2 前置依赖，参见 [Tauri 官方指南](https://tauri.app/start/prerequisites/)
- Windows：WebView2 Runtime、MSVC 生成工具
- 差分合成功能需要 `hpatchz.exe`（去 HDiffPatch release 页下载）
- 推荐 IDE：VS Code + Volar（Vue - Official）、rust-analyzer

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
│   └── App.vue                    # 主界面：区服合并卡片、版本对比、下载/合成进度、校验修复、僵尸进程清理
├── src-tauri/                     # Rust 后端
│   ├── tauri.conf.json            # Tauri 配置（窗口、bundle、前后端联动）
│   ├── channels.json              # 运行时配置：launcher_id、下载缓冲/限速/占用上限、hpatchz、Sophon/库洛覆盖项
│   ├── capabilities/default.json  # 权限声明（core / opener / dialog）
│   └── src/
│       ├── main.rs                # 二进制入口
│       ├── lib.rs                 # Tauri command 注册、注册表扫描、官启识别、进程/僵尸检测
│       ├── platform.rs            # 平台抽象 trait 与数据结构（GameInfo / RemoteGameInfo / DownloadProgress）
│       ├── sophon.rs              # Sophon 分块引擎：protobuf 清单、chunk 对账/下载(zstd)、staging 替换、config.ini 回写
│       └── platforms/
│           ├── mod.rs             # 平台注册表与 game_id → 平台 路由
│           ├── mihoyo.rs          # 米哈游：HYP 版本源、整包分卷虚拟流解压、hpatchz 差分合成(journal 事务)、校验修复
│           ├── kuro.rs            # 库洛：鸣潮国服/国际服官方清单引擎（内容级对账、分块续传、CDN 轮换重试）
│           ├── steam.rs           # Steam（协议直启）
│           └── riot.rs            # Riot（聚合入口）
└── public/                        # 静态资源
```

## 配置说明（`src-tauri/channels.json`）

```jsonc
{
  "launcher_id": "jGHBHlcOq1",          // HYP 接口 launcher_id，留空则回退内置默认值
  "launcher_ids": ["jGHBHlcOq1"],       // 候选列表，自检时逐个验证可用性
  "biz_patterns": {                      // getGames 里匹配业务名的规则（官方改名时加一条即可，不用重编译）
    "genshin": ["hk4e", "原神"], "starrail": ["hkrpg", "星穹铁道"], "zenless": ["nap", "绝区零"]
  },
  "sophon_categories": ["游戏资源", "语音包-中文"],  // Sophon 更新要处理的分类
  "hpatchz_path": "",                    // hpatchz.exe 路径，缺省 = exe 同级目录 → 项目 src-tauri/ 兜底
  "keep_volumes_after_extract": true,    // 整包解压完成后是否保留分卷
  "download_limit_mb": 0,                // 下载磁盘占用"保命线"，0 = 不限制（达到即安全截断，用于管道测试）
  "download_buffer_mb": 4,               // 流式写入缓冲区大小（MB），越大 CPU 占用越低
  "download_speed_limit_mbps": 10,       // 下载限速（MB/s），0 或不填 = 不限速；限速通过让出 CPU 时间片实现
  "kuro_games": {                        // 库洛分发常量覆盖（gameId/appKey/端点变了改这里，不用重编译）
    "wuthering_waves": { "game_id": "G152", "app_key": "...", "cn": true }
  }
}
```

配置读取为三级来源：**项目源码 `src-tauri/channels.json`（开发期主配置，每次任务开始时读取）→ 用户数据目录覆盖（生产环境）→ 内置默认值**。

`launcher_id` 失效会导致米哈游接口返回 `retcode != 0`，此时点击界面上的 🔬 自检查看报告，并替换此字段即可。

## 下载 / 更新流程细节

### Sophon 分块引擎（原神 / 星铁，国服）
1. `getGameBranches` → `getBuild` 拿最新 tag 与 manifest 地址，protobuf 解码出全量文件/chunk 清单
2. 逐文件对账本地（size + md5），只对变更文件计算所需 chunk 集合（进度按压缩字节计）
3. chunk 从 CDN 下载（zstd 解压）写入 `ql_staging/`；续跑时先校验 staging 已有区域，正确即跳过
4. 整文件 md5 复核通过后替换入游戏目录（旧文件先改名 `.qlold`，全部成功后统一删除）
5. 用实际应用到的 tag 回写 `config.ini`；空间预检 = 变更文件 staging 体积 + 5GB 余量

### hpatchz 差分合成（绝区零等 HYP 渠道）
1. 差分包下载到 `patch_<from>_<to>/`，`.7z` 完成后自动解压到其下 `extracted/`
2. `apply_patch` 六阶段事务：预检（进程占用/可写性/空间）→ 合成（原件改名 `.qold` 备份，`hpatchz 旧文件 增量 新文件`）→ 替换 → 删除（只改名不真删）→ 校验门（逐条复核 md5）→ ★提交★（删 `.qold`、写 config.ini 版本号）
3. journal 全程落盘：中断后可 `resume_patch` 续跑（每步幂等）或 `rollback_patch` 一键回滚；`dry_run` 只做预检报计划

### 整包下载与虚拟流解压
1. 多分卷（`.zip.001…` / `.7z.001…`）逐卷流式写入：缓冲大小 = `download_buffer_mb`，边写边算 MD5，启用限速时按块计算应耗时后 `sleep` 让出 CPU
2. 磁盘上大小已匹配的完整分卷直接跳过（断点友好）；每轮读循环检查取消标志与 `download_limit_mb` 保命线
3. 全部分卷校验通过后，用"虚拟连续流"跨卷拼接直接解压到 `<game_id>_<version>/`（ZIP 走自研 ZIP64/deflate 读取器，7z 走 sevenz-rust），按 `keep_volumes_after_extract` 决定是否删卷释放空间

### 库洛官方清单引擎（鸣潮）
1. 配置索引 `{api}/{gameId}/{appId_appKey}/index.json` → version / baseUrl / indexFile(+md5) / cdnList（CDN 强 gzip 响应会手动解压）
2. 拉文件清单并校验 `indexFileMd5`（不符视为旧 CDN，宁可失败也不按旧清单下）
3. 内容级对账：大文件按官方 100MiB 分块逐块 md5，小文件整文件 md5——同尺寸不同内容也能抓出来；首次安装几乎瞬间完成
4. 只下载差异集：缺失文件从零分块下；内容不符的文件**复制本地当底稿只补坏块**（不用重下整个 18GB pak）；上次中断的 `.qkuro` 直接当续跑底稿
5. 每个文件最多换 2 个 CDN 重试；连续 5 个失败即止损收尾（已完成的保住）；落位采用 `.qold` 备份 + rename，失败自动还原
6. 完成后写 `ql_kuro_version.txt` 本地版本标记；官方增量是私有 `.krpdiff` 格式（不逆向），此处以"整包清单差异下载"等效替代

## 支持的游戏与 ID

| game_id | 名称 | 平台 | 能力级别 |
| --- | --- | --- | --- |
| `genshin` / `genshin_global` | 原神（国服/国际服） | 米哈游 | Full（国服走 Sophon；国际服原神暂只能官启） |
| `starrail` / `starrail_global` | 崩坏：星穹铁道（国服/国际服） | 米哈游 | Full（同上） |
| `zenless` / `zenless_global` | 绝区零（国服/国际服） | 米哈游 | Full（整包/差分） |
| `wuthering_waves` / `wuthering_waves_global` | 鸣潮（国服 G152 / 国际服 G153） | 库洛 | Full（官方清单引擎） |
| `pgr` | 战双帕弥什 | 库洛 | Full（分发常量待接入，可在 channels.json 补齐） |
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
| `kill_game(game_id)` | 结束该游戏进程 |
| `get_running_games` | 返回各游戏是否正在运行（`sysinfo` 轮询） |
| `list_zombie_games` | 列出工作集 ≤10MB 的残留/僵尸进程 |
| `kill_process(pid)` | 按 PID 精准清理僵尸进程 |
| `official_info(game_id)` | 返回本地官启 exe 路径、官网地址与兜底地址 |
| `open_official(game_id, mode)` | `auto` 拉起本地官启（无则开兜底）/ `web` 只开官网 |
| `check_remote(game_id)` | 查询远程最新版本、包体积、差分、版本关系与版本来源 |
| `start_download(game_id, dest, use_patch, allow_old)` | 开始下载/更新（自动路由到 Sophon / 库洛 / 整包差分引擎） |
| `cancel_download(game_id)` | 取消进行中的下载或对账（保留已下好的分块） |
| `probe_api` | 接口自检，返回诊断报告文本数组 |
| `apply_patch(game_id, patch_dir, dry_run)` | 差分合成升级（可逆事务；`dry_run` 只预检报计划） |
| `patch_status(game_id)` | 读取未完成更新的 journal（阶段、进度、备份清单） |
| `resume_patch(game_id)` | 按 journal 断点续跑合成 |
| `rollback_patch(game_id)` | 一键回滚到补丁前（还原 `.qold`、删新增、复原 config.ini） |
| `verify_game_files(game_id, deep)` | 校验修复-校验：按官方清单对账（快速/内容级；原神星铁走 Sophon 口径，库洛走分块 md5） |
| `repair_game_files(game_id)` | 校验修复-修复：只补缺失/损坏文件 |

前端通过 `listen("download-progress" | "patch-progress" | "verify-progress", ...)` 接收事件更新进度条、阶段步骤条与剩余时间。

## 已知限制

- 注册表扫描与部分启动逻辑为 Windows 专属，macOS / Linux 需补充平台分支
- 米哈游下载依赖非公开 HYP / Sophon 接口，库洛下载依赖非公开官方清单接口，接口变更需同步更新对应模块（多数常量可通过 channels.json 热覆盖）
- 国际服原神 / 星铁的 Sophon 构建入口尚未接入，只能走官启更新；国际服绝区零整包可用
- 战双帕弥什的分发常量（gameId + appKey）尚未拿到，接入后填 channels.json 即可
- 差分合成需要外部的 `hpatchz.exe`，缺失时报错提示放置位置
- 限速 / 缓冲 / 保命线等配置在每次下载任务开始时读取，修改后对进行中的任务不生效

## Git 提交约定

提交信息使用中文描述实际改动，例如：`优化了下载占用，完善了一下下载交互`。
