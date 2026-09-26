//! Static data for the M0 spike (no daemon yet): a 120×40 screen exercising
//! every rendering path, plus sidebar workspaces/sessions. The shapes are the
//! real protocol types so the same code later consumes daemon events.

use berth_core::{
    char_cells, AgentInfo, AgentKind, AgentState, CellFlags, Color, CursorShape, CursorState,
    LineSnapshot, ScreenSnapshot, SessionId, SessionMeta, SessionStatus, StateSource, Style,
    StyleId, StyleInterner, TermModes, Workspace, WorkspaceId,
};
use std::path::PathBuf;

pub const COLS: u16 = 120;
pub const ROWS: u16 = 40;
/// Row that shows the last key's encoding.
pub const STATUS_ROW: u16 = ROWS - 2;
/// Row with the prompt and the local echo.
pub const PROMPT_ROW: u16 = ROWS - 1;
pub const PROMPT: &str = "berth ❯ ";

/// Inclusive cell range highlighted as a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

impl Selection {
    pub fn contains(&self, row: u16, col: u16) -> bool {
        (row, col) >= self.start && (row, col) <= self.end
    }
}

/// Line under construction; never exceeds `cols` cells.
pub struct LineWriter {
    line: LineSnapshot,
    cells: u16,
    cols: u16,
}

impl LineWriter {
    pub fn new(cols: u16) -> Self {
        Self {
            line: LineSnapshot::blank(),
            cells: 0,
            cols,
        }
    }

    pub fn put(&mut self, text: &str, style: StyleId) -> &mut Self {
        for ch in text.chars() {
            let w = char_cells(ch);
            if self.cells + w > self.cols {
                break;
            }
            self.line.push(ch, style);
            self.cells += w;
        }
        self
    }

    /// Pad with spaces up to `col`.
    pub fn pad_to(&mut self, col: u16, style: StyleId) -> &mut Self {
        while self.cells < col.min(self.cols) {
            self.line.push(' ', style);
            self.cells += 1;
        }
        self
    }

    pub fn cells(&self) -> u16 {
        self.cells
    }

    pub fn finish(mut self) -> LineSnapshot {
        self.line.trim_trailing_default();
        self.line
    }
}

pub struct Fixture {
    pub screen: ScreenSnapshot,
    pub interner: StyleInterner,
    pub selection: Option<Selection>,
}

fn fg(i: u8) -> Style {
    Style {
        fg: Color::Indexed(i),
        ..Default::default()
    }
}

fn flags(f: CellFlags) -> Style {
    Style {
        flags: f,
        ..Default::default()
    }
}

/// HSV (h in 0..1) to RGB.
fn hsv(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    let (r, g, b) = match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    (
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

impl Fixture {
    pub fn build() -> Self {
        let mut st = StyleInterner::new();
        let d = StyleId::DEFAULT;
        let mut lines: Vec<LineSnapshot> = Vec::with_capacity(ROWS as usize);
        let new = || LineWriter::new(COLS);

        // 0: title bar (inverse + bold).
        let title = st.intern(flags(CellFlags::INVERSE | CellFlags::BOLD));
        let mut l = new();
        l.put(" berth · M0 渲染 spike · fixture 120×40 · 字体回退 / emoji / box drawing / 样式 / 颜色 / IME ", title)
            .pad_to(COLS, title);
        lines.push(l.finish());

        // 1: column ruler.
        let dim = st.intern(flags(CellFlags::DIM));
        let mut l = new();
        for i in 0..COLS {
            l.put(
                &char::from(b'0' + (i % 10) as u8).to_string(),
                if i % 10 == 0 { d } else { dim },
            );
        }
        lines.push(l.finish());

        // 2..=7: ASCII table | box drawing table | rounded box + more drawing chars.
        let green = st.intern(fg(2));
        let red_bold = st.intern(Style {
            fg: Color::Indexed(1),
            flags: CellFlags::BOLD,
            ..Default::default()
        });
        let orange = st.intern(Style {
            fg: Color::Rgb(0xd7, 0x77, 0x57),
            ..Default::default()
        });
        let heavy = st.intern(fg(4));
        let rows: [(&str, &str, &str); 6] = [
            (
                "+----------+--------+--------+",
                "┌──────────┬────────┬────────┐",
                "╭────────────────────────────╮  ═ ║ ╔═╦═╗ ╒═╤═╕ ╓─╥─╖",
            ),
            (
                "| crate    | tests  | status |",
                "│ crate    │ tests  │ status │",
                "│ ✻ Welcome to Claude Code!  │  ╟─╫─╢ ╠═╬═╣ ╞═╪═╡ ├─┼─┤",
            ),
            (
                "+----------+--------+--------+",
                "├──────────┼────────┼────────┤",
                "│   /help for help           │  ╚═╩═╝ ╙─╨─╜ ╘═╧═╛ ┄┄ ┆ ╌╎",
            ),
            (
                "| berth-vt |     42 | ok     |",
                "│ berth-vt │     42 │ ok     │",
                "╰────────────────────────────╯  ━━━ ┃ ┏━┳━┓ ┣━╋━┫ ┗━┻━┛ ╱╲╳",
            ),
            (
                "| 中文模块 |      7 | 失败   |",
                "│ 中文模块 │      7 │ 失败   │",
                "░░▒▒▓▓██ ▀▀▄▄ ▌▐ ▁▂▃▄▅▆▇█ ▏▎▍▌▋▊▉ ▖▗▘▙▚▛▜▝▞▟",
            ),
            (
                "+----------+--------+--------+",
                "└──────────┴────────┴────────┘",
                "┍━┑┎─┒ ╭╮╭╮ ╼━╾ ╽╿ ╴╵╶╷ ╸╹╺╻ ┝┥┠┨┯┷┰┸",
            ),
        ];
        for (i, (ascii, boxed, extra)) in rows.iter().enumerate() {
            let mut l = new();
            l.put(ascii, d).put("  ", d);
            // Colour the status column of the data rows.
            if i == 3 || i == 4 {
                let byte_at =
                    |n: usize| boxed.char_indices().nth(n).map_or(boxed.len(), |(b, _)| b);
                let (a, b) = (byte_at(21), byte_at(28));
                l.put(&boxed[..a], d)
                    .put(&boxed[a..b], if i == 3 { green } else { red_bold })
                    .put(&boxed[b..], d);
            } else {
                l.put(boxed, d);
            }
            l.put("  ", d);
            let style =
                if extra.starts_with('╭') || extra.starts_with('│') || extra.starts_with('╰')
                {
                    orange
                } else {
                    heavy
                };
            l.put(extra, style);
            lines.push(l.finish());
        }

        // 8: blank.
        lines.push(LineSnapshot::blank());

        // 9: CJK.
        let yellow = st.intern(fg(3));
        let mut l = new();
        l.put("中文汉字测试：终端网格对齐，全角字符占两个单元格。", d)
            .put(" English 与 中文 mixed ", yellow)
            .put("日本語 かな カナ 한국어 ㄅㄆㄇ", d);
        lines.push(l.finish());

        // 10: emoji.
        let mut l = new();
        l.put(
            "Emoji: 🚀 👨‍👩‍👧 🇨🇳 ✅ 🔥 🎉 🐛 🦀 | ZWJ 家庭 + 国旗 + 宽字符 | ",
            d,
        )
        .put("Combining: e\u{301} a\u{308} n\u{303} | ", d)
        .put("ASCII -> => != === <= fi fl", dim);
        lines.push(l.finish());

        // 11: Powerline prompt.
        let seg1 = st.intern(Style {
            fg: Color::Indexed(0),
            bg: Color::Indexed(4),
            ..Default::default()
        });
        let arr1 = st.intern(Style {
            fg: Color::Indexed(4),
            bg: Color::Indexed(2),
            ..Default::default()
        });
        let seg2 = st.intern(Style {
            fg: Color::Indexed(0),
            bg: Color::Indexed(2),
            ..Default::default()
        });
        let arr2 = st.intern(Style {
            fg: Color::Indexed(2),
            bg: Color::Indexed(3),
            ..Default::default()
        });
        let seg3 = st.intern(Style {
            fg: Color::Indexed(0),
            bg: Color::Indexed(3),
            flags: CellFlags::BOLD,
            ..Default::default()
        });
        let arr3 = st.intern(fg(3));
        let mut l = new();
        l.put("Powerline: ", d)
            .put(" ~/projects/berth ", seg1)
            .put("\u{e0b0}", arr1)
            .put("  main ± ", seg2)
            .put("\u{e0b0}", arr2)
            .put(" 3 ", seg3)
            .put("\u{e0b0}", arr3)
            .put("  thin: \u{e0b1} \u{e0b3} ", d)
            .put("\u{e0b2}", arr3)
            .put(" left ", seg3);
        lines.push(l.finish());

        // 12: text attributes.
        let curl_red = Color::Indexed(9);
        let samples: [(&str, Style); 12] = [
            ("bold", flags(CellFlags::BOLD)),
            ("italic", flags(CellFlags::ITALIC)),
            ("bold-italic", flags(CellFlags::BOLD | CellFlags::ITALIC)),
            ("dim", flags(CellFlags::DIM)),
            ("underline", flags(CellFlags::UNDERLINE)),
            ("double", flags(CellFlags::DOUBLE_UNDERLINE)),
            (
                "undercurl",
                Style {
                    underline: curl_red,
                    flags: CellFlags::UNDERCURL,
                    ..Default::default()
                },
            ),
            ("dotted", flags(CellFlags::DOTTED_UNDERLINE)),
            ("dashed", flags(CellFlags::DASHED_UNDERLINE)),
            ("strike", flags(CellFlags::STRIKEOUT)),
            ("inverse", flags(CellFlags::INVERSE)),
            ("hidden", flags(CellFlags::HIDDEN)),
        ];
        let mut l = new();
        l.put("SGR: ", d);
        for (i, (word, style)) in samples.iter().enumerate() {
            let id = st.intern(*style);
            if i > 0 {
                l.put(" ", d);
            }
            if *word == "hidden" {
                l.put("hidden:[", d).put("secret", id).put("]", d);
            } else {
                l.put(word, id);
            }
        }
        let cjk_u = st.intern(Style {
            fg: Color::Indexed(6),
            flags: CellFlags::UNDERLINE | CellFlags::ITALIC,
            ..Default::default()
        });
        l.put(" ", d).put("中文下划线", cjk_u);
        lines.push(l.finish());

        // 13, 14: 16 colours (backgrounds + foregrounds).
        for base in [0u8, 8] {
            let mut l = new();
            l.put(if base == 0 { "16色 " } else { "高亮 " }, d);
            for i in base..base + 8 {
                let text_fg = if i == 0 || i == 8 {
                    Color::Indexed(15)
                } else {
                    Color::Indexed(0)
                };
                let id = st.intern(Style {
                    fg: text_fg,
                    bg: Color::Indexed(i),
                    ..Default::default()
                });
                l.put(&format!(" {i:>2} "), id);
            }
            l.put(" ", d);
            for i in base..base + 8 {
                let id = st.intern(fg(i));
                l.put(&format!("fg{i:<2} "), id);
            }
            let bold_bright = st.intern(Style {
                fg: Color::Indexed(base + 1),
                flags: CellFlags::BOLD,
                ..Default::default()
            });
            l.put("粗体色", bold_bright);
            lines.push(l.finish());
        }

        // 15, 16: 256-colour palette 16..=255 as backgrounds.
        for range in [16u16..136, 136..256] {
            let mut l = new();
            for i in range {
                let id = st.intern(Style {
                    bg: Color::Indexed(i as u8),
                    ..Default::default()
                });
                l.put(" ", id);
            }
            lines.push(l.finish());
        }

        // 17: truecolor hue sweep with a fg gradient label.
        let mut l = new();
        for c in 0..COLS {
            let (r, g, b) = hsv(c as f32 / COLS as f32, 0.85, 0.95);
            let (tr, tg, tb) = hsv(c as f32 / COLS as f32 + 0.5, 0.6, 0.35);
            let id = st.intern(Style {
                fg: Color::Rgb(tr, tg, tb),
                bg: Color::Rgb(r, g, b),
                ..Default::default()
            });
            let label = b"truecolor 24-bit gradient";
            let ch = if (c as usize) >= 2 && (c as usize) < 2 + label.len() {
                label[c as usize - 2] as char
            } else {
                ' '
            };
            l.put(&ch.to_string(), id);
        }
        lines.push(l.finish());

        // 18, 19: wide / narrow alignment check (every segment is 4 cells).
        let segs_a = [
            "汉字", "ABCD", "한글", "1234", "かな", "wxyz", "🚀🔥", "ab12",
        ];
        let segs_b = [
            "ABCD", "汉字", "1234", "한글", "wxyz", "かな", "ab12", "🚀🔥",
        ];
        for segs in [segs_a, segs_b] {
            let mut l = new();
            let bar = st.intern(fg(8));
            while l.cells() + 5 < COLS {
                for s in segs {
                    if l.cells() + 5 > COLS - 1 {
                        break;
                    }
                    l.put("|", bar).put(s, d);
                }
            }
            l.put("|", bar);
            lines.push(l.finish());
        }

        // 20: blank.
        lines.push(LineSnapshot::blank());

        // 21..=35: fake `cargo test` session.
        let prompt = st.intern(Style {
            fg: Color::Indexed(2),
            flags: CellFlags::BOLD,
            ..Default::default()
        });
        let bold_green = st.intern(Style {
            fg: Color::Indexed(10),
            flags: CellFlags::BOLD,
            ..Default::default()
        });
        let cyan = st.intern(fg(6));
        let red = st.intern(fg(1));
        let err_bg = st.intern(Style {
            fg: Color::Indexed(15),
            bg: Color::Rgb(0x5a, 0x1e, 0x22),
            ..Default::default()
        });
        let ital = st.intern(flags(CellFlags::ITALIC | CellFlags::DIM));
        let script: Vec<Vec<(&str, StyleId)>> = vec![
            vec![
                ("~/projects/berth ", cyan),
                ("$ ", prompt),
                ("cargo test -p berth-core", d),
            ],
            vec![
                ("   Compiling", bold_green),
                (
                    " berth-core v0.1.0 (/Users/xsser/projects/berth/crates/berth-core)",
                    d,
                ),
            ],
            vec![
                ("    Finished", bold_green),
                (
                    " `test` profile [unoptimized + debuginfo] target(s) in 1.84s",
                    d,
                ),
            ],
            vec![
                ("     Running", bold_green),
                (
                    " unittests src/lib.rs (target/debug/deps/berth_core-3f2a9c0d1e7b4a55)",
                    d,
                ),
            ],
            vec![],
            vec![("running 12 tests", d)],
            vec![
                ("test ids::tests::roundtrip_display_parse ... ", d),
                ("ok", green),
            ],
            vec![
                (
                    "test style::tests::interner_dedups_and_tracks_pending ... ",
                    d,
                ),
                ("ok", green),
            ],
            vec![
                (
                    "test snapshot::tests::push_merges_runs_and_counts_cells ... ",
                    d,
                ),
                ("ok", green),
            ],
            vec![
                (
                    "test protocol::tests::frame_roundtrip_split_across_pushes ... ",
                    d,
                ),
                ("FAILED", red),
            ],
            vec![],
            vec![(
                "thread 'main' panicked at crates/berth-core/src/protocol.rs:217:9:",
                d,
            )],
            vec![
                (" 断言失败：帧长度不一致 left: 42, right: 41 ", err_bg),
                ("  ← 选区高亮示例", ital),
            ],
            vec![],
            vec![
                ("test result: ", d),
                ("FAILED", red),
                (
                    ". 11 passed; 1 failed; 0 ignored; 0 measured; finished in 0.02s",
                    d,
                ),
            ],
        ];
        for parts in &script {
            let mut l = new();
            for (text, style) in parts {
                l.put(text, *style);
            }
            lines.push(l.finish());
        }

        // Fill up to the status/prompt rows.
        while lines.len() < STATUS_ROW as usize {
            lines.push(LineSnapshot::blank());
        }
        lines.push(LineSnapshot::blank()); // status row, filled by Terminal
        lines.push(LineSnapshot::blank()); // prompt row, filled by Terminal
        debug_assert_eq!(lines.len(), ROWS as usize);

        // Selection over part of the "thread 'main' panicked" line.
        let sel_row = lines
            .iter()
            .position(|l| l.text().starts_with("thread 'main'"))
            .unwrap_or(0) as u16;
        let selection = Some(Selection {
            start: (sel_row, 0),
            end: (sel_row + 1, 27),
        });

        let screen = ScreenSnapshot {
            cols: COLS,
            rows: ROWS,
            lines,
            cursor: CursorState {
                row: PROMPT_ROW,
                col: 0,
                shape: CursorShape::Block,
                visible: true,
                blinking: true,
            },
            modes: TermModes::SHOW_CURSOR,
            display_offset: 0,
            history_len: 0,
            title: "berth — fixture".into(),
        };
        Fixture {
            screen,
            interner: st,
            selection,
        }
    }
}

/// Sidebar fixture: 2 workspaces, 5 sessions, 3-line previews.
pub struct SidebarFixture {
    pub workspaces: Vec<Workspace>,
    pub sessions: Vec<SessionMeta>,
    /// Preview lines per session (same order as `sessions`).
    pub previews: Vec<Vec<LineSnapshot>>,
    pub focused: SessionId,
}

fn preview(lines: &[&str]) -> Vec<LineSnapshot> {
    lines
        .iter()
        .map(|s| {
            let mut l = LineSnapshot::blank();
            l.push_str(s, StyleId::DEFAULT);
            l
        })
        .collect()
}

impl SidebarFixture {
    pub fn build(now_ms: i64) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/me"));
        let ws1 = Workspace {
            id: WorkspaceId::new(),
            name: "berth".into(),
            root: home.join("projects/berth"),
            color: Some([0x81, 0xa2, 0xbe]),
            order: 0,
            created_at_ms: now_ms - 86_400_000,
        };
        let ws2 = Workspace {
            id: WorkspaceId::new(),
            name: "网关服务".into(),
            root: home.join("work/api-gateway"),
            color: Some([0xb5, 0xbd, 0x68]),
            order: 1,
            created_at_ms: now_ms - 3 * 86_400_000,
        };
        let mk = |ws: &Workspace,
                  order: u32,
                  kind: AgentKind,
                  title: &str,
                  cwd: PathBuf,
                  state: AgentState,
                  ago_s: i64,
                  unread: bool| {
            let is_shell = kind == AgentKind::Shell;
            SessionMeta {
                id: SessionId::new(),
                workspace: ws.id,
                title_auto: title.to_string(),
                title_user: None,
                cwd,
                command: vec![if is_shell {
                    "zsh".into()
                } else {
                    format!("{kind:?}").to_lowercase()
                }],
                env: Vec::new(),
                status: SessionStatus::Live,
                agent: AgentInfo {
                    kind,
                    state,
                    since_ms: now_ms - ago_s * 1000,
                    source: if is_shell {
                        StateSource::ShellIntegration
                    } else {
                        StateSource::Hook
                    },
                    confidence: if is_shell { 0.8 } else { 1.0 },
                    ..Default::default()
                },
                created_at_ms: now_ms - 7_200_000,
                last_active_ms: now_ms - ago_s * 1000,
                unread,
                order,
                cols: COLS,
                rows: ROWS,
                ..Default::default()
            }
        };
        let sessions = vec![
            mk(
                &ws1,
                0,
                AgentKind::Claude,
                "修复 berth-core 测试",
                ws1.root.clone(),
                AgentState::ToolRunning {
                    tool: "Bash".into(),
                },
                12,
                false,
            ),
            mk(
                &ws1,
                1,
                AgentKind::Codex,
                "重构 API 路由",
                ws1.root.join("crates/berth-daemon"),
                AgentState::WaitingPermission {
                    tool: Some("apply_patch".into()),
                },
                95,
                false,
            ),
            mk(
                &ws1,
                2,
                AgentKind::Shell,
                "",
                ws1.root.join("web"),
                AgentState::Idle,
                3600,
                false,
            ),
            mk(
                &ws2,
                0,
                AgentKind::Claude,
                "迁移数据库 schema",
                ws2.root.clone(),
                AgentState::Done,
                300,
                true,
            ),
            mk(
                &ws2,
                1,
                AgentKind::Claude,
                "生成周报",
                ws2.root.join("docs"),
                AgentState::Thinking,
                8,
                false,
            ),
        ];
        let previews = vec![
            preview(&[
                "$ cargo test -p berth-core",
                "running 12 tests",
                "test protocol::… FAILED",
            ]),
            preview(&[
                "codex 请求执行 apply_patch",
                "M crates/berth-daemon/src/server.rs",
                "允许？ [y/n]",
            ]),
            preview(&[
                "~/projects/berth/web",
                "$ pnpm dev",
                "  ➜  Local: http://localhost:5173/",
            ]),
            preview(&[
                "✓ 迁移完成：3 张表",
                "  users, sessions, audit_log",
                "Total cost: $0.42",
            ]),
            preview(&["✻ Thinking…", "读取 git log 最近 7 天", "汇总 23 个提交"]),
        ];
        let focused = sessions[0].id;
        Self {
            workspaces: vec![ws1, ws2],
            sessions,
            previews,
            focused,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_is_120_by_40_and_lines_fit() {
        let f = Fixture::build();
        assert_eq!((f.screen.cols, f.screen.rows), (COLS, ROWS));
        assert_eq!(f.screen.lines.len(), ROWS as usize);
        for (i, line) in f.screen.lines.iter().enumerate() {
            assert!(line.cells() <= COLS, "row {i} has {} cells", line.cells());
        }
    }

    #[test]
    fn required_content_is_present() {
        let f = Fixture::build();
        let all: String = f.screen.lines.iter().map(|l| l.text() + "\n").collect();
        for needle in [
            "中文汉字测试",
            "🚀",
            "👨\u{200d}👩\u{200d}👧",
            "🇨🇳",
            "┌──",
            "│",
            "└──",
            "┘",
            "\u{e0b0}",
            "+------",
        ] {
            assert!(all.contains(needle), "missing {needle:?}");
        }
        // Every SGR decoration appears on some cell.
        let table = f.interner.table();
        let used: CellFlags = f
            .screen
            .lines
            .iter()
            .flat_map(|l| l.runs.iter())
            .fold(CellFlags::empty(), |acc, r| acc | table.get(r.style).flags);
        for flag in [
            CellFlags::BOLD,
            CellFlags::ITALIC,
            CellFlags::DIM,
            CellFlags::UNDERLINE,
            CellFlags::DOUBLE_UNDERLINE,
            CellFlags::UNDERCURL,
            CellFlags::DOTTED_UNDERLINE,
            CellFlags::DASHED_UNDERLINE,
            CellFlags::STRIKEOUT,
            CellFlags::INVERSE,
            CellFlags::HIDDEN,
        ] {
            assert!(used.contains(flag), "flag {flag:?} unused");
        }
    }

    #[test]
    fn wide_alignment_rows_have_matching_separators() {
        let f = Fixture::build();
        let bars = |row: usize| {
            let mut col = 0u16;
            let mut out = Vec::new();
            for ch in f.screen.lines[row].text().chars() {
                if ch == '|' {
                    out.push(col);
                }
                col += char_cells(ch);
            }
            out
        };
        let a = bars(18);
        let b = bars(19);
        assert!(a.len() > 10);
        assert_eq!(a, b);
    }

    #[test]
    fn sidebar_fixture_shape() {
        let s = SidebarFixture::build(1_000_000_000);
        assert_eq!(s.workspaces.len(), 2);
        assert_eq!(s.sessions.len(), 5);
        assert!(s.previews.iter().all(|p| p.len() == 3));
        assert_eq!(s.sessions[2].title(), "web"); // falls back to cwd name
    }
}
