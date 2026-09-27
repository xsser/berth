# 进度

## 2026-09-26
- 设计文档 `docs/DESIGN.md` v0.1；用户确认四项决策（winit+wgpu+egui、daemon+快照、v1 只做核心四目标、名字 berth）。
- 安装 Rust 1.98.1（rustup，用户态，未改 shell profile；使用 `export PATH="$HOME/.cargo/bin:$PATH"`）。
- workspace 骨架：`berth-core`（冻结接口 + 单元测试）；`berth-vt` / `berth-store` 接口桩；`berthd` / `berth-hook` / `berth` 入口桩。
- 已核实：vte 0.15 未知 OSC 无回调 → 预扫描器路线成立；egui-wgpu 0.36 与 cosmic-text 0.19 均锁 wgpu 30。

## 2026-09-27
- `feat/vt` 合并到 main（HEAD fec77f0 之前）：59 单元 + 11 真实 PTY 测试；独立审查 0 high、2 medium、1 low 均已修复（前台进程组组长退出后的成员枚举、Drop/reap 出错路径升级信号、无 pid 分支 wait）。
- 已知缺口（vt）：alt screen 期间读不到主屏 scrollback；OSC 10/11 用默认调色板回答；Linux 进程信息未实测。
- 已知缺口（daemon）：前台进程只按进程名识别，经 node 启动的 claude 需读 argv；hook 路由没有进程树回退（hook pid → 会话子进程），环境里缺 BERTH_SESSION_ID 时只能靠 external id / cwd 匹配；`hook_recent` 用墙钟时间，系统时间跳变会影响 hook 优先窗口；server 没有连接数上限和空闲超时；ResumeAgent 的相对程序名按 daemon 的 PATH 解析，由 launchd 拉起时可能要在 config 写绝对路径；待写输入预算只计负载字节，不计每条消息的开销；berth-vt 没有独立的输入写端，写线程卡在写入时 Kill/停止改为直接 killpg 进程组，给 PtyHandle 加一个 `input_writer()` 可去掉这条回退；Linux 上子进程已回收而 tty 仍被后台进程占着、写线程又卡住时，会话要等写入失败才收尾，卡住的字节也会占着下一次 revive 的预算（macOS 会话首进程退出即 revoke tty，不受影响）。
- `feat/daemon`（bdd122d）与 `feat/app`（a736fc5）门禁均已复核通过，等待对抗审查后合并。
- M0 结论：D3 成立。release 下 120×40 每帧 2.6 ms（含 GPU），缺字 0；present 路径与真实 IME 需用户解锁后人工验证。
- 副作用：早期 `sh -i` PTY 测试向 `~/.bash_history` 追加了 104 行测试命令（已改用临时 HISTFILE），清理待用户确认。
- `feat/app` 合并到 main（a2d37aa）：独立审查 3 项均修复（IME 组合期键路由改为纯函数 `input::decide_key` + 测试；无 GPU 计时不给 PASS/FAIL；零宽组合字符归基字符格）。合并后 `cargo test --workspace` 全绿（app 69、core 13、vt 59+11），clippy 无告警；fmt 仅剩 daemon/hook/store 三个骨架桩有差异，随 `feat/daemon` 合并消失。
- `feat/daemon`（d8385bb）对抗审查：3 high（Revive 未校验 external_id 可注入命令、FetchLines 算术溢出可 panic actor、客户端角色未鉴权）+ 若干 medium（OSC 133 无条件覆盖粘性状态、outbox 合并丢 reply_to、Gui/Cli 可伪造 Hook、purge 顺序、statusline stdin 无期限、codex --chain exec 失败码），14 项已发回 impl-daemon 修复，合并前逐项复核。
- berth-core：`ensure_dirs` 对单独创建的 socket 父目录设 0700（568311e）；`StateSource` 契约注释写明 heuristics 不覆盖 hook 态、OSC 133/前台进程离开走 agent_left 转移（6fa3b5b）。
- M2 任务书 `docs/tasks/integrate.md` §6 追加审查得出的协议约束。
- `feat/daemon` 合并到 main（b293c61）：19 项审查发现全部修复并经独立复核（Revive 白名单 id + argv exec、FetchLines 饱和运算 + actor catch_unwind、双向角色鉴权、statusline 1s 期限、codex --chain 127、并发 shutdown 5s、writer 线程 + 1 MiB 背压、状态机 agent_left/Exited 终态/hook:late、outbox reply_to、purge 顺序、hook 50 ms 硬预算）。合并后 workspace 全绿：app 69、core 13、daemon 50+13、hook 8+7、store 11、vt 59+11；clippy、fmt 干净。M1 完成。
- 设计澄清：`AgentState::Exited` 只表示 PTY 子进程退出（⇔ `SessionStatus::Dormant`），Claude `SessionEnd` 走 agent 离开转移（DESIGN §9，e147a89）。
- 下一步 M2：`feat/integrate`（worktree `~/projects/berth-wt-integrate`），按 `docs/tasks/integrate.md`（含 §6 审查约束与登录 shell PATH 要求）。
- `feat/integrate` 合并到 main（0a9831b）：M2 完成。GUI 跑在 berthd 上（client 读写线程 + generation 重连、controller、真实数据侧栏与 Preview 订阅、通知、`berth list`/`berth doctor` 只读、隐藏 `berth debug` 脚本化验收）。独立审查 1 medium（触顶后历史缓存陈旧）已修（缓存代号 + 旧代回包丢弃 + 红检测试），1 low（粘贴任务全局单例）记入缺口。workspace 311 测试全绿（app 139、core 13、daemon 63、hook 15、store 11、vt 70）。验收证据 `/tmp/berth-m2/shots/`（锁屏下 offscreen）；需用户解锁验证：present/vsync、真实 IME、鼠标拖拽、目录选择器、claude 中文 prompt（信任框未替用户确认）。
- 已知缺口（app）：触顶洪水中回滚视图无法锚定、合并更新里同时增长又越界的驱逐识别不了（根治需 daemon 单调绝对行号，v1.1）；`berthd` 由 GUI 用登录 shell PATH 拉起，argv resume 依赖该 PATH。
- 下一步 M3：`feat/m3`（worktree `~/projects/berth-wt-m3`），任务书 `docs/tasks/m3-agent-aware.md` §5（官方 hook 事件核对结果、验收不写真实配置、`claude --settings` 语义）。
- `feat/m3` 合并到 main（a53c368）：M3 完成。`berth setup-hooks claude|codex [--statusline] [--yes] [--undo] [--hook-path]`（不加 --yes 只打印 diff；备份 + 字节级 --undo；自研 json_span 保持无关内容逐字节不变；只把「恰好是 berth 形式」的条目当作自己的）；zsh 集成（ZDOTDIR 垫片先 source 用户 .zshenv、集成在首个提示符后挂到用户 hook 之后、导出 `BERTH_SHELL_INTEGRATION` 供嵌套 shell 手动 source）；daemon 映射补充（PostCompact/Elicitation/ElicitationResult/CwdChanged/SubagentStart）与 ListEvents/ResumeCommand；协议 v2 + 旧 berthd 横幅/`berth debug restart-daemon`（旧 daemon 先写快照，会话以 Restored 回来）；快照格式 2（session meta 改 JSON，以后加字段不再升版；v1 文件仍可读，夹具 `crates/berth-daemon/tests/fixtures/format1-claude.bin.zst`）；`AgentInfo.last_agent` 使 `/exit` 后仍可 Resume；hover 详情、Dock 角标、`[notify] identity`（默认 com.apple.Terminal，打包后改自身 bundle id）。独立审查 1 high（安装器把复合命令误认为自己的条目并整串覆盖）2 medium（旧 daemon 无 reply_to 的 Error 只会超时；嵌套 zsh 无集成）均已修。workspace 403 测试全绿。
- 需用户验证（M3）：真实 claude 的 hook 链（信任框未替用户确认，events-claude-*.txt 只有启发式事件）；「终端想发送通知」授权弹窗未替用户批准；真实安装 hooks 由用户自己运行 `berth setup-hooks claude --yes`。
- 已知缺口（M3）：bash/fish 集成未做；嵌套 zsh 需手动 source；旧 berthd 读不了 format 2 快照（只告警不改文件）；node 托管的 claude 识别需读 argv；berth-vt 单测 stderr 打印 "Hangup: 1"（既有噪音）。
- v1 四个核心目标（Ghostty 级 GPU 终端、关闭重开历史与屏幕不丢、agent 状态感知、左侧 workspace/session 侧栏）在 main 上全部落地；剩余为用户侧验证与 v1.1 事项（daemon 单调绝对行号、bash/fish、打包 .app、Linux）。
- 最终真实环境验收（用户解锁后，2026-09-27 12:00–13:15）：GUI present 路径 PASS（window surface presented，79.9 fps，帧总耗时 p99 1.461 ms）。真实 Claude Code hook 链在 berth 会话内验证 6 轮（`claude --settings <临时文件>`，文件由 `berth setup-hooks claude` 生成、18 个事件；未写真实 `~/.claude/settings.json`）：SessionStart→idle、UserPromptSubmit→thinking、PreToolUse/PostToolUse（Bash/Read/Write/ToolSearch）、PostToolUseFailure、Stop→done、Notification(idle_prompt)→waiting_input、用户输入→idle、SessionEnd(prompt_input_exit)→agent 离开→`berth list` 回到 shell idle、osc:133D。
- 权限路径验证（3 轮）：PermissionRequest(ExitPlanMode)→waiting_permission；侧栏「等授权」+「1 需关注」+ Dock 角标 1（截图 `/tmp/berth-final2/perm.png`）；GUI 在线时通知「claude 等待授权：ExitPlanMode」以 identity com.apple.Terminal 交给 macOS（141 ms）；6 s 后 Claude 自己的 Notification(permission_prompt) 到达不重复跳变；SessionEnd 后角标清零。触发方式：用户的 Claude 配置强制 bypassPermissions（`permissions.defaultMode`；`--permission-mode default|plan` 启动数秒后都被翻回 bypass，原因在用户侧未深究），Write 永远不弹窗，所以用 shift+tab 切到 plan mode，让 ExitPlanMode 弹确认框。
- 新发现缺口（daemon 状态机）：在权限框上按 Esc 取消不触发任何 hook（官方 PermissionDenied 只对应自动拒绝），而 `Signal::UserInput` 只复位 Done/WaitingInput，所以「等授权」会一直挂到下一次 UserPromptSubmit/SessionEnd。v1.1 建议：WaitingPermission 期间收到单独的 ESC 或 ^C → Idle（`input:cancel`，保留 source），先核对 Codex 审批框语义。
- 验收方法注记：`berth --screenshot` 截图后立即退出，`--exit-after` 不能让它留在线；观察跃迁时刻的通知/角标必须用不带 `--screenshot` 的常驻 GUI。`berthd` 不带 `--foreground` 时不会把控制权交回调用脚本（脚本里用 `nohup berthd &` 或由 GUI 拉起）。从 Claude Code 会话里启动的 berthd 会把 `CLAUDECODE`/`CLAUDE_CODE_*` 传给会话（v1.1：spawn 时剥掉）。
- 验收用的两个隔离 daemon（数据目录 `/tmp/berth-final`、`/tmp/berth-final2`）已按 pid + 数据目录核对后 SIGTERM 停止；`~/Library/Application Support/berth` 未动。仍由用户决定：删除已合并 worktree `~/projects/berth-wt-{vt,daemon,app,integrate,m3}`；清理 `~/.bash_history` 的测试行；对真实配置运行 `berth setup-hooks claude --yes`。
- `fix/tabs-hook-guard` 合并到 main（c9e1cf0）：⌘T = ⌘N（新建 session，Ghostty/Terminal.app 肌肉记忆；⌘⇧T 仍未绑定）；berth-hook 在环境里没有合法 `BERTH_SESSION_ID` 时不转发（守卫放在唯一出口 `deliver()`，在 `Paths::resolve()` 之前；codex `--chain` 与 statusline 包住的原命令照常执行、退出码透传）。独立审查无功能/安全缺陷，1 处文档措辞（§10 写的「进程树回退」并不存在，实际是 external_id + cwd 两层）已在合并时改正。合并后 410 测试全绿。
- 该 hook 缺陷的实证：hooks 装进真实 `~/.claude/settings.json` 后，berth 之外的 Claude Code（本次是我自己的会话）也会跑 berth-hook，daemon 的 cwd 兜底把事件错记到同目录的 berth 会话上（`berth list` 里一个只跑 zsh 的会话被标成 claude tool_running:Bash）。装上带守卫的 berth-hook 后（2026-09-27 17:03），该会话不再收到外部事件（17:03:38 之后为空）。
- 部署注记：直接 `cp` 覆盖正在运行的二进制会让运行中的进程被内核杀掉（macOS 代码签名页失效）。替换 `~/.local/bin/berth` 时 GUI 进程即刻消失；应先写临时文件再 `mv`（原子替换，旧 inode 保留给运行中的进程）。daemon 未替换（协议仍 v2），6 个 session 全部存活。
- 新发现缺口（v1.1）：hook 来源的忙状态没有超时——`StateSource::Hook` 会挡住静默启发式，所以一次错误/丢失的 hook 会让侧栏一直停在 `tool_running`，直到该会话自己再来一个 hook。建议：hook 态超过 N 分钟且 PTY 静默时降级为 Idle（`heuristic:stale`）。
