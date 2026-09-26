# 进度

## 2026-09-26
- 设计文档 `docs/DESIGN.md` v0.1；用户确认四项决策（winit+wgpu+egui、daemon+快照、v1 只做核心四目标、名字 berth）。
- 安装 Rust 1.98.1（rustup，用户态，未改 shell profile；使用 `export PATH="$HOME/.cargo/bin:$PATH"`）。
- workspace 骨架：`berth-core`（冻结接口 + 单元测试）；`berth-vt` / `berth-store` 接口桩；`berthd` / `berth-hook` / `berth` 入口桩。
- 已核实：vte 0.15 未知 OSC 无回调 → 预扫描器路线成立；egui-wgpu 0.36 与 cosmic-text 0.19 均锁 wgpu 30。
