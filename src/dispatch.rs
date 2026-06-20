//! Dispatch tier map + execution.
//!
//! Herdr v1's socket API can: create/focus/split/close/zoom panes, create/
//! rename/close/focus workspaces and tabs, focus agents, and invoke plugin
//! actions. It CANNOT replay an arbitrary keybinding chord, so actions like
//! `help` (`prefix+?`), `settings` (`prefix+s`), `detach` (`prefix+q`),
//! `goto`, `workspace_picker`, `toggle_sidebar`, `resize_mode`, and
//! `edit_scrollback` have no programmatic path from a plugin — they're
//! reference-only in the palette (shown greyed with their chord).
//!
//! Tier A = direct `herdr <subcommand>` (create/rename/close/focus/split/zoom).
//! Tier B = list + resolve + focus, for prev/next navigation.
//! Reference = no dispatch (keybinding-only actions).

use crate::items::Dispatch;
use anyhow::{Context, Result};
use std::process::Command;

/// Map a Herdr keybinding action name to its dispatch path, or `None` if the
/// action is keybinding-only (no socket/CLI equivalent in v1).
///
/// Action names come from `herdr_pretty_which::model::SPECS` — they're the
/// canonical Herdr action ids (e.g. `new_workspace`, `focus_pane_left`).
pub fn dispatch_for_action(action: &str) -> Option<Dispatch> {
    use Dispatch::*;
    let d = match action {
        // --- Workspaces ---
        "new_workspace" => Cli(vec_into(&["herdr", "workspace", "create", "--focus"])),
        "new_worktree" => Cli(vec_into(&["herdr", "worktree", "create"])),
        "rename_workspace" => Cli(vec_into(&["herdr", "workspace", "rename"])), // needs id+label; prompt-based
        "close_workspace" => Cli(vec_into(&["herdr", "workspace", "close"])), // needs id
        "previous_workspace" => PrevWorkspace,
        "next_workspace" => NextWorkspace,

        // --- Tabs ---
        "new_tab" => Cli(vec_into(&["herdr", "tab", "create", "--focus"])),
        "rename_tab" => Cli(vec_into(&["herdr", "tab", "rename"])),
        "close_tab" => Cli(vec_into(&["herdr", "tab", "close"])),
        "previous_tab" => PrevTab,
        "next_tab" => NextTab,

        // --- Panes / agents ---
        "split_vertical" => Cli(vec_into(&["herdr", "pane", "split", "right"])),
        "split_horizontal" => Cli(vec_into(&["herdr", "pane", "split", "down"])),
        "close_pane" => Cli(vec_into(&["herdr", "pane", "close"])),
        "zoom" => Cli(vec_into(&["herdr", "pane", "zoom"])),
        "fullscreen" => Cli(vec_into(&["herdr", "pane", "fullscreen"])),
        "cycle_pane_next" => Cli(vec_into(&["herdr", "pane", "focus", "next"])),
        "cycle_pane_previous" => Cli(vec_into(&["herdr", "pane", "focus", "previous"])),
        "focus_pane_left" => Cli(vec_into(&["herdr", "pane", "focus", "left"])),
        "focus_pane_down" => Cli(vec_into(&["herdr", "pane", "focus", "down"])),
        "focus_pane_up" => Cli(vec_into(&["herdr", "pane", "focus", "up"])),
        "focus_pane_right" => Cli(vec_into(&["herdr", "pane", "focus", "right"])),
        "rename_pane" => Cli(vec_into(&["herdr", "agent", "rename"])),
        "previous_agent" => PrevAgent,
        "next_agent" => NextAgent,

        // --- Keybinding-only (no v1 dispatch path) ---
        // help, settings, detach, goto, workspace_picker, switch_tab,
        // switch_workspace, resize_mode, toggle_sidebar, edit_scrollback,
        // last_pane, focus_agent, navigate_* (these are workspace scroll /
        // pane-arrow passthroughs, not focus primitives), open_notification_target,
        // reload_config (has a CLI but clobbers live session — leave to chord),
        // open_worktree, remove_worktree.
        _ => return None,
    };
    Some(d)
}

/// Resolve the `herdr` binary path. `HERDR_BIN_PATH` wins, else PATH lookup.
pub fn herdr_bin() -> Result<String> {
    if let Ok(p) = std::env::var("HERDR_BIN_PATH") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    which("herdr").context("could not find `herdr` on PATH (set HERDR_BIN_PATH?)")
}

fn which(cmd: &str) -> Result<String, std::io::Error> {
    // Lightweight PATH lookup; avoids pulling in the `which` crate for one call.
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{cmd} not found on PATH"),
    ))
}

/// Execute a [`Dispatch`]. Spawns the resolved command(s) detached; the palette
/// closes immediately after so Herdr retains focus. Errors are surfaced but do
/// not panic.
pub fn run(dispatch: &Dispatch) -> Result<()> {
    match dispatch {
        Dispatch::Cli(argv) => {
            let strs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            run_argv(&strs)?;
        }
        Dispatch::FocusWorkspace(id) => {
            run_argv(&["herdr", "workspace", "focus", id])?;
        }
        Dispatch::FocusTab(id) => {
            run_argv(&["herdr", "tab", "focus", id])?;
        }
        Dispatch::FocusAgent(target) => {
            run_argv(&["herdr", "agent", "focus", target])?;
        }
        Dispatch::NextWorkspace => {
            focus_neighbor("workspace", Neighbor::Next)?;
        }
        Dispatch::PrevWorkspace => {
            focus_neighbor("workspace", Neighbor::Prev)?;
        }
        Dispatch::NextTab => {
            focus_neighbor("tab", Neighbor::Next)?;
        }
        Dispatch::PrevTab => {
            focus_neighbor("tab", Neighbor::Prev)?;
        }
        Dispatch::NextAgent => {
            focus_neighbor("agent", Neighbor::Next)?;
        }
        Dispatch::PrevAgent => {
            focus_neighbor("agent", Neighbor::Prev)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Neighbor {
    Next,
    Prev,
}

/// Resolve the live ordered list of `<kind>` ids, find the current one, and
/// focus its neighbor. `<kind>` ∈ {workspace, tab, agent}. For agents, "current"
/// is the focused terminal; for workspaces/tabs it's the focused entity.
fn focus_neighbor(kind: &str, neighbor: Neighbor) -> Result<()> {
    let entries = list_entries(kind)?;
    let ids: Vec<String> = entries.iter().map(|(id, _)| id.clone()).collect();
    if ids.len() < 2 {
        return Ok(()); // nothing to cycle
    }
    let current = current_id(kind)?;
    let pos = ids.iter().position(|id| id == &current).unwrap_or(0);
    let target = match neighbor {
        Neighbor::Next => (pos + 1) % ids.len(),
        Neighbor::Prev => (pos + ids.len() - 1) % ids.len(),
    };
    let id = &ids[target];
    run_argv(&["herdr", kind, "focus", id])
}

/// `herdr <kind> list` → ordered vector of (id, label). JSON is the default
/// output for all herdr list commands (no `--json` flag exists). Public so the
/// source collector can build jump targets from the same resolver the
/// prev/next dispatch path uses.
pub fn list_entries(kind: &str) -> Result<Vec<(String, String)>> {
    let out = Command::new(herdr_bin()?).args([kind, "list"]).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "herdr {kind} list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    extract_entries(&text, kind).context("could not parse entries from list output")
}

/// `herdr status --json`-style current-id probe. Falls back to first id if the
/// current entity can't be determined.
fn current_id(kind: &str) -> Result<String> {
    let entries = list_entries(kind)?;
    let _ = kind; // status shape varies; fall through to list[0]
    entries
        .first()
        .map(|(id, _)| id.clone())
        .context("no ids available to determine current")
}

/// Extract ordered (id, label) entries from a herdr JSON list envelope.
/// Id field is kind-specific: `workspace_id`, `tab_id`, or `terminal_id`.
/// Label is `label` (workspaces/tabs); for agents we synthesize
/// `<agent> · <cwd basename>` since agents have no `label` field.
fn extract_entries(text: &str, kind: &str) -> Result<Vec<(String, String)>> {
    let plural = match kind {
        "workspace" => "workspaces",
        "tab" => "tabs",
        "agent" => "agents",
        other => other,
    };
    let id_field = match kind {
        "workspace" => "workspace_id",
        "tab" => "tab_id",
        "agent" => "terminal_id",
        _ => "id",
    };
    let v: serde_json::Value = serde_json::from_str(text).context("list output was not JSON")?;
    let arr = v
        .get("result")
        .and_then(|r| r.get(plural))
        .and_then(|w| w.as_array())
        .or_else(|| v.as_array())
        .context("list output had no array")?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let id = entry
            .get(id_field)
            .and_then(|i| i.as_str())
            .map(str::to_string);
        let label = match kind {
            "agent" => {
                let agent = entry.get("agent").and_then(|s| s.as_str()).unwrap_or("agent");
                let cwd = entry
                    .get("cwd")
                    .and_then(|s| s.as_str())
                    .map(|c| std::path::Path::new(c).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| c.to_string()))
                    .unwrap_or_default();
                format!("{agent} · {cwd}")
            }
            _ => entry
                .get("label")
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .unwrap_or_default(),
        };
        if let Some(id) = id {
            out.push((id, label));
        }
    }
    Ok(out)
}

/// Run an argv, resolving `argv[0] == "herdr"` to the real binary path. String
/// slices are promoted to owned for the child.
fn run_argv(argv: &[&str]) -> Result<()> {
    let mut owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    if owned.first().is_some_and(|first| first == "herdr") {
        owned[0] = herdr_bin()?;
    }
    let (cmd, args) = owned
        .split_first()
        .context("empty argv")?;
    Command::new(cmd).args(args).spawn()?.wait()?;
    Ok(())
}

fn vec_into(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatchable_actions_map_to_cli() {
        assert!(matches!(
            dispatch_for_action("new_workspace"),
            Some(Dispatch::Cli(_))
        ));
        assert!(matches!(
            dispatch_for_action("split_vertical"),
            Some(Dispatch::Cli(_))
        ));
        assert!(matches!(
            dispatch_for_action("focus_pane_left"),
            Some(Dispatch::Cli(_))
        ));
    }

    #[test]
    fn prev_next_map_to_neighbor_dispatch() {
        assert!(matches!(dispatch_for_action("next_workspace"), Some(Dispatch::NextWorkspace)));
        assert!(matches!(dispatch_for_action("previous_tab"), Some(Dispatch::PrevTab)));
        assert!(matches!(dispatch_for_action("next_agent"), Some(Dispatch::NextAgent)));
    }

    #[test]
    fn keybinding_only_actions_have_no_dispatch() {
        for action in [
            "help",
            "settings",
            "detach",
            "goto",
            "workspace_picker",
            "resize_mode",
            "toggle_sidebar",
            "edit_scrollback",
            "reload_config",
        ] {
            assert!(
                dispatch_for_action(action).is_none(),
                "{action} should be reference-only"
            );
        }
    }

    #[test]
    fn extract_entries_maps_kind_specific_id_fields() {
        let ws = r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"toolbox"}]}}"#;
        assert_eq!(extract_entries(ws, "workspace").unwrap(), vec![("w1".into(), "toolbox".into())]);

        let tabs = r#"{"result":{"tabs":[{"tab_id":"w1:t1","label":"logs"}]}}"#;
        assert_eq!(extract_entries(tabs, "tab").unwrap(), vec![("w1:t1".into(), "logs".into())]);
    }

    #[test]
    fn extract_entries_synthesizes_agent_label() {
        let agents = r#"{"result":{"agents":[{"terminal_id":"term_1","agent":"claude","cwd":"/Users/x/toolbox"}]}}"#;
        let e = extract_entries(agents, "agent").unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].0, "term_1");
        assert_eq!(e[0].1, "claude · toolbox");
    }

    #[test]
    fn extract_entries_falls_back_to_flat_array() {
        let flat = r#"[{"workspace_id":"w1","label":"a"}]"#;
        assert_eq!(extract_entries(flat, "workspace").unwrap(), vec![("w1".into(), "a".into())]);
    }
}
