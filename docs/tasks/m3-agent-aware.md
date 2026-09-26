# 任务 M3：agent 感知闭环（hooks 安装器、shell 集成、状态 UI）

前置：M2 联调通过。分支 `feat/m3`。

## 1. `berth setup-hooks`（berth-app，显式、可撤销）
- `claude`：读 `~/.claude/settings.json`；对 `SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`Notification`、`Stop`、`SubagentStop`、`PreCompact`、`SessionEnd` 各事件在 `hooks[<event>]` 数组**末尾追加**一个 matcher 为空的条目 `{ "hooks": [{ "type": "command", "command": "<abs>/berth-hook claude" }] }`；已存在则跳过（幂等）。先打印 JSON diff，`--yes` 才写入；写前备份到 `~/.claude/settings.json.bak.berth-<ts>`；`--undo` 恢复最近备份。
- `codex`：读 `~/.codex/config.toml` 的 `notify`；若已存在则改写为 `["<abs>/berth-hook", "codex", "--chain", <原数组...>]`，否则 `["<abs>/berth-hook", "codex"]`；同样 diff / `--yes` / 备份 / `--undo`。
- `statusline`（可选 `--statusline`）：把 `statusLine.command` 包成 `berth-hook statusline -- <原命令>`。
- 绝不静默写入；`berth doctor` 只读报告安装状态。

## 2. Shell 集成（zsh 优先，bash/fish 后续）
- 资源文件 `crates/berth-daemon/resources/shell-integration/zsh/.zshenv`+`berth-integration.zsh`：仿 Ghostty 的 `ZDOTDIR` 垫片——daemon 起 shell 时若 `shell_integration != none` 且 `$SHELL` 是 zsh，设 `ZDOTDIR=<资源目录>` 并保存原 `ZDOTDIR` 到 `BERTH_ORIG_ZDOTDIR`；垫片先 source 用户原 `.zshenv/.zshrc`，再注册 `precmd`/`preexec` 发 OSC 133 A/B/C/D 与 OSC 7（cwd）。
- 只发序列，不改 prompt 外观；用户可 `[terminal] shell_integration = "none"` 关闭。

## 3. daemon 侧
- `Cwd` 事件驱动 `SessionMeta.cwd` 与标题；OSC 133 驱动 shell 状态（Idle/Running）；前台进程名驱动 `AgentKind`（`claude`/`node`+cwd 下有 `.claude` 视为 Claude，`codex` → Codex）。
- 未安装 hooks 时的启发式：输出活动 → Thinking（confidence 0.5），静默 ≥3s 且最近 OSC 133 A → Idle；**不产生** WaitingPermission。

## 4. UI
- 徽标与颜色；`source == Heuristic` 淡色；hover 显示 `since` 与来源。
- Dock 角标 = needs_attention 的 session 数（`NSApp.dockTile.badgeLabel`，经 objc2 或 osascript；失败则忽略）。
- `agents.claude.resume_command` 默认 `claude --resume {id}`；Restored session 的 Revive 菜单显示该命令。

## 验收
1. `berth setup-hooks claude` 先显示 diff，不加 `--yes` 不写；`--yes` 后 `~/.claude/settings.json` 原有条目完整保留；`--undo` 恢复字节级一致。
2. 三个 `claude` 并行：侧栏状态转移与真实一致（截图 + 事件表导出）；等授权时收到 macOS 通知。
3. 未装 hooks 的 session：状态显示为推断样式，且从未出现 WaitingPermission。
4. zsh 集成：`cd` 后侧栏 cwd 跟随；命令执行期间徽标为 Running。

## 5. M1/M2 之后的修订（2026-09-27，以本节为准）
- 分支 `feat/m3`，worktree `~/projects/berth-wt-m3`，从 M2 合并后的 main 建。所有权：`crates/berth-app/**`（setup-hooks、UI）与 `crates/berth-daemon/**`（shell 集成资源与 spawn 环境）；berth-core 只加法；不改 DESIGN.md / progress.md。
- §1 事件列表以官方文档为准，实现前用 claude-code-guide 或官方文档核对一遍（至少补 `PermissionRequest`；`PostToolUseFailure`、`StopFailure`、`PermissionDenied` 若文档存在也装）。berth-hook 永远静默退出 0、不输出 JSON，因此 `PermissionRequest` 这类可给决策的 hook 不会被它干扰。daemon 对未知事件已走 `Other` 分支（M1）。
- §1 的 `berth-hook` 路径：默认取与 `berth` 可执行文件同目录的 `berth-hook` 绝对路径，找不到则报错不写；`--hook-path` 可覆盖。`--undo` 恢复最近一次 `*.bak.berth-<ts>` 且字节级一致；`--yes` 之外一律只打印 diff。
- §3 daemon 侧状态机、agent_left、前台进程识别、启发式、hook 路由都已在 M1 完成并审查；M3 只需核对 `Cwd`（OSC 7）→ `SessionMeta.cwd`/标题是否已接通，缺则补，不重写状态机。`node` 托管的 claude 识别（读 argv）仍列为已知缺口，除非顺手能做。
- §4 UI 的经过时间/来源样式/kind 切回 Shell 已在 M2 做完；M3 补 hover 详情、Dock 角标、Restored 会话 Revive 菜单显示 resume 命令（来自 `agents.<kind>.resume_command` 模板，只展示不执行）。
- **验收不得动真实配置**：`berth setup-hooks` 的 diff/`--yes`/`--undo` 用 `HOME=/tmp/berth-m3-home` 下的假 `settings.json` / `config.toml` 验证（预置带 env、其他 hooks、注释外的复杂内容，证明原有条目字节级保留）。真实 `~/.claude/settings.json`、`~/.codex/config.toml`、shell profile 一律不写；是否给用户真实安装由用户自己运行 `berth setup-hooks --yes` 决定。
- 验收 2/3 的真实 agent 测试：给 claude 传独立的设置文件（先用 claude-code-guide 核对 `claude --settings <file>` 的确切语义与 hooks 是否从该文件生效），文件内容就是 setup-hooks 生成的 hooks 段；不能确认时记「需用户验证」。每个 claude 会话最多一条极短 prompt；权限等待用一个需要授权的工具调用触发（例如让它写一个 /tmp 文件），看到 WaitingPermission 与通知后 Esc 拒绝，不要替用户批准任何权限。
- 验收 4 的 zsh 集成：用 `BERTH_DATA_DIR` 隔离的 daemon 起 zsh，`ZDOTDIR` 垫片必须先 source 用户原 `.zshenv/.zshrc`（若 `BERTH_ORIG_ZDOTDIR` 未设则用 `$HOME`）；证明用户 prompt/alias/函数不变（对比 `alias`、`echo $PROMPT` 输出），且 OSC 133/7 序列由 daemon 收到（用 `berth debug dump`/事件表或 daemon debug 日志）。
- 屏幕可能锁定：继续用 M2 的 `--screenshot`、`--stats`、`berth debug ...` 做脚本化验收；present/IME/鼠标类留给用户。
