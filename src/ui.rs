//! ratatui UI: entry-point tree (left), per-function diff (right).

use crate::analysis::{self, Analysis, Status, TreeRow};
use crate::diffview::{fn_diff, DiffLine, Sign};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Direction},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Detail,
}

pub struct App<'a> {
    pub a: &'a Analysis,
    pub roots: Vec<String>,
    pub expanded: HashSet<String>,
    pub focus_mode: bool,
    pub rows: Vec<TreeRow>,
    pub sel: usize,
    pub tree_state: ListState,
    pub focus: Focus,
    pub detail_scroll: usize,
    pub detail_viewport: usize,
    pub tree_horizontal: u16,
    pub detail_horizontal: u16,
    pub quit: bool,
    pub flash: Option<String>,
    /// rendered detail pane (header + diff) for the current selection
    pub detail: Vec<Line<'static>>,
}

impl<'a> App<'a> {
    pub fn new(a: &'a Analysis, roots: Vec<String>, focus_mode: bool) -> Self {
        let mut app = Self {
            a,
            roots,
            expanded: HashSet::new(),
            focus_mode,
            rows: vec![],
            sel: 0,
            tree_state: ListState::default(),
            focus: Focus::Tree,
            detail_scroll: 0,
            detail_viewport: 0,
            tree_horizontal: 0,
            detail_horizontal: 0,
            quit: false,
            flash: None,
            detail: vec![],
        };
        let roots = app.roots.clone();
        for r in &roots {
            if a.nodes.get(r).map(|n| n.relevant).unwrap_or(false) {
                app.expanded.insert(r.clone());
            }
        }
        app.rebuild();
        app.update_detail();
        app
    }

    pub fn rebuild(&mut self) {
        let roots = self.roots.clone();
        self.rows = analysis::build_rows(self.a, &roots, &self.expanded, self.focus_mode);
        if self.sel >= self.rows.len() {
            self.sel = self.rows.len().saturating_sub(1);
        }
    }

    fn selected_key(&self) -> Option<&String> {
        self.rows.get(self.sel).map(|r| &r.key)
    }

    /// Recompute the detail pane for the current selection.
    pub fn update_detail(&mut self) {
        self.detail.clear();
        let Some(key) = self.selected_key().cloned() else {
            return;
        };
        let Some(n) = self.a.nodes.get(&key) else {
            return;
        };

        // header
        self.detail.push(Line::from(vec![
            Span::styled(
                n.name.clone(),
                Style::default()
                    .fg(status_color(n.status))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("[{:?}]", n.status),
                Style::default().fg(status_color(n.status)),
            ),
        ]));
        if let Some(span) = n.head.as_ref().or(n.base.as_ref()) {
            self.detail.push(Line::from(Span::styled(
                format!(
                    "{} : rows {}-{}",
                    self.a.label(&span.file),
                    span.start_row + 1,
                    span.end_row
                ),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if n.status == Status::Moved {
            if let (Some(base), Some(origin)) = (&n.base, &n.moved_from) {
                self.detail.push(Line::from(format!(
                    "moved from {}::{origin}",
                    self.a.label(&base.file)
                )));
            }
        }

        // callers (upstream impact)
        let in_scope = &n.callers;
        let external = &n.external_callers;
        let mut cl = vec![Span::raw("callers: ")];
        if in_scope.is_empty() && external.is_empty() {
            cl.push(Span::styled(
                "none (entry point)",
                Style::default().fg(Color::Cyan),
            ));
        } else {
            for (i, c) in in_scope.iter().take(5).enumerate() {
                if i > 0 {
                    cl.push(Span::raw(", "));
                }
                if let Some(cn) = self.a.nodes.get(c) {
                    cl.push(Span::styled(
                        cn.name.clone(),
                        Style::default().fg(status_color(cn.status)),
                    ));
                }
            }
            if in_scope.len() > 5 {
                cl.push(Span::raw(format!(" … +{} more", in_scope.len() - 5)));
            }
            if !external.is_empty() {
                if !in_scope.is_empty() {
                    cl.push(Span::raw(", "));
                }
                let names: Vec<String> = external
                    .iter()
                    .take(3)
                    .map(|k| match k.split_once("::") {
                        Some((path, qual)) => format!("{}::{qual}", self.a.label(path)),
                        None => self.a.label(k).to_string(),
                    })
                    .collect();
                let more = if external.len() > 3 {
                    format!(" +{} more", external.len() - 3)
                } else {
                    String::new()
                };
                cl.push(Span::styled(
                    format!("{}{more} (outside diff)", names.join(", ")),
                    Style::default().fg(Color::Cyan),
                ));
            }
        }
        self.detail.push(Line::from(cl));

        // callees (downstream)
        let mut cal = vec![Span::raw("callees: ")];
        if n.callees.is_empty() && n.lost_callees.is_empty() {
            cal.push(Span::styled("leaf", Style::default().fg(Color::DarkGray)));
        } else {
            let mut first = true;
            for c in n.callees.iter().take(8) {
                if !first {
                    cal.push(Span::raw(", "));
                }
                first = false;
                if let Some(cn) = self.a.nodes.get(c) {
                    cal.push(Span::styled(
                        cn.name.clone(),
                        Style::default().fg(status_color(cn.status)),
                    ));
                }
            }
            for ext in n
                .lost_callees
                .iter()
                .take(8usize.saturating_sub(n.callees.len()))
            {
                if !first {
                    cal.push(Span::raw(", "));
                }
                first = false;
                cal.push(Span::styled(
                    format!("{ext} (ext)"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let total = n.callees.len() + n.lost_callees.len();
            if total > 8 {
                cal.push(Span::raw(format!(" … +{} more", total - 8)));
            }
        }
        self.detail.push(Line::from(cal));
        self.detail.push(Line::from(""));

        let head_src = self.a.head_body(n);
        let base_src = self.a.base_body(n);
        let diff: Vec<DiffLine> = fn_diff(base_src.as_deref(), head_src.as_deref());
        if diff.is_empty() {
            self.detail.push(Line::from(Span::styled(
                "(no textual difference)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for dl in diff {
                let style = match dl.sign {
                    Sign::Plus => Style::default().fg(Color::Green),
                    Sign::Minus => Style::default().fg(Color::Red),
                    Sign::Same => Style::default().fg(Color::DarkGray),
                };
                self.detail.push(Line::from(Span::styled(
                    format!("{}{}", dl.sign.marker(), dl.text),
                    style,
                )));
            }
        }
    }

    fn toggle_expand(&mut self) {
        let Some(k) = self.selected_key().cloned() else {
            return;
        };
        let has_children = self
            .rows
            .get(self.sel)
            .map(|r| r.has_children)
            .unwrap_or(false);
        if has_children {
            if self.expanded.contains(&k) {
                self.expanded.remove(&k);
            } else {
                self.expanded.insert(k);
            }
            self.rebuild();
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('H') | KeyCode::Char('L') => {
                let offset = match self.focus {
                    Focus::Tree => &mut self.tree_horizontal,
                    Focus::Detail => &mut self.detail_horizontal,
                };
                *offset = if code == KeyCode::Char('H') {
                    offset.saturating_sub(1)
                } else {
                    offset.saturating_add(1)
                };
            }
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Tree {
                    if self.sel + 1 < self.rows.len() {
                        self.sel += 1;
                        self.detail_scroll = 0;
                        self.update_detail();
                    }
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Tree {
                    self.sel = self.sel.saturating_sub(1);
                    self.detail_scroll = 0;
                    self.update_detail();
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if self.focus == Focus::Tree {
                    self.sel = 0;
                    self.update_detail();
                } else {
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.focus == Focus::Tree {
                    self.sel = self.rows.len().saturating_sub(1);
                    self.update_detail();
                } else {
                    self.detail_scroll = self.max_detail_scroll();
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') => self.toggle_expand(),
            KeyCode::Char('h') => {
                let Some(r) = self.rows.get(self.sel) else {
                    return;
                };
                let k = r.key.clone();
                if self.expanded.contains(&k) {
                    self.expanded.remove(&k);
                } else if r.depth > 0 {
                    // collapse the nearest expanded ancestor above
                    for row in self.rows[..self.sel].iter().rev() {
                        if row.depth < r.depth && self.expanded.contains(&row.key) {
                            self.expanded.remove(&row.key);
                            break;
                        }
                    }
                }
                self.rebuild();
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Detail,
                    Focus::Detail => Focus::Tree,
                };
            }
            KeyCode::Char('f') => {
                self.focus_mode = !self.focus_mode;
                self.rebuild();
                self.flash = Some(if self.focus_mode {
                    "focus: changes only".into()
                } else {
                    "focus: full subtree".into()
                });
            }
            KeyCode::Char('e') => {
                self.expanded.clear();
                let roots = self.roots.clone();
                for r in &roots {
                    if self.a.nodes.get(r).map(|n| n.relevant).unwrap_or(false) {
                        self.expanded.insert(r.clone());
                    }
                }
                self.rebuild();
                self.flash = Some("collapsed to roots".into());
            }
            KeyCode::Char('x') => {
                if let Some(k) = self.selected_key().cloned() {
                    expand_all(self.a, &k, &mut self.expanded, 5);
                    self.rebuild();
                    self.flash = Some("expanded subtree".into());
                }
            }
            _ => {}
        }
        self.detail_scroll = self.detail_scroll.min(self.max_detail_scroll());
    }

    fn max_detail_scroll(&self) -> usize {
        self.detail.len().saturating_sub(self.detail_viewport)
    }
}

fn expand_all(a: &Analysis, key: &str, expanded: &mut HashSet<String>, depth: usize) {
    if depth == 0 {
        return;
    }
    expanded.insert(key.to_string());
    if let Some(n) = a.nodes.get(key) {
        for c in &n.callees {
            expand_all(a, c, expanded, depth - 1);
        }
    }
}

fn status_color(s: Status) -> Color {
    match s {
        Status::Added => Color::Green,
        Status::Removed => Color::Red,
        Status::BodyChanged => Color::Yellow,
        Status::Moved => Color::Cyan,
        Status::Unchanged => Color::DarkGray,
    }
}

pub fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let root = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(f.area());

    let (ad, rm, ch, mv) = app.a.summary;
    let header = Line::from(vec![
        Span::styled(
            " calldive ",
            Style::default().bg(Color::Blue).fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(format!("+{ad}"), Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("-{rm}"), Style::default().fg(Color::Red)),
        Span::raw(" "),
        Span::styled(format!("~{ch}"), Style::default().fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled(format!(">{mv}"), Style::default().fg(Color::Cyan)),
        Span::raw(format!(
            "   entry points: {}   focus: {}",
            app.roots.len(),
            if app.focus_mode { "changes" } else { "full" }
        )),
    ]);
    f.render_widget(Paragraph::new(header), root[0]);

    let main = ratatui::layout::Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(root[1]);

    // left: tree
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let n = &app.a.nodes[&r.key];
            let sel = i == app.sel && app.focus == Focus::Tree;
            let indent = "  ".repeat(r.depth);
            let expand_glyph = if r.has_children {
                if app.expanded.contains(&r.key) {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "  "
            };
            let name_style = if n.status.is_change() {
                Style::default()
                    .fg(status_color(n.status))
                    .add_modifier(Modifier::BOLD)
            } else if n.relevant {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let mut spans = vec![
                Span::raw(indent),
                Span::raw(expand_glyph),
                Span::styled(
                    n.status.glyph().to_string() + " ",
                    Style::default().fg(status_color(n.status)),
                ),
                Span::styled(n.name.clone(), name_style),
                Span::styled(
                    format!(
                        "  {}:{}",
                        app.a.label(&n.file),
                        n.head
                            .as_ref()
                            .or(n.base.as_ref())
                            .map_or(0, |span| span.start_row + 1)
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if sel {
                for s in spans.iter_mut() {
                    s.style = s.style.add_modifier(Modifier::REVERSED);
                }
            }
            ListItem::new(scroll_line(Line::from(spans), app.tree_horizontal))
        })
        .collect();

    let tree_block = Block::default()
        .borders(Borders::ALL)
        .title(" call tree (DFS from entry points) ")
        .border_style(if app.focus == Focus::Tree {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    app.tree_state.select(Some(app.sel));
    let list = List::new(items)
        .block(tree_block)
        .highlight_style(Style::default());
    f.render_stateful_widget(list, main[0], &mut app.tree_state);

    // right: detail
    let detail_block = Block::default()
        .borders(Borders::ALL)
        .title(" function diff ")
        .border_style(if app.focus == Focus::Detail {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    app.detail_viewport = usize::from(detail_block.inner(main[1]).height);
    app.detail_scroll = app.detail_scroll.min(app.max_detail_scroll());
    let end = (app.detail_scroll + app.detail_viewport).min(app.detail.len());
    let visible: Vec<Line<'_>> = app.detail[app.detail_scroll..end]
        .iter()
        .map(|line| {
            Line::from(
                line.spans
                    .iter()
                    .map(|span| Span::styled(span.content.as_ref(), span.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let para = Paragraph::new(visible)
        .scroll((0, app.detail_horizontal))
        .block(detail_block);
    f.render_widget(para, main[1]);

    // footer
    let flash = app.flash.take();
    let help = if let Some(msg) = flash {
        format!(" {msg}")
    } else {
        " j/k move · H/L scroll horizontally · g/G top/bottom · enter expand · x expand subtree · e collapse to roots · f focus changes · tab pane · q quit "
            .to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        ))),
        root[2],
    );
}

/// Clip by terminal columns while preserving styles and grapheme boundaries.
fn scroll_line(line: Line<'static>, offset: u16) -> Line<'static> {
    if offset == 0 {
        return line;
    }
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut skip = usize::from(offset);
    let mut spans = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            let width = grapheme.width();
            if skip > 0 {
                if width > skip {
                    text.push_str(&" ".repeat(width - skip));
                }
                skip = skip.saturating_sub(width);
            } else {
                text.push_str(grapheme);
            }
        }
        spans.push(Span::styled(text, span.style));
    }
    Line::from(spans)
}

pub fn run(app: &mut App) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let res = (|| -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| draw(f, app))?;
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.handle_key(k.code);
                }
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_scroll_moves_only_focused_panel_content() {
        use ratatui::{backend::TestBackend, Terminal};
        let a = analyze("fn target() { let z = 3; }", "fn target() { let z = 4; }");
        let mut app = target_app(&a);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let before = terminal.backend().buffer().clone();
        app.handle_key(KeyCode::Char('L'));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let after = terminal.backend().buffer();
        assert_eq!(after[(1, 2)].symbol(), before[(2, 2)].symbol());
        assert_eq!(after[(46, 2)], before[(46, 2)]);
        app.handle_key(KeyCode::Tab);
        app.handle_key(KeyCode::Char('L'));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(46, 2)].symbol(), "a");
        app.handle_key(KeyCode::Char('H'));
        app.handle_key(KeyCode::Char('H'));
        assert_eq!(app.detail_horizontal, 0);
        assert_eq!(app.tree_horizontal, 1);
    }

    #[test]
    fn horizontal_clip_preserves_wide_and_combining_graphemes() {
        assert_eq!(
            plain(&scroll_line(Line::from("界e\u{301}x"), 1)),
            " e\u{301}x"
        );
        assert_eq!(plain(&scroll_line(Line::from("界e\u{301}x"), 3)), "x");
    }

    fn analyze(base: &str, head: &str) -> Analysis {
        analysis::analyze(&analysis::Inputs {
            files: vec![("src/lib.rs".into(), Some(base.into()), Some(head.into()))],
            extra_head: vec![],
            include_tests: true,
        })
    }

    fn target_app(a: &Analysis) -> App<'_> {
        let key = analysis::node_key("src/lib.rs", "target");
        App::new(a, vec![key], false)
    }

    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn shifted_function_diff_uses_each_revision_span() {
        let base = "fn target() {\n    let z = 3;\n}\n";
        let head = "// one\n// two\n// three\nfn target() {\n    let z = 4;\n}\n";
        let a = analyze(base, head);
        let app = target_app(&a);
        let lines: Vec<String> = app.detail.iter().map(plain).collect();
        assert!(lines.iter().any(|line| line == "-     let z = 3;"));
        assert!(lines.iter().any(|line| line == "+     let z = 4;"));
    }

    #[test]
    fn callers_are_capped_with_exact_remainder() {
        let mut base = String::from("fn target() { let z = 3; }\n");
        for i in 0..7 {
            base.push_str(&format!("fn caller{i}() {{ target(); }}\n"));
        }
        let head = base.replace("let z = 3", "let z = 4");
        let a = analyze(&base, &head);
        let app = target_app(&a);
        let line = app
            .detail
            .iter()
            .map(plain)
            .find(|line| line.starts_with("callers: "))
            .unwrap();
        assert!(line.contains("… +2 more"));
        for i in 0..5 {
            assert!(line.contains(&format!("caller{i}")));
        }
        assert!(!line.contains("caller5"));
        assert!(!line.contains("caller6"));
    }

    #[test]
    fn detail_scroll_stops_at_last_full_viewport() {
        let a = analyze(
            "fn target() {\n    let z = 3;\n}\n",
            "fn target() {\n    let z = 4;\n}\n",
        );
        let mut app = target_app(&a);
        app.focus = Focus::Detail;
        app.detail_viewport = 3;
        let max = app.detail.len() - app.detail_viewport;
        app.handle_key(KeyCode::Char('G'));
        assert_eq!(app.detail_scroll, max);
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Down);
        assert_eq!(app.detail_scroll, max);
        app.handle_key(KeyCode::Home);
        assert_eq!(app.detail_scroll, 0);
        app.handle_key(KeyCode::End);
        assert_eq!(app.detail_scroll, max);
    }
}
