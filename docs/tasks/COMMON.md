# 实施通用规则（所有实现 agent 必读）

## 环境
- 仓库：`~/projects/berth`；你在自己的 git worktree 与分支上工作（任务书里给出绝对路径），**不要**切到主仓库目录改文件。
- Rust 在 `~/.cargo/bin`，每条 Bash 命令前加 `export PATH="$HOME/.cargo/bin:$PATH"`（shell 状态不跨命令保留）。
- 参考源码：`/tmp/berth-src/alacritty_terminal-0.26.0`、`/tmp/berth-src/vte-0.15.0`、`/tmp/berth-src/portable-pty-0.9.0`；依赖拉取后也在 `~/.cargo/registry/src/*/`。
- 设计文档 `docs/DESIGN.md` 是权威；接口冻结在 `crates/berth-core`。

## 文件所有权
- 只编辑任务书列出的 crate 目录（加上 `Cargo.lock`）。
- `crates/berth-core` 只允许**加法**改动（新增字段/变体/方法），且必须：保持现有测试通过、在最终报告里逐条列出。改动前先想有没有不改 core 的办法。
- 不改 `docs/DESIGN.md`、其他 crate、`~/.claude`、`~/.codex`、shell profile；不装系统级软件；不 sudo。

## 质量门槛（必须实际运行并在报告里贴输出摘要）
- `cargo test -p <你的crate>` 全绿；集成测试用 `tempfile` 隔离数据目录（`Paths::in_dir`），不碰 `~/Library/Application Support/berth`。
- `cargo clippy -p <你的crate> --all-targets -- -D warnings` 无告警；`cargo fmt`。
- 不用 `todo!()`/`unimplemented!()` 留在交付路径上；确实做不到的写进报告「已知缺口」。
- 不吞错误：`Result` 往上传或 `tracing::warn!` 记录；hook 路径例外（永远静默退出 0）。

## 提交与报告
- 小步提交到你的分支，信息格式 `<crate>: <做了什么>`。
- 最终报告（≤ 600 字）包含：完成项、测试/clippy 输出摘要（真实粘贴）、对 `berth-core` 的加法改动清单、与任务书的偏差及原因、已知缺口、给审核者的验证命令。
- 报告只回传结论与证据，不贴大段源码。
