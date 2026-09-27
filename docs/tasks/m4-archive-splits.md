# M4 任务书：归档、右键菜单、分屏

设计以 `docs/DESIGN.md` §17 为准；本文件只讲分工、顺序、边界与验收。

## 1. 顺序与所有权

| 步骤 | 分支 / worktree | 所有权 | 依赖 |
|---|---|---|---|
| 1 `feat/m4-core` | `~/projects/berth-wt-m4-core` | 只改 `crates/berth-core`：`SessionMeta.archived_at_ms`、`Request::Archive/Unarchive`、`PROTOCOL_VERSION = 3`、版本守卫测试 | 无；Fable 审后先合 main |
| 2a `feat/m4-daemon` | `~/projects/berth-wt-m4-daemon` | `crates/berth-daemon`、`crates/berth-store`；`crates/berth-app/src/cli.rs` 里**仅** `list` 与 `debug` 子命令 | 步骤 1 合入 main |
| 2b `feat/m4-app` | `~/projects/berth-wt-m4-app` | `crates/berth-app`（除 `cli.rs` 的 `list`/`debug` 子命令外）；顶层截图/调试参数可改 | 分屏与右键菜单不依赖协议；归档 UI 在步骤 1 合入 main 后 `git merge main` |

- `berth-core` 之外的 crate 不得修改 core；core 只允许加法（枚举变体追加到末尾、结构体字段追加到末尾并 `#[serde(default)]`）。
- 两条线并行，禁止同文件并行编辑；`cli.rs` 的冲突由 Fable 在合并时解决。
- 不得写 `~/.claude/settings.json`、`~/.codex/config.toml`、`~/Library/Application Support/berth`；测试一律用隔离的 `BERTH_DATA_DIR`/`BERTH_SOCKET`；不启动用户的 GUI/daemon。

## 2. 步骤 1：core 契约（精确定义）

```rust
// crates/berth-core/src/session.rs — SessionMeta 末尾追加
/// Archived sessions leave the workspace list but keep all data (DESIGN §17.1).
/// Never `Live` while set.
#[serde(default)]
pub archived_at_ms: Option<i64>,
// impl SessionMeta { pub fn is_archived(&self) -> bool }

// crates/berth-core/src/protocol.rs — Request 末尾追加
/// Kill (if live) then mark archived; reply SessionUpdated (DESIGN §17.1).
Archive { session: SessionId },
/// Clear the archived mark; reply SessionUpdated.
Unarchive { session: SessionId },
```

- `PROTOCOL_VERSION = 3`；`PROTOCOL_2_LAST_REQUEST` 类似的守卫常量与测试按现有模式补齐（老变体 tag 不变）。
- 快照 `SessionSnapshotFile` v2 的 meta 是 JSON，不升格式号；`v1` 转换补 `archived_at_ms: None`。
- 所有现有构造点（daemon、app、测试）补字段；`cargo test --workspace` 全绿。

## 3. 步骤 2a：daemon

- `config.rs` 加 `[archive]`（`auto_after_days: u32 = 7`、`purge_after_days: u32 = 0`），文档注释指向 §17.4。
- `manager.rs`：`archive(sid)`（live → 现有 kill 路径等收尾 → 打标记 → 广播 `SessionUpdated`），`unarchive(sid)`；被拒绝的请求（Attach/Input/Resize/Revive/Subscribe 对归档 session）返回 `Error`「已归档，先恢复」；hook 路由 cwd 兜底跳过归档 session；通知/角标相关计数不含归档（daemon 侧若有汇总）。
- 扫描器：启动 60 s 后一次，之后每 10 min；条件见 §17.1；时钟可注入以便测试；每次动作 `info!`。
- `store`：`meta_json` 已含新字段；如需按归档过滤加索引可加，但不改表结构版本（若必须改，走现有 migration 模式）。
- CLI（`cli.rs` 的 `list`/`debug`）：`berth list` 默认过滤归档 + 末尾计数；`berth list --archived`；`berth debug archive|unarchive <sid>`。
- 测试：见 §17.5 daemon 条目；另加「归档后 hook 按 cwd 兜底不命中」与「Revive 归档 session 被拒」。

## 4. 步骤 2b：app

- `PaneTree`（新模块 `panes.rs`）：数据结构、split/close/remove/focus-nav/序列化/剔除；纯函数 + 单元测试。
- controller：从单 `view`/`attached_to` 改为按 pane 的多 attach（每 pane 独立 `SessionView`、dims、Resize）；⌘W 语义（§17.3）；侧栏单击语义；`gui-state.json` 原子写与恢复。
- 渲染：每 pane 一次 prepare + render，共享 atlas；分隔条绘制与拖拽；焦点边框；鼠标/选区/滚动/IME 按 pane 路由。
- 右键菜单（§17.2）：侧栏行、归档区、workspace 头、终端区域；⇧+右键透传规则。
- 归档区 UI：折叠标题、行、恢复/彻底删除；归档 session 不计入「需关注」与角标。
- 快捷键：⌘D、⌘⇧D、⌥⌘方向键；`input.rs` 表与测试同步。
- 隐藏调试参数：`--split-right <sid>`、`--split-down <sid>`（与 `--screenshot`/`--session` 组合）；`--stats` 报告每 pane 与总帧耗时。
- 测试与证据：见 §17.5 GUI 条目；截图放 `/tmp/berth-m4/`，报告里给路径与 `--stats` 原样行。

## 5. 审查与合并

- 每条线完成后由 sonnet 审查者对抗复核（正确性、并发、协议兼容、状态机、渲染回归），实现者不得自评通过。
- 合并顺序：core → daemon → app；每次合并后 `cargo fmt --check`、`clippy -D warnings`、`cargo test --workspace`。
- 部署：重编 release → 复制到 `~/.local/bin` → `berth debug restart-daemon`（协议 v3）→ 重启 GUI；在 `docs/progress.md` 记录证据。
