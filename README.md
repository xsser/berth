# berth

面向 AI coding agent 的 GPU 原生终端（Rust，macOS 优先）。

- 关闭窗口不杀 session：`berthd` 常驻 daemon 持有 PTY 与终端状态，GUI 只是客户端。
- 内容持久化：定期快照 + 重启后作为只读历史前缀恢复，一键 revive / `claude --resume`。
- Agent 感知：识别 pane 里的 claude / codex / shell，状态机 + 未聚焦通知。
- 左侧栏：workspace → sessions 树、末尾几行实时预览、状态徽标。

设计文档：[docs/DESIGN.md](docs/DESIGN.md)。进度：[docs/progress.md](docs/progress.md)。

## 构建

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace
cargo test --workspace
```

## 许可证

MIT OR Apache-2.0
