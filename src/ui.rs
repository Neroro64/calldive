//! ratatui UI: entry-point tree (left), per-function diff (right).

use crate::analysis::{self, Analysis, Status, TreeRow};
use crate::diffview::{fn_diff, DiffLine, Sign};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Direction},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::collections::HashMap;
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
    pub detail_scroll: u16,
    pub quit: bool,
    pub flash: Option<String>,
    /// rendered detail pane (header + diff) for the current selection
    pub detail: Vec<Line<'static>>,
    /// base revision sources, keyed by file path
    pub base_srcs: &'a HashMap<String, String>,
}

impl<'a> App<'a> {
    pub fn new(
        a: &'a Analysis,
        roots: Vec<String>,
        focus_mode: bool,
        base_srcs: &'a HashMap<String, String>,
    ) -> Self {
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
            quit: false,
            flash: None,
            detail: vec![],
            base_srcs,
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
        let n = n.clone();

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
        self.detail.push(Line::from(Span::styled(
            format!("{} : rows {}-{}", n.file, n.start_row + 1, n.end_row),
            Style::default().fg(Color::DarkGray),
        )));

        // callers (upstream impact)
        let in_scope: Vec<&String> = n
            .callers
            .iter()
            .filter(|c| self.a.nodes.contains_key(*c))
            .collect();
        let external: Vec<&String> = n
            .callers
            .iter()
            .filter(|c| !self.a.nodes.contains_key(*c))
            .collect();
        let mut cl = vec![Span::raw("callers: ")];
        if in_scope.is_empty() && external.is_empty() {
            cl.push(Span::styled(
                "none (entry point)",
                Style::default().fg(Color::Cyan),
            ));
        } else {
            for (i, c) in in_scope.iter().enumerate() {
                if i > 0 {
                    cl.push(Span::raw(", "));
                }
                if let Some(cn) = self.a.nodes.get(*c) {
                    cl.push(Span::styled(
                        cn.name.clone(),
                        Style::default().fg(status_color(cn.status)),
                    ));
                }
                if i == 5 {
                    cl.push(Span::raw(" …"));
                }
            }
            if !external.is_empty() {
                if !in_scope.is_empty() {
                    cl.push(Span::raw(", "));
                }
                let names: Vec<String> = external
                    .iter()
                    .take(3)
                    .map(|k| k.rsplit("::").next().unwrap_or(k).to_string())
                    .collect();
                cl.push(Span::styled(
                    format!("{} (outside diff)", names.join(", ")),
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
            for c in &n.callees {
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
            for ext in &n.lost_callees {
                if !first {
                    cal.push(Span::raw(", "));
                }
                first = false;
                cal.push(Span::styled(
                    format!("{ext} (ext)"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        self.detail.push(Line::from(cal));
        self.detail.push(Line::from(""));

        // diff: extract body slices from base/head sources
        let head_src: Option<String> = if n.status == Status::Removed {
            None
        } else {
            Some(slice_rows(&n.src, n.start_row, n.end_row))
        };
        let base_src: Option<String> = match n.status {
            Status::Added => None,
            _ => self
                .base_srcs
                .get(&n.file)
                .map(|src| slice_rows(src, n.start_row, n.end_row)),
        };
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
                self.detail.push(Line::from(Span::styled(dl.text, style)));
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
                    self.detail_scroll = u16::MAX / 2;
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
    }
}

fn slice_rows(src: &str, start: usize, end: usize) -> String {
    src.lines()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect::<Vec<_>>()
        .join("\n")
}

fn expand_all(a: &Analysis, key: &str, expanded: &mut HashSet<String>, depth: usize) {
    if depth == 0 {
        return;
    }
    expanded.insert(key.to_string());
    if let Some(n) = a.nodes.get(key) {
        for c in n.callees.clone() {
            expand_all(a, &c, expanded, depth - 1);
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

fn short_file(f: &str) -> String {
    f.rsplit('/').next().unwrap_or(f).to_string()
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
                    format!("  {}::{}", short_file(&n.file), n.start_row + 1),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if sel {
                for s in spans.iter_mut() {
                    s.style = s.style.add_modifier(Modifier::REVERSED);
                }
            }
            ListItem::new(Line::from(spans))
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
    let para = Paragraph::new(app.detail.clone())
        .block(detail_block)
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(para, main[1]);

    // footer
    let flash = app.flash.take();
    let help = if let Some(msg) = flash {
        format!(" {msg}")
    } else {
        " j/k move · enter expand · x expand subtree · f focus changes · tab pane · q quit "
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
