# Berth 设计文档（工作名，待定）

> 一句话：面向 AI coding agent 的 GPU 原生终端。关掉窗口 agent 继续跑；重开窗口内容仍在；左侧看到所有 workspace / session 的实时预览与 agent 状态。
>
> 状态：v0.1 设计稿，2026-09-26。待用户确认第 12 节决策项后进入 M0。

## 1. 目标与非目标

| 编号 | 目标 | 验收信号 |
|---|---|---|
| G1 | Ghostty 级日常终端：GPU 渲染、字体回退 / CJK / emoji、IME、真彩、选择复制、平滑滚动 | vim / htop / claude TUI 渲染正确；中文输入法可用；60fps |
| G2 | 内容持久化：关 GUI 不杀 session；重开秒级重连；daemon / 机器重启后历史仍可看，可一键续起 | 关窗→重开：屏幕与 scrollback 无损；`kill -9 berthd` 后重启仍能看到历史并 revive |
| G3 | Agent 感知：识别 pane 里的 claude / codex / shell，状态机 + 未聚焦通知 | 3 个并行 claude，侧栏状态与实际一致，等授权时有通知 |
| G4 | 左侧栏：workspace → sessions 树、末尾几行实时预览、状态徽标、未读、快捷跳转 | ⌘1..9 切换；预览 ≤4Hz 刷新；30 个 session 不卡 |

非目标（v1）：分屏 splits、多窗口、远程 mux（SSH 域）、Windows、复杂文字 shaping（Arabic / Indic）、插件系统、Ghostty 配置文件兼容。

## 2. 关键决策（推荐方案 + 因果 + 证据）

| # | 决策 | 推荐 | 因果链 | 代价 / 备选 |
|---|---|---|---|---|
| D1 | 进程模型 | `berthd` 常驻 daemon 持有 PTY 与 VT 状态；GUI 是客户端 | 「关窗口 agent 不能死」只能靠 PTY 所有权在 GUI 进程之外实现；tmux / wezterm-mux-server 都是这个结构 | 多一层协议；备选「仅磁盘快照」关窗即杀 agent |
| D2 | VT 核心 | `alacritty_terminal` 0.26（Apache-2.0） | Rust 生态唯一大规模验证过的独立 VT 库，Zed 内置终端同样使用；自带 damage tracking 与 serde | 不解析 OSC 7 / 133，需要侧信道预扫描；备选 libghostty-vt（Zig FFI，成熟度待核实） |
| D3 | 渲染栈 | `winit` + `wgpu` + 自研网格渲染（`cosmic-text` shaping / 字体回退，`swash` 光栅化，自建 atlas）+ `egui-wgpu` 画侧栏与对话框 | 终端网格必须逐格定位，通用文本框架不给这个控制；侧栏是普通 UI，egui 最快 | 备选 GPUI：crates.io 版本停在 2025-10，需 git 依赖，API 漂移；M0 spike 用证据定案 |
| D4 | 持久化 | 三层：daemon 内存活态 → 5s 脏快照（紧凑行格式 + zstd）→ 可选原始输出 journal | 快照作为只读「历史前缀」拼在新 PTY 上方，不往 alacritty 内部注入，避免依赖私有 API | journal 默认关（隐私） |
| D5 | Agent 状态源 | hooks（精确）> OSC 133 / 7（shell 集成）> 进程树 / 输出活动（启发式） | 每个状态带 `source` 与 `confidence`，UI 区分实测与推断 | 启发式可能误判，用虚线图标标注 |
| D6 | Hook 安装 | `berth setup-hooks`：显示 diff、追加合并、`--undo` 可撤销；绝不静默改 `~/.claude/settings.json` / `~/.codex/config.toml` | 用户已有 Notification ×2、PreToolUse、SessionStart hooks 与 statusline，codex `notify` 已被 Computer Use 客户端占用，必须链式保留 | 未安装 hooks 时退化到 D5 的后两层 |
| D7 | 统一行格式 | `LineSnapshot`（文本 + 样式 run）同时用于 socket 协议、磁盘快照、侧栏预览 | 一种格式三处复用，序列化体积比 Cell 数组小 10× 以上 | 需要从 alacritty `Row<Cell>` 转换一次 |

## 3. 总体架构

```
┌──────────────── berth（GUI：winit + wgpu + egui）──────────────────┐
│  Sidebar(egui)  │  Grid renderer(wgpu, 自研)  │  IME / 键鼠 / 剪贴板  │
└───────────┬──────────────────────────────────────────────────────┘
            │ unix socket（postcard 帧，长度前缀）
┌───────────┴──────────────── berthd（tokio）────────────────────────┐
│ SessionManager                                                      │
│   Session ×N：PTY(portable-pty) → OSC 预扫描 → alacritty Term → damage │
│ AgentStateMachine ← HookReceiver(socket) ← berth-hook（claude/codex 调起）│
│ Store：SQLite(meta, events) + snapshots/<sid>.bin.zst + journal/     │
└─────────────────────────────────────────────────────────────────────┘
```

### 3.1 Cargo workspace

| crate | 二进制 | 职责 |
|---|---|---|
| `berth-core` | — | 领域类型：`WorkspaceId`、`SessionId`、`AgentState`、`LineSnapshot`、协议消息（serde） |
| `berth-vt` | — | `alacritty_terminal` 封装：PTY、OSC 侧信道预扫描、damage → 行增量、`Row<Cell>` → `LineSnapshot` |
| `berth-store` | — | SQLite（`rusqlite` bundled）元数据与事件；快照原子写；journal 轮转；`purge` |
| `berth-daemon` | `berthd` | 会话管理、状态机、socket 服务、单实例锁、快照调度 |
| `berth-hook` | `berth-hook` | 极小 CLI：stdin JSON → socket；daemon 不在时 <10ms 静默退出，永不阻塞 agent；`statusline` 子命令 tee 原 statusline |
| `berth-app` | `berth` | GUI + CLI 子命令（`list` / `attach` / `setup-hooks` / `doctor` / `daemon start|stop`） |

### 3.2 进程生命周期

- `berth` 启动时连接 socket；不存在则 spawn `berthd`（detached）。可选 `berth daemon install` 生成 launchd plist 开机自启（显式，不默认）。
- 单实例：`~/Library/Application Support/berth/berthd.lock`（flock）+ `berthd.sock`（0600，目录 0700）。Linux 用 `$XDG_RUNTIME_DIR`。
- daemon 启动：读 SQLite；上次标记为 Live 的 session 一律变为 `Dormant`，快照加载为可恢复历史。
- session 结束条件：shell 退出 / 用户 kill；GUI 关闭不影响。

## 4. 数据模型

```rust
struct Workspace { id, name, root: PathBuf, color: Option<Color>, order: u32, created_at }

struct Session {
    id: SessionId, workspace: WorkspaceId,
    title: Title { auto: String, user: Option<String> },
    cwd: PathBuf,                 // OSC 7 优先，其次 tcgetpgrp + proc_pidinfo
    command: Vec<String>, env_extra: Vec<(String,String)>,
    status: Live | Dormant { exit: Option<i32>, at } | Restored,   // Restored = 仅历史
    agent: AgentInfo, created_at, last_active, unread: bool,
    persist: PersistPolicy { snapshot: bool, journal: bool },
}

struct AgentInfo {
    kind: Shell | Claude | Codex | Other(String),
    external_id: Option<String>,         // claude session uuid / codex thread id
    transcript: Option<PathBuf>, model: Option<String>,
    context_pct: Option<f32>, cost_usd: Option<f32>,   // 来自 statusline tee（可选）
    state: AgentState, since: Instant, source: Hook | Osc | Heuristic, confidence: f32,
}

enum AgentState { Idle, Thinking, ToolRunning { name }, WaitingPermission { tool },
                  WaitingInput, Done, Error(String), Compacting, Exited }

struct LineSnapshot { runs: Vec<Run> }         // Run { text: String, style: StyleId, width_cells: u16 }
struct StyleTable { styles: Vec<Style> }        // fg/bg/underline color、flags（bold/italic/dim/curly…）、hyperlink id
```

## 5. VT 引擎与 OSC 侧信道

- `Term<Listener>` 每 session 一个，`scrolling_history` 默认 20_000 行（可配，上限 100_000）。
- PTY 读线程 → 4ms 合批 → `Processor::advance` → `term.damage()` 取脏行 → 广播。
- **OSC 预扫描器**：alacritty 对未知 OSC 只打 debug 日志，没有回调；因此在字节进 `Term` 前跑一个只识别 `ESC ] … BEL/ST` 的轻量状态机，抽取：
  - OSC 7（cwd）、OSC 133 A/B/C/D（prompt 开始 / 命令开始 / 命令结束 + exit code）、OSC 9 / 777（通知）、OSC 1337（iTerm 扩展，仅识别不实现）。
  - 若核实 `vte::ansi::Handler` 已提供未处理 OSC 回调，则改为回调路径，删掉预扫描。
- **Shell 集成**：仿 Ghostty，zsh 通过 `ZDOTDIR` 垫片注入（bash 用 `--rcfile`，fish 用 `XDG_DATA_DIRS`），只发 OSC 7 / 133。用户可 `shell-integration = none` 关闭。
- 环境注入：`BERTH_SESSION_ID`、`BERTH_SOCKET`、`TERM=xterm-256color`（terminfo 先复用 xterm，自有 terminfo 放 v2）、`COLORTERM=truecolor`。
- **颜色查询**（OSC 4 / 10 / 11 / 12）：程序没有自设的颜色，按 GUI 实际绘制的颜色回答。GUI 每次连上 daemon 的第一条请求是 `SetTermColors`（`[theme]` 生效后的前景、背景、光标与 256 色，§8.4）；daemon 存一份并带代号，每个 session 在解析输出前比对代号取用，所以已在运行的、之后新建或 Revive 的 session 都拿得到，恰在此时启动的也不会漏。GUI 发来之前（以及调色板里没给的项）用 Alacritty 的默认配色；多个 GUI 以最后发来的为准。答错的代价：Codex 启动时查一次 OSC 11 判断深浅并据此给输入框配底色，窗口是白底而回答是 `#181818` 时，输入框成了 `#333333` 深灰配 `#1f2328` 深色字（1.25:1）。

## 6. 持久化（三层）

| 层 | 内容 | 触发 | 恢复方式 |
|---|---|---|---|
| L1 内存 | `Term` 全状态 | 常驻 | GUI 重连即得 |
| L2 快照 | 可视屏 + scrollback（`LineSnapshot` + `StyleTable`）+ 光标 + 标题 + cwd + agent 元数据 | 脏且距上次 ≥5s；优雅退出；session 结束 | daemon 重启后作为只读历史前缀 |
| L3 journal | 原始 PTY 字节 + 时间戳（asciinema 风格） | 可选，默认关；64MB 轮转 | 精确回放 / 取证 |

- 快照写入：`postcard` 序列化 → `zstd` → 写 `.tmp` → `fsync` → `rename`；100k 行 × 200 列典型压缩后 < 10MB。
- 恢复视图：虚拟行号空间 `[restored 0..R) ++ [live 0..)`，客户端 `FetchLines` 按范围拉，daemon 从快照或 `Term` 取；多次重启累积前缀，总量按配置封顶。
- **Revive**：`Dormant` / `Restored` session 一键在原 cwd 起 shell；若 `agent.kind == Claude` 且有 `external_id`，提供「`claude --resume <id>`」按钮（命令可配）。Codex 对应 `codex resume <thread>`（待核实 CLI 参数）。
- 隐私：数据目录 0700、文件 0600；per-session / per-workspace `persist=false`；`berth purge <session|--all>`；OSC 52 剪贴板读默认拒绝、写需配置开启。

## 7. 协议（客户端 ⟷ daemon）

- 传输：unix socket，帧 = `u32 len` + `postcard(Msg)`；`Hello { role: Gui | Hook | Cli, version }` 版本握手，不兼容即拒绝。
- 客户端 → daemon：`ListWorkspaces`、`ListSessions`、`CreateWorkspace`、`CreateSession { ws, cwd, cmd }`、`Attach { sid, cols, rows }`、`Detach`、`Resize`、`Input { sid, bytes }`、`FetchLines { sid, range }`、`Subscribe { sid, mode: Full | Preview { rows: 4, hz: 4 } }`、`Kill`、`Revive { sid, mode: Shell | ResumeAgent }`、`MarkRead`、`Rename`、`Move`。
- 协议 v4（只追加）：`Request::SetTermColors(TermColors)`，GUI 握手后的第一条请求，回 `Ok`（§5 颜色查询）。`PROTOCOL_VERSION = 4`；旧 daemon 照例在 `Hello` 处回 `Incompatible`，走横幅 / `berth debug restart-daemon`。
- daemon → 客户端：`Sessions(Vec<SessionSummary>)`、`Screen { sid, seq, dirty: Full | Lines(Vec<(row, LineSnapshot)>), cursor, display_offset, modes }`、`Lines { sid, start, lines }`、`AgentChanged { sid, agent }`、`Exited { sid, code }`、`Title`、`Cwd`、`Bell`、`Notify { sid, text }`。
- hook → daemon：`AgentEvent { berth_sid（来自 env）, kind, event: ClaudeHook(json) | CodexNotify(json) | Statusline(json) }`。
- 带宽控制：聚焦 session 全分辨率增量（合批 ≤120Hz）；侧栏预览只订阅末尾 N 行、≤4Hz；30 个 session 时总流量 < 200KB/s。
- 尺寸：v1 单 GUI 客户端；多客户端时最小尺寸胜出（tmux 语义），预览订阅不影响尺寸。

## 8. GUI 客户端

### 8.1 渲染

- `winit` 0.30 事件循环，`wgpu` 30（Metal），retina 感知；两个 pass：网格 pass → egui pass。
- 网格 pass：实例化 quad —— 背景色层、文字层（atlas 采样）、装饰层（下划线 / 波浪线 / 删除线 / 光标 / 选区）。
- 文字：每行用 `cosmic-text` shaping（得到字体回退 + 连字 + ZWJ emoji 簇），**忽略其 advance，按 cell 定位**；簇跨多格时放首格。`swash` 光栅化进 atlas（`etagere` 打包，灰度 + 彩色两张）。按行内容哈希缓存 shaping 结果。
- 侧栏预览：egui `PaintCallback` 内调用同一网格渲染器，小字号。
- 默认字体：跟随用户 Ghostty 配置 `SF Mono 13`，回退链由系统提供。

### 8.2 输入

- 键盘：winit `KeyEvent` → xterm 编码（含 modifyOtherKeys / kitty keyboard protocol v2 可选）；⌘ 系快捷键在 GUI 层拦截。
- IME：winit `Ime::Preedit / Commit`，预编辑文本画在光标处（下划线），Commit 才写 PTY。M0 必须验证中文输入法。
- 鼠标：选择（字 / 行 / 块）、滚轮、⌘点击 URL（v1.1）、鼠标模式透传给 TUI。
- 剪贴板：`arboard`；bracketed paste；粘贴多行时提示（可关）。

### 8.3 侧栏 UX

```
┌────────────────────────────┬──────────────────────────────────────┐
│ ▾ ~/projects/foo      [+]  │                                        │
│  ● claude  修测试   ⚙ Bash 12s │   ← 聚焦 session 的完整终端            │
│    │ $ cargo test …        │                                        │
│    │ running 42 tests      │                                        │
│  ◐ codex   重构 api  ⏳ 等授权 │                                        │
│  ○ zsh     ~/foo/web        │                                        │
│ ▾ ~/work/bar               │                                        │
│  ✓ claude  完成 5m前 (未读) │                                        │
│ ▸ Dormant (2)  点击 revive │                                        │
├────────────────────────────┴──────────────────────────────────────┤
│ opus-5 · ctx 42% · $1.23 · 3 running · 1 needs attention   ⌘K 面板 │
└───────────────────────────────────────────────────────────────────┘
```

- 徽标：○ Idle / ◐ Thinking（旋转）/ ⚙ ToolRunning(name + 秒)/ ⏳ WaitingPermission（橙色脉冲）/ ✎ WaitingInput / ✓ Done（未读高亮）/ ✗ Error / ⏹ Exited；启发式来源用虚线图标。
- 交互：单击切换；⌘1..9 跳转；⌘N / ⌘T 当前 workspace 新 session；⌘⇧N 新 workspace（目录选择器）；⌘W 关闭（有 agent 运行时二次确认）；⌘K 命令面板；⌘F 搜索（v1.1）；拖拽排序（v2）。
- M4 起（§17）：⌘T = ⌘N；右键菜单；⌘D / ⌘⇧D 分屏；⌘W = 关闭 pane，session 没有别的 pane 时归档（可从「归档」区找回）；单击 session 若已在某个 pane 则聚焦该 pane。
- 通知：未聚焦 session 进入 WaitingPermission / WaitingInput / Done / Error 时 macOS 通知 + Dock 角标（数量 = 需要关注的 session），可按 workspace 关闭。

### 8.4 配置

- `~/.config/berth/config.toml`，**启动时读一次**：改完要重启 `berth`（GUI）或 `berthd`（daemon 侧的段）才生效。热重载仍在 M5，代码里没有文件监听。示例：

```toml
[font]           family = "SF Mono"   size = 13
[terminal]       scrollback = 20000   shell_integration = "auto"
[persist]        snapshot_interval_s = 5   journal = false   max_restored_lines = 50000
[sidebar]        width = 280   preview_rows = 3   preview_hz = 4
[notify]         on = ["waiting_permission", "waiting_input", "done", "error"]
                 identity = "com.apple.Terminal"   # 通知身份的默认值：未打包的程序不能自报身份，只能借用
                                                   # 装了 Berth.app 之后改成 "io.github.xsser.berth"（§8.5）
[theme]          preset = "light"          # "light"（默认，白底）| "dark"
                 background = "#ffffff"    foreground = "#1f2328"
                 cursor = "#1f2328"        cursor_text = "#ffffff"
                 accent = "#b35c00"        # 「等授权」脉冲、状态行「N 需关注」、agent 图标、出错文案、聚焦 pane 边框
                                           # （未读圆点用的是蓝色 palette[4]，不走 accent）
                 ansi = ["#383a42", "#c84c40", ...]   # 正好 16 个
[agents.claude]  resume_command = "claude --resume {id}"
[[keybind]]      key = "cmd+k"   action = "command_palette"
```

- `[theme]` 全部键可选，示例里写的就是默认值（`preset` 决定这些默认值来自哪套预设）：
  - `preset = "light"`（默认）= 白底 + One Light 16 色，压暗按槽位的用途分档：正常色 0..=7 承载正文（`ls`、diff、编译器输出），按 WCAG 1.4.3 的 4.5:1，red / green / yellow / cyan / white 已压暗；高亮色 8..=15 只标记正文，按 1.4.11 的 3:1，只有 bright magenta 不够。`"dark"` = 此前的 Ghostty 默认（`#282c34` + Tomorrow Night）。其他值 warn 后按 `light` 处理。
  - 其余键在预设之上逐键覆盖。`cursor` / `cursor_text` 不给时分别跟随生效后的 `foreground` / `background`（Ghostty 语义）；`accent` 不给时按背景的 WCAG 相对亮度取（浅底 `#b35c00`，深底 `#de935f`），所以只改 `background` 也会带着 accent 一起走。
  - `ansi` 覆盖 0..=15，必须正好 16 个，否则 warn + 整段忽略；16..=255 仍按 xterm 色立方与灰阶从新的 16 色重建。
  - 颜色写法 `#rgb` / `#rrggbb`，大小写不敏感，`#` 可省。单个值非法**只跳过该键**并 warn（写明键名与原值），同段其余键照常生效；`foreground` 对 `background` 对比度低于 4.5:1 只告警，不改用户的值。
  - 窗口内其他颜色没有第二套配色：侧栏、预览块、分隔线与 pane 边框、通知条、egui 菜单/对话框全部由 `[theme]` 推导，按 `Theme::is_light()`（背景相对亮度 > 0.5）选深/浅两套混色系数。
  - 程序向终端查询颜色（OSC 4 / 10 / 11 / 12）得到的也是这套 `[theme]`：GUI 每次连上 berthd 都把它发过去（§5）。改了 `[theme]` 重启 GUI 即对之后的查询生效；已在运行、只在启动时查一次的程序（如 Codex）要重开。

### 8.5 macOS 应用打包

`packaging/make-app.sh` 由 `target/release` 产出 `dist/Berth.app`，不安装也不启动。

- **自包含**：`berth`、`berthd`、`berth-hook` 三个二进制都放进 `Contents/MacOS`。`client.rs`
  的 `find_berthd` 先找可执行文件旁边的同名文件，再找 `PATH`，所以 bundle 不依赖 PATH 就能
  拉起自己的 daemon。从 Finder 启动时 PATH 只有系统默认值，这一点是打包能成立的前提。
- **bundle id** `io.github.xsser.berth`，`CFBundleIconFile` 指向 `Resources/berth.icns`。
- **签名**：Apple Silicon 上二进制必须带签名才能运行。脚本先逐个签嵌套的可执行文件、再签
  bundle 本身（`--deep` 已废弃，不用），用 ad-hoc 签名（`--sign -`），最后 `codesign --verify
  --strict` 自检。本地构建的应用不带隔离属性，不经 Gatekeeper。
- **图标**：`assets/icon/make_icon.py` 按 macOS 图标网格（1024 画布内 824 超椭圆方块居中）
  画两份图稿——完整稿给 64px 及以上，简化稿（去掉提示符与分隔线、点和光标放大）给 16/32px，
  由 `build_icns.sh` 合成 `.icns`。单一图稿在 16px 下会糊成一团，这是分两份的原因。
  `assets/icon/make_logo.py` 用同一套几何生成 README 的 `assets/logo.svg`。
- **通知身份**：未打包的程序不能自报身份，所以 `[notify].identity` 默认借用
  `com.apple.Terminal`。装好 Berth.app 并 `lsregister` 之后改成自身 bundle id，通知才会显示
  为「berth」。这是 LaunchServices 层面的解析，不要求应用正在运行。
- **不打包的用法不受影响**：直接跑 `target/release/berth` 一切照旧，只是通知仍借用 Terminal
  的身份（除非 Berth.app 已装，此时该 bundle id 对两种跑法都解析得到）。

## 9. Agent 状态机（daemon）

| 输入信号 | 来源 | 转移 |
|---|---|---|
| `SessionStart` | Claude hook | kind=Claude，记 `session_id` / `transcript_path` / `cwd`，→ Idle |
| `UserPromptSubmit` | Claude hook | → Thinking |
| `PreToolUse { tool_name }` / `PostToolUse` | Claude hook | → ToolRunning(name) / → Thinking |
| `Notification { permission_prompt }` / `{ idle_prompt }` | Claude hook | → WaitingPermission / → WaitingInput |
| `Stop` / `SubagentStop` | Claude hook | → Done（用户再输入后 → Idle）/ 不变，记子代理计数 |
| `PermissionRequest` / `PermissionDenied` | Claude hook | → WaitingPermission(tool)（即时信号；`permission_prompt` 通知约 6s 后才来）/ → Thinking |
| `PostToolUseFailure` / `StopFailure` | Claude hook | → Thinking / → Error |
| `PreCompact` | Claude hook | → Compacting |
| `SessionEnd { reason }` | Claude hook | agent 离开：kind=Shell，→ Idle，保留 `session_id` / `transcript_path` 供「续接」展示（不是 Exited：shell 仍活着） |
| `agent-turn-complete` | Codex notify | → Done |
| OSC 133 A / C / D(exit) | shell 集成 | → Idle / Running(cmd) / Idle(+exit code)；若当前 kind 是 agent，则同为 agent 离开（kind=Shell） |
| 前台进程名 = claude / codex / node(claude) | `tcgetpgrp` + `proc_pidinfo` | 设 kind；无 hook 时输出活动 → Thinking，静默 ≥3s 且光标在行首 → Idle（confidence 0.5）；前台从 agent 变为非 agent → agent 离开 |
| 子进程退出 | PTY EOF | → Exited，status=Dormant（终态：任何信号都不改变，只有 Revive 重置） |

优先级按状态门控而非时间门控（`StateSource` 契约，见 berth-core `agent.rs`）：启发式不覆盖 hook 态，也不离开粘性状态（WaitingPermission / WaitingInput / Done / Error / Exited）；shell 集成提示符与前台进程离开是「agent 已不在前台」的证据，可结束 hook 态但走 agent 离开转移；迟到的 hook 不能把 Exited 复活。每次转移写入 SQLite `events` 表（时间线 / 复盘）。

## 10. 与 Claude Code / Codex 集成

- **关联**：daemon 向 PTY 注入 `BERTH_SESSION_ID`；`berth-hook` 从自身 env 读取（claude / codex 继承 shell 环境），随事件上报；带 id 但 daemon 不认识时按外部 id（claude session_id / codex thread_id）+ `cwd` 回退匹配（没有进程树匹配）。hook 进程环境里没有 `BERTH_SESSION_ID`（未设置、为空或不是合法 id）就不转发：hooks 装在全局配置里，berth 之外的 agent 也会触发，按 cwd 回退会把它们的事件错记到同目录的 berth 会话上；`codex --chain` 与 `statusline` 包住的原命令照常执行。
- **Claude hooks 安装**（显式）：`berth setup-hooks claude` 读取 `~/.claude/settings.json`，对每个事件在数组**末尾追加** `{ "type": "command", "command": "berth-hook claude" }`，先打印 JSON diff，确认后写入并备份原文件；`--undo` 按备份恢复。已有的 Notification / PreToolUse / SessionStart 条目原样保留。
- **statusline tee**（可选）：把现有 `bash ~/.claude/scripts/statusline.sh` 包成 `berth-hook statusline -- bash ~/.claude/scripts/statusline.sh`：stdin JSON 复制一份发 daemon（`session_id`、`model`、`context_window.used_percentage`、`cost.total_cost_usd`、`workspace.project_dir`），原样透传给原脚本，输出不变。
- **Codex**：`notify` 是单命令，当前已指向 Computer Use 客户端；`berth setup-hooks codex` 改为 `berth-hook codex --chain "<原命令>"`，我们收到事件后再 exec 原命令并透传 stdin / argv。若 2026 版 Codex 已有多 hook 机制，则用原生机制追加（待调研结论）。
- **Resume**：Claude 用 `claude --resume <session_id>`（cwd 必须一致，否则 claude 找不到 transcript）；Codex 待核实。
- **Session 发现**（无 hook 时）：`~/.claude/projects/<编码cwd>/*.jsonl` 首行元数据（`sessionId` / `cwd` / `gitBranch`），只读，用于「未在 berth 里跑过的历史会话」列表（v1.1）。

## 11. 安全与隐私边界

- 快照 / journal 可能含命令输出里的敏感内容：目录 0700、文件 0600、per-session 关闭、`purge`；不做自动脱敏（会破坏还原精度），在 UI 明示。
- `berth-hook` 只转发 JSON，不落盘、不打印；daemon 事件表只存事件名、工具名、时间，不存 prompt / tool_input 正文。
- socket 仅本用户可访问；协议无远程监听；hook 消息不含凭据。
- OSC 52 读剪贴板默认拒绝；写需配置显式开启（与 Ghostty 默认一致）。
- 不静默修改用户 hooks / statusline / codex notify（D6）。

## 12. 技术栈与版本（crates.io 2026-09-26 实测）

| 用途 | crate | 版本 | 备注 |
|---|---|---|---|
| VT | alacritty_terminal | 0.26.0 | 2026-04 更新，serde feature |
| PTY | portable-pty | 0.9.0 | wezterm 出品 |
| 窗口 | winit | 0.30.13 | 2026-09 |
| GPU | wgpu | 30.0.1 | Metal 后端 |
| 文字 | cosmic-text / swash / etagere | 0.19.0 / — / — | shaping + 回退 + 光栅 |
| 侧栏 UI | egui + egui-wgpu | 0.36.2 | 需手动加载 CJK 字体 |
| 异步 | tokio | 1.53 | daemon |
| 序列化 | serde + postcard + zstd | 1.0.229 / 1.1.3 / — | 协议与快照 |
| 存储 | rusqlite (bundled) | — | 元数据 / 事件 |
| IPC | interprocess 或 tokio UnixStream | 2.4.4 | 优先 tokio 原生 |
| 通知 | notify-rust / mac-notification-sys | 4.18 | macOS 通知 |
| 进程 | libproc / nix | 0.14 / 0.31 | 前台进程、cwd |

备选栈（若 M0 否决 D3）：gpui 0.2.2（crates.io，2025-10）/ git 版 + gpui-component 0.6.6；sugarloaf 0.5.28（Rio 渲染器）仅作文字后端。

## 13. 里程碑与验收证据

| 里程碑 | 交付 | 验收证据（必须实际运行） |
|---|---|---|
| M0 Spike（1–2 天） | winit+wgpu 窗口画静态网格（中英 emoji 混排），egui 侧栏面板，IME 预编辑 | 截图 + 帧率日志；中文输入法 commit 到日志；否决则切 GPUI |
| M1 daemon 核心 | `berthd` + 协议 + `berth attach` 文本测试客户端 | 集成测试：起 session → `echo hi` → `FetchLines` 含 hi；断开重连状态一致 |
| M2 GUI 终端 | 可日常使用的单 session 终端 | vttest 子集；vim / htop / claude TUI 手工清单；resize 无错位 |
| M3 侧栏 + workspace + agent 状态 | hooks CLI、`setup-hooks`、zsh 集成、进程树回退、通知 | 3 个 claude 并行，状态转移与实际一致；hook 未装时启发式标注为推断 |
| M4 持久化 L2 | 快照 / 恢复 / revive / resume | `kill -9 berthd` → 重启 → 历史可见 → revive 续写在下方；`claude --resume` 成功 |
| M5 打磨 | 配置热重载、搜索、URL、launchd 可选（主题已随 §8.4 `[theme]` 落地，`.app` 打包已随 §8.5 落地） | 冷启动 <300ms；30 session 预览 CPU <5% |

分工（按既有约定）：Fable 5 设计 / 验收 / 审核；opus（effort max）或 codex 在各自 worktree 按 crate 所有权并发实现（`berth-vt` / `berth-daemon` / `berth-app` 三条线，`berth-core` 先冻结接口）。

## 14. 风险与待验证（2026-09-26 更新）

| 风险 | 状态 | 证据 / 验证方式 |
|---|---|---|
| alacritty `Handler` 是否有未处理 OSC 回调 | **已否定** → 保留预扫描器 | vte 0.15 `ansi.rs` `osc_dispatch` 未知分支只调 `unhandled()` 打 debug 日志；`Handler` 无 OSC 7 方法 |
| alacritty serde / damage API | **已肯定** | 0.26.0 源码：`default = ["serde"]`，`Grid`/`Cell` 派生 serde；`Term::damage()` 返回 `TermDamage::{Full, Partial}`，`reset_damage()` |
| cosmic-text 按 cell 定位是否破坏连字 / 簇 | 待 M0 实测 | app-spike 截图 |
| winit IME 在 macOS 26 的行为 | 待人工验证 | app-spike 保留 `BERTH_IME_DEBUG` |
| libghostty-vt 成熟度 | **部分肯定，暂不采用** | 有非官方 Rust 绑定 `libghostty-vt` 0.2.1（MIT OR Apache-2.0）但需联编 Ghostty Zig 源码、pre-1.0 API 漂移；作为 v2 可选 VT 后端 |
| gpui 备选可行性 | **可行但有缺口** | gpui Apache-2.0（与 Zed GPL 主体分离），gpui-component 有 Sidebar/Tree/Resizable；CJK IME 无成熟证据（第三方 Crux 项目列为待攻克）|
| Claude hooks 事件 / 字段 | **已肯定** | 约 32 个事件；通用字段含 `prompt_id`；`notification_type` 高置信值 `permission_prompt` / `idle_prompt` / `auth_success` / `elicitation_dialog`，其余低置信值按通用分支处理 |
| Codex hook 机制 | **旧机制为准** | 稳定版仍是 `notify` argv JSON、仅 `agent-turn-complete`；新 hooks 过渡中（部分 handler 未实现），本期不接入 |
| 快照体积（100k 行长输出） | 待 M4 基准 | — |
| 「重启后从磁盘恢复终端内容」无业界先例 | 已确认为差异化点 | wezterm mux 靠进程常驻，跨重启依赖第三方插件；Conductor / Superset / Warp 均未见此能力 |

## 15. 待用户决策

1. 渲染栈：D3（winit + wgpu 自研 + egui）还是 GPUI？或先做 M0 spike 再定。
2. 持久化语义：D1 + D4（daemon 常驻 + 快照）还是仅快照（无 daemon，关窗即杀）？
3. v1 范围：是否必须包含 splits / 多窗口 / Linux？（推荐都放 v2）
4. 项目名与路径：工作名 `berth`（泊位：session 停靠处），路径 `~/projects/berth`；备选 `quay` / `moor` / 自定义。
5. 许可证：MIT / Apache-2.0 双许可（避免引入 Zed GPL 代码）。

## 16. 调研结论摘要（2026-09-26，来源见调研报告）

- **VT 核心**：alacritty_terminal 0.26（Apache-2.0，Zed 生产使用）为 v1 选择；libghostty-vt 保留为 v2 可选后端。
- **UI 栈**：D3 按用户决策执行；gpui + gpui-component 作为 M0 失败时的备选，其 Sidebar/Tree 组件契合本项目，CJK IME 需专项验证。
- **持久化参照**：wezterm mux server 的 SequenceNo 脏区跟踪 + `ClientPane` 缓存远端状态，与本设计的 `seq` + 客户端镜像一致；磁盘快照恢复需自研。
- **Agent 集成**：Claude 9 个核心事件足以驱动状态机；Codex 短期只用 `notify`；Codex 会话文件 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` 可供 v1.1 会话发现。
- **竞品范式**：Conductor / Superset / Warp 都采用「workspace 侧栏 + 实时预览 + 状态徽标」；Warp 2026-04 起开源（MIT + AGPLv3）；Crystal 已停更。本项目差异化 = 关窗不杀 agent + 重启后仍可看并恢复内容。
- **现成终端 widget**：iced_term / egui_term 均标注开发中，不作生产依赖。

## 17. M4：归档、右键菜单与分屏（2026-09-27 用户新增需求）

用户原话：「增加归档功能，某个标签想删除就点下删除就没了，或者 1 周以上自动关闭这个标签的记录；归档以后可以找回；可以分屏，把功能增加到右键里。」分屏原定 v2，现按用户要求提前。

### 17.1 归档模型（daemon 为准）

- `SessionMeta.archived_at_ms: Option<i64>`（追加为最后一个字段，`#[serde(default)]`）。归档与 `Live | Dormant | Restored` 正交，但**归档的 session 一定不是 Live**：归档 live session 时 daemon 先 `kill`（等 PTY 收尾，最长沿用现有 30 s 上限）再打标记。
- 语义：**归档** = 从 workspace 列表移到侧栏「归档」区，历史、快照、事件、agent 信息原样保留；**恢复** = 清标记，回到原 workspace，以 Dormant/Restored 呈现，Enter/Revive 照旧；**彻底删除** = 现有 `Delete`（purge），需二次确认。
- 归档的 session：不接受 `Attach`/`Input`/`Resize`/`Revive`/`Subscribe`（返回 `Error`「已归档，先恢复」）；不参与 hook 路由的 cwd 兜底；不计入通知、Dock 角标、「需关注」；`MoveSession`/`Rename`/`FetchLines`/`ListEvents`/`MarkRead`/`Delete` 允许（归档区要能读历史、彻底删除）。`Unarchive` 把 `last_active_ms` 刷成当前时间，否则下一轮扫描会立刻把它再归档。`DaemonStatus.sessions_total` 仍是「daemon 知道的全部 session」（含归档）；排除归档的是「需关注」计数与通知。
- 自动归档（daemon 扫描：启动 60 s 后一次，之后每 10 min）：配置 `[archive] auto_after_days = 7`（0 = 关闭）。条件：`now − last_active_ms > days`，且满足其一：(a) 非 live；(b) live 且 `agent.kind` 不是 agent、`agent.state` 不 busy、且没有前台命令（前台进程就是 shell 本身）。live 的先 kill 再归档。`[archive] purge_after_days = 0`（0 = 永不；>0 时 `now − archived_at_ms > days` 的归档 session 自动 purge）。每次自动归档/清理各写一条 `info!` 日志。
- 协议 v3（只追加）：`Request::Archive { session }`、`Request::Unarchive { session }` → 成功回 `Event::SessionUpdated(meta)`；daemon 主动归档/清理走现有 `SessionUpdated` / `SessionRemoved` 广播。`PROTOCOL_VERSION = 3`；旧 GUI 对新 daemon 沿用现有 Incompatible/横幅与 `berth debug restart-daemon` 流程。
- 快照：meta 是 JSON（格式 2），新字段靠 `serde(default)`，格式号不升。registry（SQLite）沿用 `meta_json`，`list_sessions` 保持返回全部（含归档），由客户端过滤。
- CLI：`berth list` 默认不列归档，末尾计数「N 个已归档」；`berth list --archived` 只列归档（含归档时间）；`berth debug archive <sid>` / `berth debug unarchive <sid>`。

### 17.2 右键菜单（GUI，egui `context_menu` / 指针处 popup）

- 侧栏 session 行：`在右侧分屏打开`、`在下方分屏打开`（已在分屏中时禁用，提示「已在分屏中」）、`重命名…`、`标记已读`、`归档`（live 且 agent 运行/命令忙时二次确认，否则立即）、非 live 时 `恢复运行`（= Revive）。
- 侧栏「归档」区（折叠标题「归档 (N)」，默认折叠，放在 workspace 列表之后、footer 之前）：每行 = 标题 · workspace 名 · 归档时间（相对）；右键/悬停：`恢复`、`彻底删除…`（确认框沿用现有 Confirm::Delete）。
- workspace 头：`新建 session`、`重命名…`、`删除 workspace…`（其下还有任何 session 时禁用，**归档的也算**，提示「先移走或彻底删除其中的 session」）。理由：连同归档一起删会产生指向已删 workspace 的孤儿，恢复后无处可归。
- 终端区域右键：`复制`（有选区时）、`粘贴`、`向右分屏`、`向下分屏`、`从分屏移除`（保留 session）、`关闭 pane`（= ⌘W 语义）、`归档 session`、`重命名…`。程序开启鼠标上报时，⇧+右键透传给程序，普通右键仍开菜单。

### 17.3 分屏（仅 GUI 侧，daemon 不改）

- 模型：`PaneTree = Leaf(SessionId) | Split { axis: Horizontal | Vertical, ratio: f32 ∈ [0.2, 0.8], first: Box<PaneTree>, second: Box<PaneTree> }`；一个聚焦 leaf；侧栏「当前」= 聚焦 leaf 的 session；**同一 session 不能同时出现在两个 pane**（菜单禁用；侧栏单击它则聚焦已有 pane）。
- 快捷键：⌘D 向右分屏、⌘⇧D 向下分屏（新 session：同 workspace，cwd = 当前 session 的 cwd）；⌘W 关闭聚焦 pane（该 session 没有其他 pane 时：live → kill + 归档，agent 运行/命令忙时二次确认；非 live → 归档）；⌥⌘← → ↑ ↓ 在 pane 间移动焦点；单击 pane 聚焦。
- 每个 pane 独立：`SessionView`、Attach/Resize（自己的 cols×rows）、选区、滚动、IME 候选框位置；分隔条 6 px 可拖动改 ratio；聚焦 pane 有 1 px 高亮边框，非聚焦为暗色。daemon 侧同一连接可对多个 session 各自 Attach（`attached: HashSet<ConnId>` 按 session 独立），不需要协议改动。
- 渲染：同一帧内对每个 pane 各做一次 prepare + render，共享字形 atlas；性能门槛：release 下 4 个 pane 各 120×40，帧总耗时 p99 < 4 ms（沿用 M0 方法与 `--stats`）。
- 布局持久化：`<data dir>/gui-state.json`（`{ "version": 1, "layout": <PaneTree，leaf 存 session id>, "focused": <sid> }`），GUI 退出与每次布局变化后写（原子写：临时文件 + rename）。重开 GUI 恢复分屏；引用的 session 不存在或已归档 → 从树中剔除并收拢；树空 → 与今天一样打开一个 session。
- 侧栏单击：session 已在某 pane → 聚焦该 pane；否则替换聚焦 pane 的 session（今天的行为）。

### 17.4 配置追加（§8.4）

```toml
[archive]
auto_after_days = 7    # 0 = 不自动归档
purge_after_days = 0   # 0 = 归档永不自动删除
```

### 17.5 验收证据（实现者提供，独立审查者复核）

- daemon：扫描条件的单元测试（注入时钟）；Archive/Unarchive/拒绝 Attach 的协议测试；registry 与快照往返保留 `archived_at_ms`；`berth debug` 端到端：new-session → archive → `list` 不显示且计数 +1 → `list --archived` 显示 → unarchive → revive 正常。
- GUI：`PaneTree` 的分裂/关闭/焦点导航/序列化/剔除测试；controller 的 ⌘W 语义测试（live 忙 → 确认；否则 kill + archive；非 live → archive）；右键菜单动作映射测试；隐藏调试参数 `berth --screenshot x.png --session A --split-right B [--split-down C]` 产出多 pane 截图；`--stats` 4 pane PASS 行。
