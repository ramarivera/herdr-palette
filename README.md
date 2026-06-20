# herdr-palette

A Raycast/Linear-style fuzzy command palette for [Herdr](https://github.com/ramarivera/herdr) — the terminal workspace manager for AI coding agents.

`prefix+space` (or your configured entrypoint) opens a centered overlay that fuses **every Herdr surface into one fuzzyable list**: keybindings, plugin actions, workspaces, tabs, and agents. Type, select, Enter, done.

```
┌ Herdr Palette · catppuccin ──────────────────────────────────────────────────┐
│> split▏                                                                       │
│▶ Split vertical   Split side by side.        [Keybinding]   prefix+v, prefix+|│
│  Split horizontal Split top/bottom.          [Keybinding]   prefix+minus      │
│  Open pretty which  ramarivera.pretty-which.open             [Plugin]          │
│  toolbox  w5                                                 [Workspace]       │
│  claude · toolbox  term_6549f64f8679115                      [Agent]           │
└──────────────────────────────────────────────────────────────────────────────┘
```

## What it does

The palette is a **read layer over the live Herdr server**. It collects:

| Source | How | Count (example) |
|--------|-----|-----------------|
| Keybindings | reuses `herdr-pretty-which`'s config-aware binding model | 49 |
| Plugin actions | `herdr plugin action list` (runs the action's real `command[]`) | 2 |
| Custom commands | your `[[keys.command]]` entries | 3 |
| Workspaces | `herdr workspace list` → `workspace focus <id>` | 3 |
| Tabs | `herdr tab list` → `tab focus <id>` | 4 |
| Agents | `herdr agent list` → `agent focus <terminal_id>` | 4 |

Then fuzzy-filters them with skim-style ranking (title-biased) and, on Enter, dispatches.

### Dispatch tiers

- **Tier A — direct CLI:** create/focus/split/close/zoom panes, workspaces, tabs (`herdr workspace focus w5`, etc.) and **plugin actions** (each ships a fully-resolved `command[]` we run verbatim).
- **Tier B — list + resolve + focus:** `previous/next` workspace/tab/agent cycles (reads the live ordered list, finds current, focuses the neighbor).
- **Reference-only (greyed):** `help`, `settings`, `detach`, `goto`, `workspace_picker`, `resize_mode`, `toggle_sidebar`, `edit_scrollback`, `reload_config`. These are keybinding-only — Herdr v1 exposes no programmatic path to trigger them from a plugin, so the palette shows them with their chord but Enter does nothing. (This is a Herdr socket-API limitation, not a palette limitation.)

## Build

```bash
cargo build --release
```

Requires the `herdr-pretty-which` crate as a path sibling (at `../herdr-pretty-which`), which provides config loading, action modeling, discovery, and theming.

## Run

```bash
# Interactive overlay (when stdout is a TTY)
./target/release/herdr-palette

# Snapshot to stdout (for tests/screenshots/non-TTY)
./target/release/herdr-palette --snapshot --query "split" --width 100 --height 30

# Diagnose which sources are contributing items
./target/release/herdr-palette --debug-kinds
```

| Flag | Default | Purpose |
|------|---------|---------|
| `--config <PATH>` | `~/.config/herdr/config.toml` | Herdr config to reflect |
| `--query <Q>` | `""` | Initial fuzzy query |
| `--snapshot` | off | Render once to stdout instead of opening the TUI |
| `--width` / `--height` | `100` / `24` | Snapshot dimensions |
| `--debug-kinds` | off | Print item counts by source, then exit |

## Keybindings (inside the palette)

| Key | Action |
|-----|--------|
| `Enter` | Dispatch the selected row (greyed rows are reference-only) |
| `↑` `↓` or `Ctrl-n` / `Ctrl-p` | Move selection |
| `Esc` / `Ctrl-c` / `Ctrl-d` | Cancel |
| `Ctrl-u` | Clear query |
| typing | Append to query (fuzzy re-ranks live) |

## Theming

Inherits your configured Herdr theme via `Palette::from_theme` — catppuccin, tokyo-night, dracula, nord, gruvbox, one-dark, solarized, kanagawa, and more, plus `[[theme.custom]]` overrides. The palette renders with the exact same colors `herdr-pretty-which` uses, so it matches the rest of your Herdr UI.

## Herdr plugin manifest

`herdr-plugin.toml` declares the plugin so Herdr can discover and launch it:

```toml
id = "ramarivera.palette"
[[actions]]
id = "open"
title = "Open palette"
contexts = ["workspace", "tab", "pane", "global"]
command = ["herdr", "plugin", "pane", "open", "--plugin", "ramarivera.palette", "--entrypoint", "overlay", "--placement", "overlay", "--focus"]
```

Bind it to a chord in your Herdr config:

```toml
[keys]
# e.g. open the palette with prefix+space
workspace_picker = "prefix+space"
```

## Architecture

```
src/
├── main.rs      # CLI args, mode dispatch (interactive / snapshot / debug)
├── source.rs    # collect(): fuses all sources into Vec<Item> + resolves Palette
├── items.rs     # Item model + constructors (binding/command/plugin/jump)
├── dispatch.rs  # action→Dispatch tier map + runner (Cli/Focus/Next/Prev)
└── tui.rs       # ratatui overlay: fuzzy filter, render, input loop, snapshot
```

The terminal restore path is guarded by drop-guards (`RawModeGuard`, `AltScreenGuard`) so a failure mid-loop never leaves your terminal in raw mode.

## License

MIT
