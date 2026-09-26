# 任务 integrate（M2）：GUI ↔ daemon 联调，替换 fixture 为真实会话

前置：`feat/vt`、`feat/daemon`、`feat/app` 已合并到 `main`，`cargo test --workspace` 全绿。分支 `feat/integrate`。所有权：`crates/berth-app/**`；daemon 侧缺口以「加法」补在 `crates/berth-daemon/**` 并在报告列出。

## 1. `client.rs`（berth-app）
- 连接 `Paths::resolve().socket`；失败则启动 `berthd`（查找顺序：与 `berth` 同目录 → `$PATH`），`--foreground` 不带，detached；2s 内重试连接。
- 读线程：`UnixStream` → `FrameReader` → `DaemonMsg` → `EventLoopProxy::send_event(UserEvent::Daemon(msg))`；写线程：`crossbeam` 通道 → `encode_frame`。请求 id 递增，`reply_to` 关联到回调。
- 镜像状态 `SessionView`：`StyleTable`、可视行、cursor、modes、`seq`（丢弃乱序）、历史缓存 `BTreeMap<u64, LineSnapshot>` + 按需 `FetchLines`（滚动到缓存边界前 200 行预取）。

## 2. 主视图
- 聚焦 session：`Attach{dims}` → 首个 full `Screen`；`Resize` 随窗口/侧栏宽度变化（防抖 50ms）；键盘经 `input.rs` 编码为 `Input`；IME commit 同路径；粘贴按 `BRACKETED_PASTE` 包裹。
- 滚动：trackpad/滚轮改变本地 `display_offset`，从历史缓存渲染；新输出到达且用户在底部时自动跟随；`ALT_SCREEN` 且 `ALTERNATE_SCROLL` 时转成方向键。
- 选择与复制：客户端在镜像行上做（字/行/块三种），⌘C 复制 `text_trimmed` 拼接；`wrapped` 行不插换行。
- 鼠标模式：`modes.mouse_reporting()` 时按 SGR 编码转发；按住 ⇧ 强制本地选择。
- 光标：随 `modes.SHOW_CURSOR` 与 `cursor.shape`；失焦画 HollowBlock。

## 3. 侧栏（真实数据）
- `ListWorkspaces`/`ListSessions` 初始化，之后靠 `WorkspaceUpdated`/`SessionUpdated`/`SessionRemoved`/`AgentChanged`/`Exited` 增量更新。
- 可见 session 订阅 `Preview{rows:3,max_hz:4}`，滚出视野则 `Unsubscribe`。
- 徽标、经过时间（`agent.since_ms`）、未读点、状态来源（Heuristic 用虚线/淡色）。
- 快捷键：⌘N 新 session（当前 workspace，cwd=workspace root）、⌘⇧N 新 workspace（目录选择：`rfd` 或 osascript）、⌘W 关闭（agent busy 时二次确认）、⌘1..9 跳转、⌘K 命令面板（v1.1 可留空壳）。
- Dormant/Restored 分组：显示只读历史，提供「Revive: shell / resume agent」按钮。

## 4. 通知
- `AgentChanged` 且 `state.needs_attention()` 且该 session 未聚焦（或窗口失焦）→ `mac-notification-sys` 通知，标题 `<workspace>/<session>`，正文状态文案；按 `[notify].on` 过滤；同一 session 30s 内不重复。

## 5. CLI
- `berth list`：表格输出 workspace / session / status / agent state / cwd。
- `berth doctor`：daemon 可达、socket 权限、hook 是否安装（只读检查 `~/.claude/settings.json`）、shell 集成是否生效。

## 验收（全部真实运行，记录截图路径与命令）
1. 启动 `berth`：自动拉起 `berthd`，新建 session 后输入 `ls -la` 显示正确；`vim`、`htop` 渲染与 resize 无错位。
2. 在 session 里跑 `claude`（用户已装），TUI 渲染正常，输入中文 prompt 成功。
3. 关闭 `berth` 窗口再打开：screen 与 scrollback 无损，光标位置一致。
4. `pkill -9 berthd` 后打开 `berth`：session 显示为 Restored，历史可滚动；Revive 后新 shell 输出接在历史下方。
5. 三个 session 同时输出（`yes | head -c 10M` 等），侧栏预览更新且主视图不掉帧（记录帧时间）。
6. `cargo test --workspace` 全绿，clippy 无告警。

## 6. 审查后追加的约束（2026-09-27，daemon 审查结论）
- 连接后第一帧必须是 `Hello{role: Gui, ...}`；daemon 按角色鉴权：Gui/Cli 不得发送 `Request::Hook`，Hook 角色只能 Hello/Hook。客户端遇到 `Event::Error` 要显示而不是静默丢弃。
- Revive 只发 `Revive{mode: ResumeAgent}` 等协议消息，`external_id` 由 daemon 校验（`[A-Za-z0-9._-]{1,128}`）并以 argv 方式启动；客户端绝不拼接 shell 命令字符串。
- `FetchLines` 单次上限 5000 行（daemon 侧 `MAX_FETCH_LINES`），客户端分页预取；`Input` 有 1 MiB 背压上限，粘贴超大文本要分块并等待 `Ok`。
- 同一 session 的 `Screen`/`Preview` 可能被 daemon 合并（outbox coalesce），客户端只信任 `seq`，不假设每次输出都对应一帧。
- agent 状态显示 `source`：Hook 实色、ShellIntegration 普通、Heuristic 淡色/虚线；`AgentChanged` 里 kind 从 Claude/Codex 变回 Shell 表示 agent 已退出（`agent_left`），侧栏徽标随之切回 shell 图标。
- `berth doctor` 对 `~/.claude/settings.json`、`~/.codex/config.toml` 只读；任何写入都属于 M3 `berth setup-hooks` 且必须显式确认。
