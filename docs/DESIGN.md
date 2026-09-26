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
- 交互：单击切换；⌘1..9 跳转；⌘N 当前 workspace 新 session；⌘⇧N 新 workspace（目录选择器）；⌘W 关闭（有 agent 运行时二次确认）；⌘K 命令面板；⌘F 搜索（v1.1）；拖拽排序（v2）。
- 通知：未聚焦 session 进入 WaitingPermission / WaitingInput / Done / Error 时 macOS 通知 + Dock 角标（数量 = 需要关注的 session），可按 workspace 关闭。

### 8.4 配置

- `~/.config/berth/config.toml`，热重载（`notify` 监听）。示例：

```toml
[font]           family = "SF Mono"   size = 13
[terminal]       scrollback = 20000   shell_integration = "auto"
[persist]        snapshot_interval_s = 5   journal = false   max_restored_lines = 50000
[sidebar]        width = 280   preview_rows = 3   preview_hz = 4
[notify]         on = ["waiting_permission", "waiting_input", "done", "error"]
[agents.claude]  resume_command = "claude --resume {id}"
[[keybind]]      key = "cmd+k"   action = "command_palette"
```

## 9. Agent 状态机（daemon）

| 输入信号 | 来源 | 转移 |
|---|---|---|
| `SessionStart` | Claude hook | kind=Claude，记 `session_id` / `transcript_path` / `cwd`，→ Idle |
| `UserPromptSubmit` | Claude hook | → Thinking |
| `PreToolUse { tool_name }` / `PostToolUse` | Claude hook | → ToolRunning(name) / → Thinking |
| `Notification { permission_prompt }` / `{ idle_prompt }` | Claude hook | → WaitingPermission / → WaitingInput |
| `Stop` / `SubagentStop` | Claude hook | → Done（用户再输入后 → Idle）/ 不变，记子代理计数 |
| `PreCompact` / `SessionEnd` | Claude hook | → Compacting / → Exited |
| `agent-turn-complete` | Codex notify | → Done |
| OSC 133 A / C / D(exit) | shell 集成 | → Idle / Running(cmd) / Idle(+exit code) |
| 前台进程名 = claude / codex / node(claude) | `tcgetpgrp` + `proc_pidinfo` | 设 kind；无 hook 时输出活动 → Thinking，静默 ≥3s 且光标在行首 → Idle（confidence 0.5） |
| 子进程退出 | PTY EOF | → Exited，status=Dormant |

优先级：hook 事件 30s 内有效期内覆盖启发式；启发式不能覆盖 WaitingPermission（避免把「等授权」误判成 Idle）。每次转移写入 SQLite `events` 表（时间线 / 复盘）。

## 10. 与 Claude Code / Codex 集成

- **关联**：daemon 向 PTY 注入 `BERTH_SESSION_ID`；`berth-hook` 从自身 env 读取（claude / codex 继承 shell 环境），随事件上报；缺失时按 `cwd` + 进程树回退匹配。
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
| M5 打磨 | 配置热重载、主题、搜索、URL、`.app` 打包、launchd 可选 | 冷启动 <300ms；30 session 预览 CPU <5% |

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
