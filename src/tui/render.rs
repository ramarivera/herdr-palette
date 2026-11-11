use super::*;

pub(super) fn run_loop(state: &mut PaletteState, palette: Palette) -> Result<Outcome> {
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

pub(super) fn draw(f: &mut Frame<'_>, state: &mut PaletteState, palette: Palette) {
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

pub(super) fn shell_overlay_height(area: Rect, state: &PaletteState) -> u16 {
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
pub(super) fn render_shell_snapshot(
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

pub(super) fn render_buffer(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
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
