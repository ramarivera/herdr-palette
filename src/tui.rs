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
    PosixDash,
    LongFlag,
    PowerShell,
}

impl ShellCommandSpec {
    fn args_for(&self, command: &str) -> Vec<String> {
        match self.style {
            ShellCommandStyle::PosixDash => vec!["-lc".into(), command.into()],
            ShellCommandStyle::LongFlag => vec!["--login".into(), "-c".into(), command.into()],
            ShellCommandStyle::PowerShell => {
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
        "nu" | "nushell" | "fish" => ShellCommandStyle::LongFlag,
        "pwsh" | "powershell" => ShellCommandStyle::PowerShell,
        _ => ShellCommandStyle::PosixDash,
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
    if trimmed.contains([';', '&', '|', '>', '<', '$', '\\', '\n', '\r']) {
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
            ("workspace", "create") | ("tab", "create") | ("pane", "create") | ("pane", "split")
        );

    if !is_create || tokens.contains(&"--focus") {
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

        *self.list_state.offset_mut() = 0;

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

mod render;
pub use render::render_snapshot;
use render::run_loop;

#[cfg(test)]
use render::{draw, render_buffer, render_shell_snapshot, shell_overlay_height};
#[cfg(test)]
mod tests;
