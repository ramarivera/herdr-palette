//! ratatui render + fuzzy filter + input loop.
//!
//! The TUI reuses `herdr_pretty_which::theme::Palette` so the palette inherits
//! the user's configured Herdr theme colors (loaded via `Palette::from_theme`)
//! instead of hardcoding its own. Layout is a Raycast/Linear-style centered
//! overlay: a prompt line, a scrollable list/tree of matched rows, and a footer
//! hint.

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
use herdr_pretty_which::{binding_search_score, NavigationViewMode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::BTreeSet;
use std::io::{self};

/// Outcome of one interactive run.
pub enum Outcome {
    /// User picked an item; carry its dispatch out to the caller.
    Selected(Box<Item>),
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
    navigation_view: NavigationViewMode,
    collapsed_tree_paths: BTreeSet<String>,
    selected: usize,
}

#[derive(Debug, Clone)]
struct MatchedItem {
    item: Item,
    score: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeRowKind {
    Group,
    Item,
}

#[derive(Debug, Clone)]
struct TreeRow {
    path: Vec<String>,
    label: String,
    depth: usize,
    kind: TreeRowKind,
    selectable: bool,
    context_only: bool,
    expanded: bool,
    item: Option<Item>,
    score: Option<i64>,
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
            navigation_view: NavigationViewMode::Tree,
            collapsed_tree_paths: BTreeSet::new(),
            selected: 0,
        };
        s.recompute_matches();
        s
    }

    fn recompute_matches(&mut self) {
        let q = self.query.trim();
        let mut hits = self
            .all
            .iter()
            .filter_map(|item| self.match_item(item, q))
            .collect::<Vec<_>>();
        sort_flat_matches(&mut hits);
        self.matched = hits;

        let selectable_len = self.selectable_len();
        if selectable_len == 0 {
            self.selected = 0;
            self.list_state.select(None);
            return;
        }

        if !q.is_empty() {
            self.selected = self.best_selectable_index().unwrap_or(0);
        } else {
            self.selected = self.selected.min(selectable_len - 1);
        }
        self.sync_list_selection();
    }

    fn match_item(&self, item: &Item, query: &str) -> Option<MatchedItem> {
        if query.is_empty() {
            return Some(MatchedItem {
                item: item.clone(),
                score: None,
            });
        }
        let score = if let Some(binding) = item.binding.as_ref() {
            binding_search_score(binding, query)
        } else {
            self.matcher.fuzzy_match(&item.haystack(), query)
        }?;
        Some(MatchedItem {
            item: item.clone(),
            score: Some(score),
        })
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.selectable_len();
        if len == 0 {
            return;
        }
        let cur = self.selected as i32;
        self.selected = ((cur + delta).rem_euclid(len as i32)) as usize;
        self.sync_list_selection();
    }

    fn selected(&self) -> Option<Item> {
        match self.navigation_view {
            NavigationViewMode::List => self.matched.get(self.selected).map(|m| m.item.clone()),
            NavigationViewMode::Tree => self.selected_tree_row().and_then(|row| row.item),
        }
    }

    fn toggle_navigation_view(&mut self) {
        let selected = self.selected().map(|item| item_identity(&item));
        self.navigation_view = match self.navigation_view {
            NavigationViewMode::List => NavigationViewMode::Tree,
            NavigationViewMode::Tree => NavigationViewMode::List,
        };
        self.selected = 0;
        if let Some(identity) = selected {
            self.select_identity(&identity);
        }
        self.sync_list_selection();
    }

    fn tree_left(&mut self) {
        if self.navigation_view != NavigationViewMode::Tree || !self.query.trim().is_empty() {
            return;
        }
        let Some(row) = self.selected_tree_row() else {
            return;
        };
        match row.kind {
            TreeRowKind::Item => {
                let parent = row.path[..row.path.len().saturating_sub(1)].to_vec();
                self.select_tree_path(&parent);
            }
            TreeRowKind::Group if row.expanded => {
                self.collapsed_tree_paths.insert(path_key(&row.path));
                self.select_tree_path(&row.path);
            }
            TreeRowKind::Group if row.path.len() > 1 => {
                let parent = row.path[..row.path.len() - 1].to_vec();
                self.select_tree_path(&parent);
            }
            TreeRowKind::Group => {}
        }
        self.sync_list_selection();
    }

    fn tree_right(&mut self) {
        if self.navigation_view != NavigationViewMode::Tree || !self.query.trim().is_empty() {
            return;
        }
        let Some(row) = self.selected_tree_row() else {
            return;
        };
        if row.kind != TreeRowKind::Group {
            return;
        }
        let key = path_key(&row.path);
        if self.collapsed_tree_paths.remove(&key) {
            self.select_tree_path(&row.path);
            self.sync_list_selection();
            return;
        }
        if let Some(child) = self
            .visible_tree_rows()
            .into_iter()
            .filter(|candidate| candidate.selectable)
            .find(|candidate| candidate.path.starts_with(&row.path) && candidate.path != row.path)
        {
            self.select_tree_path(&child.path);
        }
        self.sync_list_selection();
    }

    fn expand_all_tree_nodes(&mut self) {
        self.collapsed_tree_paths.clear();
        self.sync_list_selection();
    }

    fn collapse_all_tree_nodes(&mut self) {
        let mut collapsed = BTreeSet::new();
        for item in &self.all {
            let mut path = Vec::new();
            for segment in tree_path_for_item(item) {
                path.push(segment);
                collapsed.insert(path_key(&path));
            }
        }
        self.collapsed_tree_paths = collapsed;
        self.selected = 0;
        self.sync_list_selection();
    }

    fn visible_tree_rows(&self) -> Vec<TreeRow> {
        let query = self.query.trim();
        let query_active = !query.is_empty();
        let mut rows = Vec::new();
        let mut emitted_groups = BTreeSet::new();

        for item in self
            .all
            .iter()
            .filter_map(|item| self.match_item(item, query))
        {
            let mut parent_path = Vec::new();
            let mut hidden_by_collapse = false;
            for segment in tree_path_for_item(&item.item) {
                parent_path.push(segment);
                let key = path_key(&parent_path);
                let expanded = query_active || !self.collapsed_tree_paths.contains(&key);
                if emitted_groups.insert(key.clone()) && !hidden_by_collapse {
                    rows.push(TreeRow {
                        path: parent_path.clone(),
                        label: parent_path.last().cloned().unwrap_or_default(),
                        depth: parent_path.len() - 1,
                        kind: TreeRowKind::Group,
                        selectable: !query_active,
                        context_only: query_active,
                        expanded,
                        item: None,
                        score: None,
                    });
                }
                if !expanded && !query_active {
                    hidden_by_collapse = true;
                    break;
                }
            }

            if hidden_by_collapse {
                continue;
            }

            let mut leaf_path = tree_path_for_item(&item.item);
            leaf_path.push(item.item.title.clone());
            rows.push(TreeRow {
                path: leaf_path,
                label: item.item.title.clone(),
                depth: item.item.tree_path.len().max(1),
                kind: TreeRowKind::Item,
                selectable: true,
                context_only: false,
                expanded: false,
                item: Some(item.item),
                score: item.score,
            });
        }
        rows
    }

    fn selected_tree_row(&self) -> Option<TreeRow> {
        self.visible_tree_rows()
            .into_iter()
            .filter(|row| row.selectable)
            .nth(self.selected)
    }

    fn selectable_len(&self) -> usize {
        match self.navigation_view {
            NavigationViewMode::List => self.matched.len(),
            NavigationViewMode::Tree => self
                .visible_tree_rows()
                .iter()
                .filter(|row| row.selectable)
                .count(),
        }
    }

    fn best_selectable_index(&self) -> Option<usize> {
        match self.navigation_view {
            NavigationViewMode::List => self
                .matched
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| compare_matches(a, b))
                .map(|(index, _)| index),
            NavigationViewMode::Tree => self
                .visible_tree_rows()
                .into_iter()
                .filter(|row| row.selectable)
                .enumerate()
                .filter_map(|(index, row)| row.item.map(|item| (index, item, row.score)))
                .max_by(|(_, item_a, score_a), (_, item_b, score_b)| {
                    compare_item_rank(item_a, *score_a, item_b, *score_b)
                })
                .map(|(index, _, _)| index),
        }
    }

    fn selected_visible_row_index(&self) -> Option<usize> {
        match self.navigation_view {
            NavigationViewMode::List => (!self.matched.is_empty()).then_some(self.selected),
            NavigationViewMode::Tree => self
                .visible_tree_rows()
                .iter()
                .enumerate()
                .filter(|(_, row)| row.selectable)
                .nth(self.selected)
                .map(|(index, _)| index),
        }
    }

    fn sync_list_selection(&mut self) {
        self.list_state.select(self.selected_visible_row_index());
    }

    fn select_identity(&mut self, identity: &(ItemKind, String, String)) {
        match self.navigation_view {
            NavigationViewMode::List => {
                if let Some(index) = self
                    .matched
                    .iter()
                    .position(|m| &item_identity(&m.item) == identity)
                {
                    self.selected = index;
                }
            }
            NavigationViewMode::Tree => {
                if let Some(index) = self
                    .visible_tree_rows()
                    .iter()
                    .filter(|row| row.selectable)
                    .position(|row| {
                        row.item
                            .as_ref()
                            .is_some_and(|item| &item_identity(item) == identity)
                    })
                {
                    self.selected = index;
                }
            }
        }
    }

    fn select_tree_path(&mut self, path: &[String]) {
        if let Some(index) = self
            .visible_tree_rows()
            .iter()
            .filter(|row| row.selectable)
            .position(|row| row.path == path)
        {
            self.selected = index;
        }
    }
}

fn sort_flat_matches(hits: &mut [MatchedItem]) {
    hits.sort_by(|a, b| compare_matches(a, b).reverse());
}

fn compare_matches(a: &MatchedItem, b: &MatchedItem) -> std::cmp::Ordering {
    compare_item_rank(&a.item, a.score, &b.item, b.score)
}

fn compare_item_rank(
    a: &Item,
    a_score: Option<i64>,
    b: &Item,
    b_score: Option<i64>,
) -> std::cmp::Ordering {
    a_score
        .cmp(&b_score)
        .then_with(|| a.is_dispatchable().cmp(&b.is_dispatchable()))
        .then_with(|| kind_priority(b.kind).cmp(&kind_priority(a.kind)))
        .then_with(|| b.title.cmp(&a.title))
}

fn kind_priority(kind: ItemKind) -> u8 {
    match kind {
        ItemKind::PluginAction => 0,
        ItemKind::JumpWorkspace => 1,
        ItemKind::JumpTab => 2,
        ItemKind::JumpAgent => 3,
        ItemKind::Binding => 4,
        ItemKind::CustomCommand => 5,
    }
}

fn tree_path_for_item(item: &Item) -> Vec<String> {
    if item.tree_path.is_empty() {
        vec![item.kind.category_label().to_string()]
    } else {
        item.tree_path.clone()
    }
}

fn item_identity(item: &Item) -> (ItemKind, String, String) {
    (item.kind, item.title.clone(), item.subtitle.clone())
}

fn path_key(path: &[String]) -> String {
    path.join("\u{1f}")
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
                (KeyCode::Esc, _)
                | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                    return Ok(Outcome::Cancelled);
                }
                (KeyCode::Enter, _) => match state.selected() {
                    Some(item) if item.is_dispatchable() => {
                        return Ok(Outcome::Selected(Box::new(item)));
                    }
                    Some(_) => {
                        continue;
                    }
                    None => return Ok(Outcome::Cancelled),
                },
                (KeyCode::Down, _)
                | (KeyCode::Char('j'), KeyModifiers::CONTROL)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                    state.move_selection(1);
                }
                (KeyCode::Up, _)
                | (KeyCode::Char('k'), KeyModifiers::CONTROL)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                    state.move_selection(-1);
                }
                (KeyCode::Left, _) => state.tree_left(),
                (KeyCode::Right, _) => state.tree_right(),
                (KeyCode::Char('t'), KeyModifiers::CONTROL) => state.toggle_navigation_view(),
                (KeyCode::Char('['), KeyModifiers::CONTROL) => state.collapse_all_tree_nodes(),
                (KeyCode::Char(']'), KeyModifiers::CONTROL) => state.expand_all_tree_nodes(),
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
            Constraint::Min(1),    // list/tree
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
        Span::styled("View ", Style::default().fg(palette.muted)),
        Span::styled(
            state.navigation_view.label(),
            Style::default()
                .fg(palette.accent_2)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("> ", Style::default().fg(palette.accent)),
        Span::styled(state.query.as_str(), Style::default().fg(palette.text)),
        Span::styled("▏", Style::default().fg(palette.muted)),
    ]))
    .style(Style::default().bg(palette.panel));
    f.render_widget(prompt, chunks[0]);

    // List/tree rows.
    let rows = match state.navigation_view {
        NavigationViewMode::List => state
            .matched
            .iter()
            .map(|m| render_row(&m.item, palette))
            .collect::<Vec<_>>(),
        NavigationViewMode::Tree => {
            let tree_rows = state.visible_tree_rows();
            tree_rows
                .iter()
                .map(|row| render_tree_row(row, palette))
                .collect::<Vec<_>>()
        }
    };
    state.sync_list_selection();
    let list = List::new(rows)
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
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        format!(
            "Enter run · Ctrl-t view · ←/→ tree · ↑↓/Ctrl-n-p select · Esc cancel · Ctrl-u clear · {}",
            state.config_path
        ),
        Style::default().fg(palette.muted),
    )]))
    .style(Style::default().bg(palette.panel).fg(palette.muted));
    f.render_widget(footer, chunks[3]);
}

fn render_tree_row(row: &TreeRow, palette: Palette) -> ListItem<'static> {
    let base_style = if row.context_only {
        Style::default()
            .fg(palette.muted)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(palette.text)
    };
    let mut spans = vec![Span::raw("  ".repeat(row.depth))];
    let glyph = match row.kind {
        TreeRowKind::Group if row.expanded => "▾ ",
        TreeRowKind::Group => "▸ ",
        TreeRowKind::Item => "• ",
    };
    spans.push(Span::styled(glyph, base_style));

    match row.item.as_ref() {
        Some(item) => {
            let title_style = if item.is_dispatchable() {
                Style::default().fg(palette.text)
            } else {
                Style::default()
                    .fg(palette.muted)
                    .add_modifier(Modifier::DIM)
            };
            spans.push(Span::styled(format!("{}  ", item.title), title_style));
            if !item.keys.is_empty() {
                spans.push(Span::styled(
                    format!("{}  ", item.keys.join(", ")),
                    Style::default().fg(palette.accent_2),
                ));
            }
            spans.push(Span::styled(
                format!("[{}]", item.kind.category_label()),
                Style::default().fg(accent_for_kind(item.kind, palette)),
            ));
            if row.score.is_some() {
                spans.push(Span::styled("  match", Style::default().fg(palette.muted)));
            }
        }
        None => {
            spans.push(Span::styled(row.label.clone(), base_style));
        }
    }

    ListItem::new(Line::from(spans)).style(base_style)
}

fn render_row(item: &Item, palette: Palette) -> ListItem<'static> {
    let title_style = if item.is_dispatchable() {
        Style::default().fg(palette.text)
    } else {
        // Reference-only: dimmed so users know Enter won't fire it.
        Style::default()
            .fg(palette.muted)
            .add_modifier(Modifier::DIM)
    };
    let kind_style = Style::default().fg(accent_for_kind(item.kind, palette));
    let keys_style = Style::default().fg(palette.muted);

    let mut line = Line::default();
    line.spans
        .push(Span::styled(format!("{}  ", item.title), title_style));
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
    // Reflow the highlight to row 0 for deterministic snapshots when no query
    // chooses a best match.
    state.sync_list_selection();
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
                tree_path: vec!["Panes".into(), "Layout".into()],
                binding: None,
                dispatch: Some(Dispatch::Cli(vec![
                    "herdr".into(),
                    "pane".into(),
                    "split".into(),
                    "--direction".into(),
                    "right".into(),
                ])),
            },
            Item {
                kind: ItemKind::Binding,
                title: "Help".into(),
                subtitle: "Open key map.".into(),
                keys: vec!["prefix+?".into()],
                tree_path: vec!["Session".into(), "Core".into()],
                binding: None,
                dispatch: None,
            },
            Item {
                kind: ItemKind::PluginAction,
                title: "Open pretty which".into(),
                subtitle: "ramarivera.pretty-which.open".into(),
                keys: vec![],
                tree_path: vec!["Plugins".into(), "ramarivera.pretty-which".into()],
                binding: None,
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
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "split");
        s.navigation_view = NavigationViewMode::List;
        assert_eq!(
            s.matched.first().map(|m| m.item.title.as_str()),
            Some("Split vertical")
        );
    }

    #[test]
    fn tree_snapshot_groups_matches_by_mnemonic_path() {
        let p = sample_palette();
        let out = render_snapshot(sample_items(), p, "Palette", "split", 90, 16).unwrap();
        assert!(out.contains("▾ Panes"));
        assert!(out.contains("▾ Layout"));
        assert!(out.contains("• Split vertical"));
    }

    #[test]
    fn flat_sort_prefers_dispatchable_then_kind_priority_on_equal_scores() {
        let mut items = sample_items();
        items.push(Item {
            kind: ItemKind::Binding,
            title: "Open reference".into(),
            subtitle: "Reference-only keybinding".into(),
            keys: vec!["prefix+o".into()],
            tree_path: vec!["Session".into(), "Core".into()],
            binding: None,
            dispatch: None,
        });
        let mut s = PaletteState::new(items, "Palette".into(), "open");
        s.navigation_view = NavigationViewMode::List;
        s.recompute_matches();
        assert_eq!(
            s.matched.first().map(|m| m.item.title.as_str()),
            Some("Open pretty which")
        );
    }

    #[test]
    fn move_selection_wraps() {
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "");
        s.navigation_view = NavigationViewMode::List;
        s.selected = 0;
        s.move_selection(-1); // wrap to last
        assert_eq!(s.selected, 2);
        s.move_selection(1); // wrap back to first
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn snapshot_renders_without_panic() {
        let p = sample_palette();
        let out = render_snapshot(sample_items(), p, "Palette", "", 90, 16).unwrap();
        assert!(out.contains("Palette"));
        assert!(out.contains("Split vertical"));
    }
}
