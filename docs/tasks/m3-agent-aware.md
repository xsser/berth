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
