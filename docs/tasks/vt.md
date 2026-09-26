# 任务 vt：实现 `crates/berth-vt`

分支 `feat/vt`。所有权：`crates/berth-vt/**`。先读 `docs/tasks/COMMON.md`、`docs/DESIGN.md` §5、§7，以及 `crates/berth-core/src/{snapshot,style}.rs`。

公开 API 已在 `src/{lib,osc,convert,pty,terminal}.rs` 里以签名 + 文档给定；实现它们，签名可以加 `&mut`/返回类型细化但不要删方法。`Terminal`/`PtyHandle` 的私有字段自行设计。

## 1. `osc.rs` — OscPrescanner
- 识别 `ESC ]` … `BEL` / `ESC \`；跨 chunk 保持状态；载荷 > 4096 字节丢弃并复位。
- OSC 7：`file://host/path`，percent-decoding，忽略非本机 host 也照收 path。
- OSC 133：`A`/`B`/`C`/`D[;exit]`，其它子参数忽略。
- OSC 9：`9;<body>` → Notify{title:None}；OSC 777：`777;notify;<title>;<body>`。
- 单元测试：BEL 与 ST 终止；序列被切成 1 字节一个 chunk；非 OSC 的 ESC 序列不触发；超长丢弃；`\x1b]133;D;0\x07` → CommandEnd{Some(0)}；`\x1b]7;file://Mac/Users/x/a%20b\x07` → Cwd("/Users/x/a b")。

## 2. `convert.rs`
- 依 alacritty `Flags`（`term/cell.rs`）与 `vte::ansi::Color`（Named/Spec/Indexed）映射到 `berth_core::{Style, CellFlags, Color}`，规则见文件头注释。`NamedColor::Foreground/Background` → `Color::Default`；Bright* → 8..15；Dim* → 基色。
- `cell.zerowidth()` 追加到 run 文本；`WIDE_CHAR_SPACER`/`LEADING_WIDE_CHAR_SPACER` 跳过；行尾默认样式空白裁掉；`WRAPLINE` → `wrapped`。
- 单元测试用真实 `Term` 喂序列后取 `term.grid()[Line(0)]` 转换（不要手工构造 Cell）。

## 3. `pty.rs`
- `portable-pty`：`native_pty_system().openpty(PtySize)`，`CommandBuilder`；`command` 为空时用 `$SHELL`（缺省 `/bin/zsh`）加 `-l`。设置 `TERM=xterm-256color`、`COLORTERM=truecolor`，再叠加 `spec.env`。
- 读线程：64 KiB 缓冲循环 `read` → `PtyOutput::Data`，EOF/错误 → `PtyOutput::Eof` 后退出。写端 `take_writer()` 用 `Mutex` 包住。
- `foreground_process`：`master.as_raw_fd()` → `libc::tcgetpgrp`；macOS 用 `libc::proc_name` / `proc_pidpath` 取名字、`proc_pidinfo(PROC_PIDVNODEPATHINFO)` 取 cwd；Linux 读 `/proc/<pid>/{comm,cwd}`。失败返回 None，不 panic。
- `kill`：`killpg(child_pid, SIGHUP)`，1s 后仍在则 SIGKILL（由调用方决定是否等待，这里提供 `kill` 与 `try_wait` 即可）。
- 集成测试（真实 PTY）：`spawn(["/bin/sh","-c","echo hi; exit 3"])` → 收到含 `hi` 的 Data → Eof → `try_wait()==Some(3)`；`spawn(["/bin/sleep","5"])` → `foreground_process().name` 以 `sleep` 结尾 → `kill()`。

## 4. `terminal.rs`
- `Term<Listener>`：`Listener` 收集 `alacritty_terminal::event::Event` 到内部 Vec，`process` 结束时映射为 `TermEvent`（`Title`/`ResetTitle`→`Title(None)`/`Bell`/`PtyWrite`/`ClipboardStore`/`CursorBlinkingChange`/`ChildExit`；`ColorRequest` 用 alacritty 默认调色板回答并作为 `PtyWrite`；`TextAreaSizeRequest` 以 0 像素回答；`ClipboardLoad` 忽略；`Wakeup`/`MouseCursorDirty` 忽略）。
- `process`：先 `OscPrescanner::scan`，再 `Processor::advance(&mut term, bytes)`；随后把 `term.damage()` 合并进累计 damage（`TermDamage::Full` 覆盖一切；`Partial` 收集 `line`），`reset_damage()`。`take_damage` 返回并清空累计值。
- `screen`/`lines`：可视行 `grid[Line(i)]`；`history_len = grid.history_size()`；`history(start,count)`：第 `start` 旧的行对应 `Line(start as i32 - history_len as i32)`。daemon 不滚动 `Term`（display offset 恒 0）。
- `cursor`：`grid.cursor.point` + `term.cursor_style()`（shape/blinking）+ `TermMode::SHOW_CURSOR`；`modes`：`TermMode` 位映射到 `TermModes`（含 kitty 键盘五个位）。
- `title`：跟踪最近的 Title 事件。`resize`：`term.resize(TermSize::new(cols, rows))`。
- 单元测试：`"hello\r\n"` → 第 0 行文本 `hello`；SGR `\x1b[1;31mX\x1b[0m` → 样式 fg=Indexed(1)+BOLD；`你好` → 一个 run cells=4；24 行终端喂 30 行 → `history_len()==6`、`history(0,1)` 是第一行；`\x1b[?1049h` → `ALT_SCREEN`；`\x1b[6n` → `PtyWrite` 含 `\x1b[`；damage：改第 3 行后 `take_damage()==Lines([2])`（首帧为 Full）；resize 后 `dims()` 更新。

## 验收
`cargo test -p berth-vt` 全绿（含真实 PTY 集成测试），clippy 无告警。报告附各测试名与结果。
