# 任务 daemon：实现 `crates/berth-store`、`crates/berth-daemon`、`crates/berth-hook`

分支 `feat/daemon`。所有权：这三个 crate 目录。先读 `docs/tasks/COMMON.md`、`docs/DESIGN.md` §3、§4、§6、§7、§9、§10、§11，以及 `crates/berth-core/src/*.rs`。

`crates/berth-vt` 由另一 agent 在 `feat/vt` 并行实现；你只依赖它在 `crates/berth-vt/src/*.rs` 里给定的公开签名。需要跑端到端测试时执行 `git merge feat/vt`（只读使用，不改其文件；若尚未合入就先用 `cfg(test)` 的假 `Terminal`/`PtyHandle` 适配层或把端到端测试标 `#[ignore]` 并在报告说明）。

## A. `berth-store`
- SQLite（WAL，文件 0600）：`schema_version`、`workspaces`、`sessions`（列：id、workspace、status、order、last_active_ms、meta_json 全量 `SessionMeta` JSON；需要加 `serde_json` 依赖）、`events`（session、at_ms、kind、state、detail）。
- 快照：`postcard` → `zstd`(level 3) → `<sid>.bin.zst.tmp` → fsync → rename；读时校验 `format_version`。
- `JournalWriter`：`journals/<sid>/NNNN.log`，每条记录 `u64 ts_ms | u32 len | bytes`，64 MiB 轮转。
- 单元测试：CRUD 往返、快照往返（含 20k 行）、purge 删净、事件按时间倒序。

## B. `berth-daemon`
拆成 `lib.rs`（`pub async fn run(paths, config, shutdown: watch::Receiver<bool>) -> anyhow::Result<()>`，供集成测试进程内启动）+ `main.rs`（解析参数、日志、信号 → shutdown）。模块：
- `lock.rs`：`flock` 独占 `berthd.lock`；已被占则打印 pid 退出 0。取得锁后删除残留 socket 文件。
- `config.rs`：读 `Paths.config_file`（TOML，缺省值见 DESIGN §8.4；daemon 只用 `[terminal]`、`[persist]`、`[agents.*]`）。
- `server.rs`：`tokio::net::UnixListener`；每连接一个 task：`FrameReader` 解 `ClientMsg`，首帧必须 `Hello`（协议不符回 `Incompatible` 后断开）；把请求转给 `Manager`；订阅事件通过每连接 `mpsc`（有界，满则丢弃旧的 `Screen`/`Preview` 只保留最新）写回。
- `manager.rs`：workspace/session 注册表、持久化编排、启动恢复（DB 中 `Live` → `Restored`，快照按需懒加载为「restored prefix」）、`Revive`（`Shell` 在原 cwd 起 shell；`ResumeAgent` 用 `agents.<kind>.resume_command` 模板替换 `{id}`）。
- `session.rs`：每 session 一个 std 线程，拥有 `PtyHandle` + `Terminal`：循环 select PTY 输出 / 输入命令 / 4ms 合批 tick / 快照 tick（≥5s 且脏）。产出 `ScreenUpdate`（首个订阅者或 Attach 发 full + 全量 styles，之后按 damage 发行增量 + `take_pending` styles）与 `Preview`（末尾 N 行，≤4Hz）。`PtyWrite` 事件立刻写回 PTY。虚拟行号空间 = restored prefix ++ live history。
- `agent_state.rs`：DESIGN §9 状态机；输入枚举 `Signal::{Hook(ClaudeHookEvent…), Codex, Osc(OscEvent), ForegroundProcess(name), OutputActivity, Silence(secs), Exited}`；规则：hook 30s 内优先；启发式不得覆盖 `WaitingPermission`；每次转移写 `EventRecord`。**单元测试覆盖表中每一行**。
- `hooks.rs`：`HookEnvelope` → 目标 session：`berth_session` 优先；否则 `cwd` 唯一匹配；否则丢弃并 `debug!`。Statusline 更新 model/context/cost。
- 环境注入：`BERTH_SESSION_ID`、`BERTH_SOCKET`。
- `DaemonStatus`、`Shutdown`（优雅：所有 session 写快照后退出）。

## C. `berth-hook`
- 无 tokio/clap；`std` 读 stdin ≤1 MiB；`serde_json` 解析；映射规则：
  - `claude`：字段以官方文档为准（用 WebFetch 核对 https://code.claude.com/docs/en/hooks 与 statusline 文档；不要凭记忆）：`session_id`、`transcript_path`、`cwd`、`hook_event_name`、`permission_mode`、`tool_name`、`notification_type`、`message`、`stop_hook_active`、`source`、`trigger`、`reason`。未知事件 → `Other`。
  - `codex`：JSON 在最后一个 argv；`type`、`thread-id`/`thread_id`、`cwd`、`last-assistant-message`。`--chain <cmd...>` 时发送后 `exec` 原命令并原样传 argv（保持 Computer Use 客户端可用）。
  - `statusline -- <cmd...>`：stdin JSON 复制发送（`session_id`、`model.id`、`context_window.used_percentage`、`cost.total_cost_usd`、`workspace.project_dir`、`cwd`），再以同一 JSON 为 stdin 运行原命令并透传 stdout/退出码。
- 连接：`UnixStream::connect(BERTH_SOCKET 或 Paths::resolve().socket)`，读写超时 50ms；发 `Hello{role:Hook}` + `Hook(envelope)`；任何失败静默退出 0。
- 单元测试：三种 JSON 样例映射；无 socket 时 50ms 内退出 0。

## D. 端到端集成测试（`crates/berth-daemon/tests/e2e.rs`，临时目录）
1. 进程内 `run` → `Hello` → `CreateWorkspace` → `CreateSession{command:["/bin/sh"]}` → `Attach` → `Input("echo hi-$$\n")` → 收到 `Screen` 含 `hi-` → `Detach` → 再 `Attach` 得到 full screen 仍含该行。
2. `Hook(Claude PreToolUse{Bash})` 带 `berth_session` → 收到 `SessionUpdated`/`AgentChanged`，state=`ToolRunning{Bash}`；随后 `Notification{permission_prompt}` → `WaitingPermission`；`Stop` → `Done`。
3. `Kill` → `Exited` → 快照文件存在；`Shutdown`；同目录再次 `run` → `ListSessions` 该 session 为 `Restored`，`FetchLines` 返回含 `hi-` 的历史；`Revive{Shell}` 后新输出在历史之后。
4. 预览订阅：`Subscribe{Preview{rows:3,max_hz:4}}` 后 1s 内收到 ≤5 条 `Preview`。

## 验收
`cargo test -p berth-store -p berth-daemon -p berth-hook` 全绿；clippy 无告警；`berthd --foreground` 手动启动后 `ls -la` 数据目录权限为 0700/0600。报告附测试名与结果、`berth-hook` 冷启动耗时（`time` 10 次）。
