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
use std::env;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

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
    start_shell: bool,
    cwd: &Path,
) -> Result<Outcome> {
    let mut state = PaletteState::new(items, header.to_string(), initial_query, cwd);
    state.config_path = config_path.to_string();
    if start_shell {
        state.enter_shell_mode();
    }
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
    input_mode: InputMode,
    shell: ShellPanel,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Palette,
    Shell,
}

struct ShellPanel {
    input: String,
    command: Option<String>,
    shell_label: String,
    lines: Vec<ShellLine>,
    rx: Option<Receiver<ShellEvent>>,
    running: bool,
    cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellLine {
    stream: ShellStream,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellStream {
    Stdout,
    Stderr,
    Status,
}

#[derive(Debug)]
enum ShellEvent {
    Line(ShellLine),
    Exit(i32),
    SpawnError(String),
}

impl ShellPanel {
    fn new(cwd: PathBuf) -> Self {
        Self {
            input: String::new(),
            command: None,
            shell_label: login_shell_command_spec().label,
            lines: Vec::new(),
            rx: None,
            running: false,
            cwd,
        }
    }

    fn enter(&mut self) {
        self.input.clear();
    }

    fn clear(&mut self) {
        self.input.clear();
        self.command = None;
        self.lines.clear();
        self.rx = None;
        self.running = false;
    }

    fn start(&mut self) {
        let command = inject_focus_for_herdr_create(self.input.trim());
        if command.is_empty() || self.running {
            return;
        }

        let (tx, rx) = mpsc::channel();
        let shell = login_shell_command_spec();
        self.command = Some(command.clone());
        self.shell_label = shell.label.clone();
        self.lines.clear();
        self.rx = Some(rx);
        self.running = true;
        self.input.clear();

        let cwd = self.cwd.clone();
        thread::spawn(move || {
            let mut child = match Command::new(&shell.program)
                .args(shell.args_for(&command))
                .current_dir(&cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(err) => {
                    let _ = tx.send(ShellEvent::SpawnError(err.to_string()));
                    return;
                }
            };

            if let Some(stdout) = child.stdout.take() {
                let tx = tx.clone();
                thread::spawn(move || {
                    for line in BufReader::new(stdout).lines() {
                        let text = line.unwrap_or_else(|err| format!("read stdout failed: {err}"));
                        if tx
                            .send(ShellEvent::Line(ShellLine {
                                stream: ShellStream::Stdout,
                                text,
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }

            if let Some(stderr) = child.stderr.take() {
                let tx = tx.clone();
                thread::spawn(move || {
                    for line in BufReader::new(stderr).lines() {
                        let text = line.unwrap_or_else(|err| format!("read stderr failed: {err}"));
                        if tx
                            .send(ShellEvent::Line(ShellLine {
                                stream: ShellStream::Stderr,
                                text,
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }

            let code = child
                .wait()
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(1);
            let _ = tx.send(ShellEvent::Exit(code));
        });
    }

    fn closes_on_empty_enter(&self) -> bool {
        !self.running && self.input.trim().is_empty()
    }

    fn drain_events(&mut self) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        let mut keep_rx = true;
        while let Ok(event) = rx.try_recv() {
            match event {
                ShellEvent::Line(line) => self.lines.push(line),
                ShellEvent::Exit(code) => {
                    self.lines.push(ShellLine {
                        stream: ShellStream::Status,
                        text: format!("exit {code}"),
                    });
                    self.running = false;
                    keep_rx = false;
                }
                ShellEvent::SpawnError(message) => {
                    self.lines.push(ShellLine {
                        stream: ShellStream::Status,
                        text: format!("spawn failed: {message}"),
                    });
                    self.running = false;
                    keep_rx = false;
                }
            }
        }
        if keep_rx {
            self.rx = Some(rx);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellCommandSpec {
    program: String,
    label: String,
    style: ShellCommandStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellCommandStyle {
    DashLoginCommand,
    LongLoginCommand,
    PowerShellLoginCommand,
}

impl ShellCommandSpec {
    fn args_for(&self, command: &str) -> Vec<String> {
        match self.style {
            ShellCommandStyle::DashLoginCommand => vec!["-lc".into(), command.into()],
            ShellCommandStyle::LongLoginCommand => {
                vec!["--login".into(), "-c".into(), command.into()]
            }
            ShellCommandStyle::PowerShellLoginCommand => {
                vec!["-Login".into(), "-Command".into(), command.into()]
            }
        }
    }
}

fn login_shell_command_spec() -> ShellCommandSpec {
    shell_command_spec_for(env::var("SHELL").unwrap_or_else(|_| "bash".into()))
}

fn shell_command_spec_for(program: impl Into<String>) -> ShellCommandSpec {
    let program = program.into();
    let label = shell_label(&program);
    let style = match label.as_str() {
        "nu" | "nushell" | "fish" => ShellCommandStyle::LongLoginCommand,
        "pwsh" | "powershell" => ShellCommandStyle::PowerShellLoginCommand,
        _ => ShellCommandStyle::DashLoginCommand,
    };

    ShellCommandSpec {
        program,
        label,
        style,
    }
}

/// When a user types a Herdr creation command in shell mode, automatically
/// append `--focus` so the newly created workspace/tab/pane is focused.
/// This ensures that after the palette closes the user lands inside the
/// thing they just created.
fn inject_focus_for_herdr_create(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    // Avoid rewriting compound shell constructs; only touch simple herdr CLI
    // invocations that the user typed literally.
    if trimmed.contains(|c: char| matches!(c, ';' | '&' | '|' | '>' | '<' | '$' | '\\' | '\n' | '\r')) {
        return trimmed.to_string();
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 3 {
        return trimmed.to_string();
    }

    let is_herdr = tokens[0].ends_with("herdr");
    let kind = tokens[1];
    let sub = tokens[2];

    let is_create = is_herdr
        && matches!(
            (kind, sub),
            ("workspace", "create")
                | ("tab", "create")
                | ("pane", "create")
                | ("pane", "split")
        );

    if !is_create || tokens.iter().any(|t| *t == "--focus") {
        return trimmed.to_string();
    }

    format!("{} --focus", trimmed)
}

fn shell_label(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .trim_start_matches('-')
        .to_string()
}

impl PaletteState {
    fn new(items: Vec<Item>, header: String, initial_query: &str, cwd: &Path) -> Self {
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
            input_mode: InputMode::Palette,
            shell: ShellPanel::new(cwd.to_path_buf()),
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

    fn enter_shell_mode(&mut self) {
        self.input_mode = InputMode::Shell;
        self.shell.enter();
    }

    fn drain_shell_events(&mut self) {
        self.shell.drain_events();
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
        state.drain_shell_events();
        terminal.draw(|f| draw(f, state, palette))?;
        if !event::poll(std::time::Duration::from_millis(250))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if state.input_mode == InputMode::Shell {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                        if !state.shell.running =>
                    {
                        return Ok(Outcome::Cancelled);
                    }
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        return Ok(Outcome::Cancelled);
                    }
                    (KeyCode::Enter, _) if state.shell.closes_on_empty_enter() => {
                        return Ok(Outcome::Cancelled);
                    }
                    (KeyCode::Enter, _) => state.shell.start(),
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => state.shell.clear(),
                    (KeyCode::Backspace, _) if !state.shell.running => {
                        state.shell.input.pop();
                    }
                    (KeyCode::Char(ch), mods)
                        if !state.shell.running && !mods.contains(KeyModifiers::CONTROL) =>
                    {
                        state.shell.input.push(ch);
                    }
                    _ => {}
                }
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
                (KeyCode::Char('!'), _) => state.enter_shell_mode(),
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

    let overlay = overlay_rect(area, state);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
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

    let prompt = match state.input_mode {
        InputMode::Palette => palette_prompt(state, palette),
        InputMode::Shell => shell_prompt(state, palette),
    };
    f.render_widget(prompt, chunks[0]);

    match state.input_mode {
        InputMode::Palette => render_palette_rows(f, chunks[1], state, palette),
        InputMode::Shell => render_shell_output(f, chunks[1], state, palette),
    }

    let footer_text = match state.input_mode {
        InputMode::Palette => format!(
            "Enter run · ! shell · Ctrl-t view · ←/→ tree · ↑↓/Ctrl-n-p select · Esc cancel · Ctrl-u clear · {}",
            state.config_path
        ),
        InputMode::Shell => {
            "Enter run · empty Enter/Esc close · Ctrl-u clear · Ctrl-d cancel".to_string()
        }
    };
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().fg(palette.muted),
    )]))
    .style(Style::default().bg(palette.panel).fg(palette.muted));
    f.render_widget(footer, chunks[2]);
}

fn palette_prompt(state: &PaletteState, palette: Palette) -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled("View ", Style::default().fg(palette.muted)),
        Span::styled(
            state.navigation_view.label(),
            Style::default()
                .fg(palette.accent_2)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("> ", Style::default().fg(palette.accent)),
        Span::styled(state.query.clone(), Style::default().fg(palette.text)),
        Span::styled("▏", Style::default().fg(palette.muted)),
    ]))
    .style(Style::default().bg(palette.panel))
}

fn shell_prompt(state: &PaletteState, palette: Palette) -> Paragraph<'static> {
    let cursor = if state.shell.running { "" } else { "▏" };
    let status = if state.shell.running { " running" } else { "" };
    Paragraph::new(Line::from(vec![
        Span::styled("Shell ", Style::default().fg(palette.warning)),
        Span::styled(
            state.shell.shell_label.clone(),
            Style::default().fg(palette.accent_2),
        ),
        Span::styled(status, Style::default().fg(palette.muted)),
        Span::raw("   "),
        Span::styled("! ", Style::default().fg(palette.warning)),
        Span::styled(state.shell.input.clone(), Style::default().fg(palette.text)),
        Span::styled(cursor, Style::default().fg(palette.muted)),
    ]))
    .style(Style::default().bg(palette.panel))
}

fn render_palette_rows(f: &mut Frame<'_>, area: Rect, state: &mut PaletteState, palette: Palette) {
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
    f.render_stateful_widget(list, area, &mut state.list_state);
}

fn render_shell_output(f: &mut Frame<'_>, area: Rect, state: &PaletteState, palette: Palette) {
    let mut header_rows = Vec::new();
    if let Some(command) = state.shell.command.as_ref() {
        header_rows.push(ListItem::new(Line::from(vec![
            Span::styled("command ", Style::default().fg(palette.muted)),
            Span::styled(command.clone(), Style::default().fg(palette.text)),
        ])));
        header_rows.push(ListItem::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(palette.muted),
        ))));
    } else {
        header_rows.push(ListItem::new(Line::from(Span::styled(
            "Type a shell command and press Enter.",
            Style::default().fg(palette.muted),
        ))));
    }

    let max_rows = area.height as usize;
    let header_len = header_rows.len();
    let output_capacity = max_rows.saturating_sub(header_len);
    let omitted = state.shell.lines.len().saturating_sub(output_capacity);
    let mut rows = header_rows;

    if omitted > 0 && output_capacity > 0 {
        rows.push(ListItem::new(Line::from(Span::styled(
            format!("… {omitted} earlier lines"),
            Style::default().fg(palette.muted),
        ))));
    }

    let output_capacity = max_rows.saturating_sub(rows.len());
    let start = state.shell.lines.len().saturating_sub(output_capacity);
    rows.extend(state.shell.lines.iter().skip(start).map(|line| {
        let style = shell_line_style(line.stream, palette);
        ListItem::new(Line::from(Span::styled(line.text.clone(), style)))
    }));

    let list = List::new(rows).style(Style::default().bg(palette.panel));
    f.render_widget(list, area);
}

fn shell_line_style(stream: ShellStream, palette: Palette) -> Style {
    match stream {
        ShellStream::Stdout => Style::default().fg(palette.text),
        ShellStream::Stderr => Style::default().fg(palette.danger),
        ShellStream::Status => Style::default().fg(palette.muted),
    }
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
fn overlay_rect(area: Rect, state: &PaletteState) -> Rect {
    match state.input_mode {
        InputMode::Palette => centered_rect(area, 80, palette_overlay_height(area)),
        InputMode::Shell => centered_rect(area, 78, shell_overlay_height(area, state)),
    }
}

fn palette_overlay_height(area: Rect) -> u16 {
    ((area.height as f32 * 0.60).round() as u16).clamp(12, area.height.saturating_sub(2).max(1))
}

fn shell_overlay_height(area: Rect, state: &PaletteState) -> u16 {
    let command_rows = if state.shell.command.is_some() { 2 } else { 1 };
    let content_rows = command_rows + state.shell.lines.len() as u16;
    let desired = content_rows + 4; // border, margin, prompt, and footer chrome.
    let min_height = 7;
    let max_height = ((area.height as f32 * 0.52).round() as u16).clamp(min_height, 18);
    desired.clamp(
        min_height,
        max_height.min(area.height.saturating_sub(2).max(1)),
    )
}

fn centered_rect(area: Rect, width_pct: u16, height: u16) -> Rect {
    let height = height.min(area.height);
    let top = area.height.saturating_sub(height) / 2;
    let pop = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area)[1];

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(pop)[1]
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let mut state = PaletteState::new(items, header.to_string(), query, &cwd);
    // Reflow the highlight to row 0 for deterministic snapshots when no query
    // chooses a best match.
    state.sync_list_selection();
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|f| draw(f, &mut state, palette))?;
    let buffer = terminal.backend().buffer().clone();
    Ok(render_buffer(&buffer, width, height))
}

#[cfg(test)]
fn render_shell_snapshot(
    palette: Palette,
    header: &str,
    command: &str,
    lines: Vec<ShellLine>,
    running: bool,
    width: u16,
    height: u16,
) -> Result<String> {
    use ratatui::backend::TestBackend;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let mut state = PaletteState::new(Vec::new(), header.to_string(), "", &cwd);
    state.input_mode = InputMode::Shell;
    state.shell.command = (!command.is_empty()).then(|| command.to_string());
    state.shell.lines = lines;
    state.shell.running = running;
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

    fn sample_cwd() -> std::path::PathBuf {
        std::env::temp_dir()
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
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "", &sample_cwd());
        assert_eq!(s.matched.len(), 3);
        s.query = "split".into();
        s.recompute_matches();
        assert!(s.matched.len() <= 3);
        assert!(s.matched.iter().any(|m| m.item.title.contains("Split")));
    }

    #[test]
    fn fuzzy_ranks_split_above_help() {
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "split", &sample_cwd());
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
        let mut s = PaletteState::new(items, "Palette".into(), "open", &sample_cwd());
        s.navigation_view = NavigationViewMode::List;
        s.recompute_matches();
        assert_eq!(
            s.matched.first().map(|m| m.item.title.as_str()),
            Some("Open pretty which")
        );
    }

    #[test]
    fn move_selection_wraps() {
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "", &sample_cwd());
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

    #[test]
    fn shell_mode_can_be_entered() {
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "", &sample_cwd());
        assert_eq!(s.input_mode, InputMode::Palette);

        s.enter_shell_mode();
        assert_eq!(s.input_mode, InputMode::Shell);
    }

    #[test]
    fn shell_empty_enter_closes_only_when_idle() {
        let mut shell = ShellPanel::new(sample_cwd());

        assert!(shell.closes_on_empty_enter());

        shell.input = "echo ok".into();
        assert!(!shell.closes_on_empty_enter());

        shell.input.clear();
        shell.running = true;
        assert!(!shell.closes_on_empty_enter());

        shell.running = false;
        shell.command = Some("echo ok".into());
        assert!(shell.closes_on_empty_enter());
    }

    #[test]
    fn shell_clear_removes_input_command_and_output() {
        let mut shell = ShellPanel::new(sample_cwd());
        shell.input = "echo nope".into();
        shell.command = Some("echo old".into());
        shell.lines.push(ShellLine {
            stream: ShellStream::Stdout,
            text: "old".into(),
        });

        shell.clear();

        assert!(shell.input.is_empty());
        assert_eq!(shell.command, None);
        assert!(shell.lines.is_empty());
        assert!(!shell.running);
    }

    #[test]
    fn shell_snapshot_renders_command_separator_output_and_status() {
        let p = sample_palette();
        let out = render_shell_snapshot(
            p,
            "Palette",
            "printf hello",
            vec![
                ShellLine {
                    stream: ShellStream::Stdout,
                    text: "hello".into(),
                },
                ShellLine {
                    stream: ShellStream::Status,
                    text: "exit 0".into(),
                },
            ],
            false,
            90,
            20,
        )
        .unwrap();

        assert!(out.contains("Shell "));
        assert!(out.contains("command printf hello"));
        assert!(out.contains("────"));
        assert!(out.contains("hello"));
        assert!(out.contains("exit 0"));
    }

    #[test]
    fn shell_overlay_starts_compact_and_grows_with_output() {
        let area = Rect::new(0, 0, 100, 40);
        let mut s = PaletteState::new(sample_items(), "Palette".into(), "", &sample_cwd());
        s.enter_shell_mode();

        assert_eq!(shell_overlay_height(area, &s), 7);

        s.shell.command = Some("seq 20".into());
        s.shell.lines = (1..=10)
            .map(|n| ShellLine {
                stream: ShellStream::Stdout,
                text: n.to_string(),
            })
            .collect();

        assert_eq!(shell_overlay_height(area, &s), 16);

        s.shell.lines = (1..=50)
            .map(|n| ShellLine {
                stream: ShellStream::Stdout,
                text: n.to_string(),
            })
            .collect();

        assert_eq!(shell_overlay_height(area, &s), 18);
    }

    #[test]
    fn shell_snapshot_marks_omitted_output_when_capped() {
        let p = sample_palette();
        let lines = (1..=30)
            .map(|n| ShellLine {
                stream: ShellStream::Stdout,
                text: format!("line {n}"),
            })
            .collect();
        let out = render_shell_snapshot(p, "Palette", "seq 30", lines, false, 90, 20).unwrap();

        assert!(out.contains("…"));
        assert!(out.contains("earlier lines"));
        assert!(out.contains("line 30"));
    }

    #[test]
    fn shell_command_spec_uses_nushell_login_command_mode() {
        let spec = shell_command_spec_for("/opt/homebrew/bin/nu");

        assert_eq!(spec.program, "/opt/homebrew/bin/nu");
        assert_eq!(spec.label, "nu");
        assert_eq!(
            spec.args_for("pwd"),
            vec!["--login".to_string(), "-c".to_string(), "pwd".to_string()]
        );
    }

    #[test]
    fn shell_command_spec_uses_dash_lc_for_posix_shells() {
        let spec = shell_command_spec_for("/bin/zsh");

        assert_eq!(spec.label, "zsh");
        assert_eq!(
            spec.args_for("pwd"),
            vec!["-lc".to_string(), "pwd".to_string()]
        );
    }

    #[test]
    #[cfg(unix)]
    fn shell_command_runs_in_panel_cwd() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = std::env::temp_dir().join("herdr-palette-shell-cwd-test");
        std::fs::create_dir_all(&tmp).unwrap();
        let marker = tmp.join("cwd_marker.txt");
        let script = tmp.join("fake_shell.sh");
        let body = format!("#!/bin/sh\npwd > {}\n", marker.display());
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        std::env::set_var("SHELL", &script);
        let mut shell = ShellPanel::new(tmp.clone());
        shell.input = "ignored".to_string();
        shell.start();

        // Wait for the fake shell to finish writing its cwd marker.
        for _ in 0..50 {
            shell.drain_events();
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let recorded = std::fs::read_to_string(&marker).unwrap_or_default();
        let expected = tmp.canonicalize().unwrap_or(tmp.clone());
        assert_eq!(recorded.trim(), expected.display().to_string());

        std::env::remove_var("SHELL");
    }

    #[test]
    fn inject_focus_adds_flag_to_herdr_creates() {
        assert_eq!(
            inject_focus_for_herdr_create("herdr workspace create Foo"),
            "herdr workspace create Foo --focus"
        );
        assert_eq!(
            inject_focus_for_herdr_create("herdr tab create Bar"),
            "herdr tab create Bar --focus"
        );
        assert_eq!(
            inject_focus_for_herdr_create("herdr pane split --direction right"),
            "herdr pane split --direction right --focus"
        );
        assert_eq!(
            inject_focus_for_herdr_create("herdr pane create"),
            "herdr pane create --focus"
        );
    }

    #[test]
    fn inject_focus_respects_existing_flag() {
        assert_eq!(
            inject_focus_for_herdr_create("herdr workspace create Foo --focus"),
            "herdr workspace create Foo --focus"
        );
        assert_eq!(
            inject_focus_for_herdr_create("herdr tab create --focus Bar"),
            "herdr tab create --focus Bar"
        );
    }

    #[test]
    fn inject_focus_skips_non_herdr_and_compound_commands() {
        assert_eq!(
            inject_focus_for_herdr_create("ls -la"),
            "ls -la"
        );
        assert_eq!(
            inject_focus_for_herdr_create("herdr workspace create Foo; echo done"),
            "herdr workspace create Foo; echo done"
        );
        assert_eq!(
            inject_focus_for_herdr_create("herdr workspace create Foo && herdr tab create Bar"),
            "herdr workspace create Foo && herdr tab create Bar"
        );
        assert_eq!(
            inject_focus_for_herdr_create("cd /tmp && herdr workspace create Foo"),
            "cd /tmp && herdr workspace create Foo"
        );
    }

    #[test]
    fn inject_focus_preserves_whitespace_and_quotes() {
        assert_eq!(
            inject_focus_for_herdr_create("  herdr workspace create \"Foo Bar\"  "),
            "herdr workspace create \"Foo Bar\" --focus"
        );
    }
}
