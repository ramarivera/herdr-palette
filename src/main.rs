//! herdr-palette — a Raycast/Linear-style fuzzy command palette for Herdr.
//!
//! Reuses `herdr-pretty-which` as a path dep for config loading, action
//! modeling, discovery, and theming (so the palette inherits the user's real
//! Herdr theme). Adds: plugin-action discovery, live jump targets, fuzzy
//! filtering, and an overlay TUI.
//!
//! Two modes:
//!   - Interactive (default, when stdout is a TTY): opens the overlay.
//!   - Snapshot (`--snapshot` or non-TTY stdout): renders once to stdout for
//!     tests/screenshots.
//!
//! Dispatch on Enter runs the resolved `herdr` subcommand (Tier A) or the
//! plugin action's real `command[]`, or resolves list+focus for prev/next.
//! Reference-only rows (keybinding actions with no v1 dispatch path) render
//! greyed and ignore Enter.

pub mod dispatch;
pub mod items;
pub mod source;
pub mod tui;

use anyhow::Result;
use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Read a specific Herdr config path instead of HERDR_CONFIG_PATH or
    /// ~/.config/herdr/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Render once to stdout for tests/snapshots instead of opening the TUI.
    #[arg(long)]
    snapshot: bool,

    /// Initial fuzzy query for snapshot/tests or interactive mode.
    #[arg(long, default_value = "")]
    query: String,

    /// Start the interactive overlay directly in shell mode.
    #[arg(long)]
    shell: bool,

    /// Snapshot width.
    #[arg(long, default_value_t = 100)]
    width: u16,

    /// Snapshot height.
    #[arg(long, default_value_t = 24)]
    height: u16,

    /// Print a count of collected items grouped by kind, then exit. Used to
    /// diagnose which sources are/aren't contributing rows.
    #[arg(long)]
    debug_kinds: bool,

    /// Print every occupied key chord and the action it's bound to (across all
    /// sources), then exit. The authoritative occupied set — use it to find a
    /// free chord instead of guessing from `herdr --default-config`, which is
    /// an incomplete reference.
    #[arg(long)]
    debug_keys: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let data = source::collect(args.config)?;

    if args.debug_keys {
        use std::collections::BTreeMap;
        let mut by_key: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for it in &data.items {
            if it.keys.is_empty() {
                continue;
            }
            for k in &it.keys {
                by_key.entry(k.clone()).or_default().push(it.title.as_str());
            }
        }
        println!("occupied chords ({}):", by_key.len());
        for (k, titles) in &by_key {
            println!("  {k:<22} → {}", titles.join(", "));
        }
        return Ok(());
    }

    if args.debug_kinds {
        use std::collections::BTreeMap;
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for it in &data.items {
            *counts.entry(it.kind.category_label()).or_default() += 1;
        }
        println!("collected {} items:", data.items.len());
        for (k, v) in &counts {
            println!("  {k}: {v}");
        }
        return Ok(());
    }

    let header = format!("Herdr Palette · {}", data.theme_name);

    if args.snapshot || !std::io::stdout().is_terminal() {
        let out = tui::render_snapshot(
            data.items,
            data.palette,
            &header,
            &args.query,
            args.width,
            args.height,
        )?;
        print!("{out}");
        return Ok(());
    }

    match tui::run(
        data.items,
        data.palette,
        &header,
        &args.query,
        &data.config_path,
        args.shell,
        &data.cwd,
    )? {
        tui::Outcome::Selected(item) => {
            if let Some(d) = item.dispatch.as_ref() {
                dispatch::run(d, &data.cwd)?;
            }
        }
        tui::Outcome::Cancelled => {}
    }
    Ok(())
}
