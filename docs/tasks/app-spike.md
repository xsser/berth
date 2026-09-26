# 任务 app-spike：`crates/berth-app` M0 渲染/输入 spike（可演进为正式 GUI）

分支 `feat/app`。所有权：`crates/berth-app/**`。先读 `docs/tasks/COMMON.md`、`docs/DESIGN.md` §8、§13（M0）、§14，以及 `crates/berth-core/src/{snapshot,style,protocol}.rs`。

目标：用**证据**回答 D3 是否成立——winit + wgpu 自研网格渲染 + egui 侧栏能否达到 Ghostty 级文字质量与 60fps，且中文输入法可用。本阶段不连 daemon：数据来自 fixture，但代码结构按正式版组织。

## 模块
- `app.rs`：winit 0.30 `ApplicationHandler`；窗口标题 `berth`；HiDPI 缩放；resize 重算 cols/rows；`--screenshot <png>` 参数：渲染 3 帧后把 surface 读回存 PNG（加 `image` 或 `png` 依赖）并退出。
- `renderer/`：
  - `metrics.rs`：从字体取 cell 宽高（advance、ascent、descent、line gap），配置 `font.family`/`font.size`，缺省 `SF Mono` 13。
  - `text.rs`：`cosmic_text::FontSystem`（系统字体 + 回退）；按行 shaping（`ShapeLine` 或 `Buffer`）得到 (font, glyph_id, cluster)；**忽略 advance，按 cell 定位**，簇跨多格放首格；shaping 结果按 (文本, 样式 run) 哈希缓存。
  - `atlas.rs`：`swash` 光栅化；`etagere` 打包；R8 掩码图集 + RGBA8 彩色 emoji 图集；满了重建。
  - `grid.rs` + `shaders.wgsl`：三层实例化 quad：背景色、字形、装饰（下划线/双线/波浪/点/虚线/删除线、光标 Block/Beam/Underline/Hollow、选区高亮）。`INVERSE` 交换前后景；`DIM` 降亮度；`Default` 色取主题。
  - 主题：内置一套（Ghostty 默认色即可），`[u8;3]` 16 色 + fg/bg/cursor。
- `sidebar.rs`：`egui` + `egui-wgpu` + `egui-winit` 共用同一 device/surface；左侧 280px 面板：fixture 的 2 个 workspace、5 个 session，按 `AgentState` 显示徽标（○ ◐ ⚙ ⏳ ✎ ✓ ✗ ⏹）、标题、cwd、经过时间、3 行等宽预览（v0 用 egui 文本即可）；必须加载一款 CJK 字体（从系统找 PingFang/Hiragino，用 `cosmic_text` 的 fontdb 定位文件后 `include` 到 egui `FontDefinitions`），中文标题不能是豆腐块。
- `input.rs`：winit `KeyEvent` → xterm 字节序列（方向键 + APP_CURSOR 变体、Home/End/PgUp/PgDn/Del/Ins、F1–F12、Enter=`\r`、Backspace=`\x7f`、Tab、Ctrl+字母、Alt 前缀 ESC、Shift/Ctrl 修饰的方向键 `\x1b[1;5C` 形式）；**单元测试**覆盖每类。
- `ime.rs`：`window.set_ime_allowed(true)`；处理 `WindowEvent::Ime::{Enabled,Preedit,Commit,Disabled}`；预编辑文本画在光标处并加下划线；`set_ime_cursor_area` 跟随光标。状态机单元测试（Preedit 变化、Commit 清空）。
- `fixture.rs`：构造 `ScreenSnapshot` 120×40：ASCII 表格、`中文汉字测试`、`🚀 👨‍👩‍👧 🇨🇳`、box drawing `┌─┐│└┘`、Powerline `` 若字体有、粗/斜/暗/下划线/波浪线/删除线、16 色 + 256 色 + truecolor 渐变一行、宽字符与 ASCII 混排对齐检查行、光标位置。键盘输入在最后一行本地回显（证明输入路径），IME commit 也回显。
- `main.rs`：保留现有 CLI 子命令骨架，无子命令时启动 GUI。

## 验收（全部要实际运行）
1. `cargo run -p berth-app -- --screenshot /tmp/berth-m0.png` 生成截图；用 Read 工具打开 PNG 自检：中文/emoji/box drawing 对齐、无豆腐块、装饰线正确、侧栏中文正常。把截图路径写进报告。
2. 帧率：内置光标闪烁动画连续渲染 5s，stderr 打印平均/最差帧时间；报告数字（目标 ≤ 8ms/帧 @ 120×40）。
3. `cargo test -p berth-app` 全绿（input/ime/atlas 单元测试）；clippy 无告警。
4. IME 无法自动化：写清楚实现依据（winit 文档链接）并保留 `BERTH_IME_DEBUG=1` 打印 preedit/commit 事件，留给人工验证。
5. 报告明确回答：D3 成立/不成立，理由与数据；若不成立，说明卡点与建议（例如切 GPUI）。
