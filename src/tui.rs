//! ratatui render + fuzzy filter + input loop.
//!
//! The TUI reuses `herdr_pretty_which::theme::Palette` so the palette inherits
//! the user's configured Herdr theme colors (loaded via `Palette::from_theme`)
//! instead of hardcoding its own. Layout is a Raycast/Linear-style centered
//! overlay: a prompt line, a scrollable list of matched rows grouped by kind,
//! and a footer hint.

use crate::items::{Item, ItemKind};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use herdr_pretty_which::theme::Palette;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{self};

/// Outcome of one interactive run.
pub enum Outcome {
    /// User picked an item; carry its dispatch out to the caller.
    Selected(Item),
    /// User cancelled (Esc / Ctrl+C / Ctrl+D / empty Enter).
    Cancelled,
}

/// Run the palette overlay over the live terminal. Restores raw mode + alt
/// screen on return, including on error paths.
pub fn run(
    items: Vec<Item>,
    palette: Palette,
    header: &str,
    initial_query: &str,
    config_path: &str,
) -> Result<Outcome> {
    let mut state = PaletteState::new(items, header.to_string(), initial_query);
    state.config_path = config_path.to_string();
    run_loop(&mut state, palette)
}

struct PaletteState {
    all: Vec<Item>,
    matched: Vec<MatchedItem>,
    matcher: SkimMatcherV2,
    list_state: ListState,
    query: String,
    header: String,
    config_path: String,
}

struct MatchedItem {
    item: Item,
    score: i64,
}

impl PaletteState {
    fn new(items: Vec<Item>, header: String, initial_query: &str) -> Self {
        let matcher = SkimMatcherV2::default().ignore_case();
        let mut s = Self {
            all: items,
            matched: Vec::new(),
            matcher,
            list_state: ListState::default(),
            query: initial_query.to_string(),
            header,
            config_path: String::new(),
        };
        s.recompute_matches();
        s.list_state.select(Some(0));
        s
    }

    fn recompute_matches(&mut self) {
        let q = self.query.trim();
        let mut hits: Vec<MatchedItem> = if q.is_empty() {
            self.all
                .iter()
                .map(|item| MatchedItem {
                    item: item.clone(),
                    score: 0,
                })
                .collect()
        } else {
            self.all
                .iter()
                .filter_map(|item| {
                    let score = self.matcher.fuzzy_match(&item.haystack(), q)?;
                    Some(MatchedItem {
                        item: item.clone(),
                        score,
                    })
                })
                .collect()
        };
        if !q.is_empty() {
            hits.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| a.item.title.cmp(&b.item.title))
            });
        }
        // Keep the selection valid after the list shrinks.
        let len = hits.len();
        let sel = self.list_state.selected().unwrap_or(0).min(len.saturating_sub(1));
        self.list_state
            .select(if len == 0 { None } else { Some(sel) });
        self.matched = hits;
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.matched.len();
        if len == 0 {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let next = ((cur + delta).rem_euclid(len as i32)) as usize;
        self.list_state.select(Some(next));
    }

    fn selected(&self) -> Option<&Item> {
        self.list_state
            .selected()
            .and_then(|i| self.matched.get(i))
            .map(|m| &m.item)
    }
}

fn run_loop(state: &mut PaletteState, palette: Palette) -> Result<Outcome> {
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    enable_raw_mode()?;
    let _raw_guard = RawModeGuard;
    execute!(&mut io::stdout(), EnterAlternateScreen)?;
    let _alt_guard = AltScreenGuard;

    let mut terminal = Terminal::new(backend)?;
    loop {
        terminal.draw(|f| draw(f, state, palette))?;
        if !event::poll(std::time::Duration::from_millis(250))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                    return Ok(Outcome::Cancelled);
                }
                (KeyCode::Enter, _) => match state.selected() {
                    Some(item) if item.is_dispatchable() => {
                        return Ok(Outcome::Selected(item.clone()));
                    }
                    Some(_) => {
                        continue;
                    }
                    None => return Ok(Outcome::Cancelled),
                },
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                    state.move_selection(1);
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                    state.move_selection(-1);
                }
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    state.query.clear();
                    state.recompute_matches();
                }
                (KeyCode::Backspace, _) => {
                    state.query.pop();
                    state.recompute_matches();
                }
                (KeyCode::Char(ch), mods) if !mods.contains(KeyModifiers::CONTROL) => {
                    state.query.push(ch);
                    state.recompute_matches();
                }
                _ => {}
            }
        }
    }
}

/// Restores cooked terminal mode on drop, including on `?` error paths or
/// panic, so a failure mid-loop never leaves the user's terminal in raw mode.
struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Leaves the alternate screen on drop for the same reason.
struct AltScreenGuard;
impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

fn draw(f: &mut Frame<'_>, state: &mut PaletteState, palette: Palette) {
    let area = f.area();
    f.render_widget(Clear, area);

    let (overlay, _) = centered_rect(area, 80, 60);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // prompt
            Constraint::Min(1),    // list
            Constraint::Length(1), // breathing room between results and footer/help
            Constraint::Length(1), // footer
        ])
        .split(overlay);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![Span::styled(
            format!(" {} ", state.header),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )]))
        .style(Style::default().bg(palette.panel).fg(palette.text));
    f.render_widget(block, overlay);

    // Prompt line.
    let prompt = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(palette.accent)),
        Span::styled(
            state.query.as_str(),
            Style::default().fg(palette.text),
        ),
        Span::styled(
            "▏",
            Style::default().fg(palette.muted),
        ),
    ]))
    .style(Style::default().bg(palette.panel));
    f.render_widget(prompt, chunks[0]);

    // List.
    let items: Vec<ListItem<'_>> = state
        .matched
        .iter()
        .map(|m| render_row(&m.item, palette))
        .collect();
    let list = List::new(items)
        .style(Style::default().bg(palette.panel))
        .highlight_style(
            Style::default()
                .bg(palette.panel_alt)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut state.list_state);

    // Breathing room between the last result row and the footer/help line.
    f.render_widget(
        Paragraph::new(Line::default()).style(Style::default().bg(palette.panel)),
        chunks[2],
    );

    // Footer.
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(
                "Enter run · ↑↓/Ctrl-n-p select · Esc cancel · Ctrl-u clear · {}",
                state.config_path
            ),
            Style::default().fg(palette.muted),
        ),
    ]))
    .style(Style::default().bg(palette.panel).fg(palette.muted));
    f.render_widget(footer, chunks[3]);
}

fn render_row(item: &Item, palette: Palette) -> ListItem<'_> {
    let title_style = if item.is_dispatchable() {
        Style::default().fg(palette.text)
    } else {
        // Reference-only: dimmed so users know Enter won't fire it.
        Style::default().fg(palette.muted).add_modifier(Modifier::DIM)
    };
    let kind_style = Style::default().fg(accent_for_kind(item.kind, palette));
    let keys_style = Style::default().fg(palette.muted);

    let mut line = Line::default();
    line.spans.push(Span::styled(format!("{}  ", item.title), title_style));
    if !item.subtitle.is_empty() && item.subtitle != item.title {
        line.spans.push(Span::styled(
            format!("{} ", item.subtitle),
            Style::default().fg(palette.muted),
        ));
    }
    line.spans.push(Span::styled(
        format!("[{}]", item.kind.category_label()),
        kind_style,
    ));
    if !item.keys.is_empty() {
        line.spans.push(Span::styled(
            format!("  {}", item.keys.join(", ")),
            keys_style,
        ));
    }
    ListItem::new(line)
}

fn accent_for_kind(kind: ItemKind, palette: Palette) -> ratatui::style::Color {
    use ItemKind::*;
    match kind {
        Binding => palette.accent,
        PluginAction => palette.success,
        CustomCommand => palette.warning,
        JumpWorkspace => palette.accent,
        JumpTab => palette.accent,
        JumpAgent => palette.accent,
    }
}

/// Center a rect of relative width/height percent inside `area`.
fn centered_rect(area: Rect, width_pct: u16, height_pct: u16) -> (Rect, Rect) {
    let pop = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area)[1];
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(pop)[1];
    (mid, area)
}

/// Render the palette to a string for snapshot tests (non-interactive).
pub fn render_snapshot(
    items: Vec<Item>,
    palette: Palette,
    header: &str,
    query: &str,
    width: u16,
    height: u16,
) -> Result<String> {
    use ratatui::backend::TestBackend;
    let mut state = PaletteState::new(items, header.to_string(), query);
    // Reflow the highlight to row 0 for deterministic snapshots.
    state.list_state.select(Some(0));
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|f| draw(f, &mut state, palette))?;
    let buffer = terminal.backend().buffer().clone();
    Ok(render_buffer(&buffer, width, height))
}

fn render_buffer(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
    let mut out = String::with_capacity((width as usize) * (height as usize));
    for y in 0..height {
        for x in 0..width {
            let cell = &buffer[(x, y)];
            out.push_str(cell.symbol());
        }
        // Trim trailing whitespace per line for cleaner snapshots.
        out.truncate(out.trim_end().len());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::Dispatch;
    use herdr_pretty_which::theme::Palette;
    use ratatui::style::Color;

    fn sample_palette() -> Palette {
        Palette {
            bg: Color::Reset,
            panel: Color::Rgb(30, 30, 40),
            panel_alt: Color::Rgb(45, 45, 60),
            text: Color::Rgb(220, 220, 230),
            muted: Color::Rgb(120, 120, 140),
            accent: Color::Rgb(136, 57, 239),
            accent_2: Color::Rgb(80, 150, 220),
            success: Color::Rgb(80, 200, 120),
            warning: Color::Rgb(220, 160, 60),
            danger: Color::Rgb(220, 80, 80),
        }
    }

    fn sample_items() -> Vec<Item> {
        vec![
            Item {
                kind: ItemKind::Binding,
                title: "Split vertical".into(),
                subtitle: "Split side by side.".into(),
                keys: vec!["prefix+v".into()],
                dispatch: Some(Dispatch::Cli(vec!["herdr".into(), "pane".into(), "split".into(), "right".into()])),
            },
            Item {
                kind: ItemKind::Binding,
                title: "Help".into(),
                subtitle: "Open key map.".into(),
                keys: vec!["prefix+?".into()],
                dispatch: None,
            },
            Item {
                kind: ItemKind::PluginAction,
                title: "Open pretty which".into(),
                subtitle: "ramarivera.pretty-which.open".into(),
                keys: vec![],
                dispatch: Some(Dispatch::Cli(vec!["herdr".into()])),
            },
        ]
    }

    #[test]
    fn empty_query_matches_everything() {
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "");
        assert_eq!(s.matched.len(), 3);
        s.query = "split".into();
        s.recompute_matches();
        assert!(s.matched.len() <= 3);
        assert!(s.matched.iter().any(|m| m.item.title.contains("Split")));
    }

    #[test]
    fn fuzzy_ranks_split_above_help() {
        let s = PaletteState::new(sample_items(), "Palette".into(), "split");
        assert_eq!(s.matched.first().map(|m| m.item.title.as_str()), Some("Split vertical"));
    }

    #[test]
    fn move_selection_wraps() {
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "");
        s.list_state.select(Some(0));
        s.move_selection(-1); // wrap to last
        assert_eq!(s.list_state.selected(), Some(2));
        s.move_selection(1); // wrap back to first
        assert_eq!(s.list_state.selected(), Some(0));
    }

    #[test]
    fn snapshot_renders_without_panic() {
        let p = sample_palette();
        let out = render_snapshot(sample_items(), p, "Palette", "", 90, 16).unwrap();
        assert!(out.contains("Palette"));
        assert!(out.contains("Split vertical"));
    }
}
