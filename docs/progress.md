# 进度

## 2026-09-26
- 设计文档 `docs/DESIGN.md` v0.1；用户确认四项决策（winit+wgpu+egui、daemon+快照、v1 只做核心四目标、名字 berth）。
- 安装 Rust 1.98.1（rustup，用户态，未改 shell profile；使用 `export PATH="$HOME/.cargo/bin:$PATH"`）。
- workspace 骨架：`berth-core`（冻结接口 + 单元测试）；`berth-vt` / `berth-store` 接口桩；`berthd` / `berth-hook` / `berth` 入口桩。
- 已核实：vte 0.15 未知 OSC 无回调 → 预扫描器路线成立；egui-wgpu 0.36 与 cosmic-text 0.19 均锁 wgpu 30。

## 2026-09-27
- `feat/vt` 合并到 main（HEAD fec77f0 之前）：59 单元 + 11 真实 PTY 测试；独立审查 0 high、2 medium、1 low 均已修复（前台进程组组长退出后的成员枚举、Drop/reap 出错路径升级信号、无 pid 分支 wait）。
- 已知缺口（vt）：alt screen 期间读不到主屏 scrollback；OSC 10/11 用默认调色板回答；Linux 进程信息未实测。
- `feat/daemon`（bdd122d）与 `feat/app`（a736fc5）门禁均已复核通过，等待对抗审查后合并。
- M0 结论：D3 成立。release 下 120×40 每帧 2.6 ms（含 GPU），缺字 0；present 路径与真实 IME 需用户解锁后人工验证。
- 副作用：早期 `sh -i` PTY 测试向 `~/.bash_history` 追加了 104 行测试命令（已改用临时 HISTFILE），清理待用户确认。
