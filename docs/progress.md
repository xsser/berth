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
