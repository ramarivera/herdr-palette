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
fn incremental_search_resets_scroll_offset() {
    use ratatui::backend::TestBackend;

    let mut items = Vec::new();
    for i in 0..120 {
        items.push(Item {
            kind: ItemKind::Binding,
            title: format!("Action {i}"),
            subtitle: format!("Does thing {i}."),
            keys: vec![format!("prefix+{}", (b'a' + (i % 26) as u8) as char)],
            tree_path: vec![format!("Category{}", i / 10), format!("Sub{}", i / 5)],
            binding: None,
            dispatch: Some(Dispatch::Cli(vec!["herdr".into()])),
        });
    }
    items.push(Item {
        kind: ItemKind::PluginAction,
        title: "Toggle sidebar".into(),
        subtitle: "herdr-sidebar.open-sidebar".into(),
        keys: vec![],
        tree_path: vec!["Plugins".into(), "herdr-sidebar".into()],
        binding: None,
        dispatch: Some(Dispatch::Cli(vec!["bash".into()])),
    });
    items.push(Item {
        kind: ItemKind::PluginAction,
        title: "Redeploy sidebar panes".into(),
        subtitle: "herdr-sidebar.redeploy".into(),
        keys: vec![],
        tree_path: vec!["Plugins".into(), "herdr-sidebar".into()],
        binding: None,
        dispatch: Some(Dispatch::Cli(vec!["bash".into()])),
    });

    let p = sample_palette();
    let mut state = PaletteState::new(items, "Palette".into(), "", &sample_cwd());
    state.navigation_view = NavigationViewMode::Tree;

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| draw(f, &mut state, p)).unwrap();

    for ch in "sidebar".chars() {
        state.query.push(ch);
        state.recompute_matches();
        terminal.draw(|f| draw(f, &mut state, p)).unwrap();
    }

    let buffer = terminal.backend().buffer().clone();
    let rendered = render_buffer(&buffer, 100, 30);
    assert!(
        rendered.contains("Toggle sidebar"),
        "stale scroll offset hid items above the selected row"
    );
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
    assert_eq!(inject_focus_for_herdr_create("ls -la"), "ls -la");
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

#[test]
fn tui_modules_stay_within_file_size_limit() {
    for (name, source) in [
        ("tui.rs", include_str!("../tui.rs")),
        ("tui/render.rs", include_str!("render.rs")),
        ("tui/tests.rs", include_str!("tests.rs")),
    ] {
        assert!(source.lines().count() <= 800, "{name} exceeds 800 lines");
    }
}
