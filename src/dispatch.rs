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
use std::path::{Path, PathBuf};
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
        "new_worktree" => Cli(vec_into(&["herdr", "worktree", "create", "--focus"])),
        "previous_workspace" => PrevWorkspace,
        "next_workspace" => NextWorkspace,

        // --- Tabs ---
        "new_tab" => Cli(vec_into(&["herdr", "tab", "create", "--focus"])),
        "previous_tab" => PrevTab,
        "next_tab" => NextTab,

        // --- Panes / agents ---
        "split_vertical" => Cli(vec_into(&[
            "herdr",
            "pane",
            "split",
            "--direction",
            "right",
            "--focus",
        ])),
        "split_horizontal" => Cli(vec_into(&[
            "herdr",
            "pane",
            "split",
            "--direction",
            "down",
            "--focus",
        ])),
        "zoom" | "fullscreen" => Cli(vec_into(&[
            "herdr",
            "pane",
            "zoom",
            "--current",
            "--toggle",
        ])),
        "focus_pane_left" => Cli(vec_into(&["herdr", "pane", "focus", "--direction", "left"])),
        "focus_pane_down" => Cli(vec_into(&["herdr", "pane", "focus", "--direction", "down"])),
        "focus_pane_up" => Cli(vec_into(&["herdr", "pane", "focus", "--direction", "up"])),
        "focus_pane_right" => Cli(vec_into(&[
            "herdr",
            "pane",
            "focus",
            "--direction",
            "right",
        ])),
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

/// Query Herdr for the cwd of the current caller pane. This is the directory
/// the palette should use as the working directory for shell commands and for
/// newly created panes/tabs/workspaces/plugin panes.
///
/// Herdr's global focused pane can move while a plugin is opening; prefer the
/// process/caller-aware `pane current` result, then fall back to scanning the
/// focused pane list for older or degraded Herdr contexts.
pub fn focused_pane_cwd() -> Option<PathBuf> {
    current_pane_cwd().or_else(focused_pane_cwd_from_list)
}

fn current_pane_cwd() -> Option<PathBuf> {
    let output = Command::new(herdr_bin().ok()?)
        .args(["pane", "current"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    extract_current_pane_cwd(&text)
}

fn extract_current_pane_cwd(text: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("result")
        .and_then(|r| r.get("pane"))
        .or_else(|| v.get("pane"))
        .and_then(cwd_from_pane_entry)
}

fn focused_pane_cwd_from_list() -> Option<PathBuf> {
    let output = Command::new(herdr_bin().ok()?)
        .args(["pane", "list"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let arr = list_array_for_kind(&v, "pane")?;
    arr.iter()
        .find(|entry| entry.get("focused").and_then(|f| f.as_bool()) == Some(true))
        .and_then(cwd_from_pane_entry)
}

fn cwd_from_pane_entry(entry: &serde_json::Value) -> Option<PathBuf> {
    ["foreground_cwd", "cwd"].iter().find_map(|key| {
        entry
            .get(*key)
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
    })
}

/// For surface-creating `herdr` subcommands, inject an explicit `--cwd` flag
/// so the new pane/workspace/tab starts in the focused pane's directory
/// rather than inheriting the palette process's cwd.
fn inject_cwd_for_creation(argv: &mut Vec<String>, cwd: &Path) {
    if argv.iter().any(|s| s == "--cwd") {
        return;
    }
    let program = argv.first().map(|s| s.as_str());
    let kind = argv.get(1).map(|s| s.as_str());
    let sub = argv.get(2).map(|s| s.as_str());
    let third = argv.get(3).map(|s| s.as_str());
    let needs_cwd = program == Some("herdr")
        && (matches!(
            (kind, sub),
            (Some("pane"), Some("split"))
                | (Some("workspace"), Some("create"))
                | (Some("tab"), Some("create"))
        ) || matches!(
            (kind, sub, third),
            (Some("plugin"), Some("pane"), Some("open"))
        ));
    if needs_cwd {
        argv.push("--cwd".to_string());
        argv.push(cwd.to_string_lossy().to_string());
    }
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

/// Execute a [`Dispatch`] using `cwd` as the working directory for the child
/// process. New pane/workspace/tab creation commands also receive an explicit
/// `--cwd` flag so they inherit the focused pane's directory. The palette
/// closes immediately after so Herdr retains focus. Errors are surfaced but
/// do not panic.
pub fn run(dispatch: &Dispatch, cwd: &Path) -> Result<()> {
    match dispatch {
        Dispatch::Cli(argv) => {
            let mut owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            inject_cwd_for_creation(&mut owned, cwd);
            let strs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
            run_argv(&strs, cwd)?;
        }
        Dispatch::FocusWorkspace(id) => {
            run_argv(&["herdr", "workspace", "focus", id], cwd)?;
        }
        Dispatch::FocusTab(id) => {
            run_argv(&["herdr", "tab", "focus", id], cwd)?;
        }
        Dispatch::FocusAgent(target) => {
            run_argv(&["herdr", "agent", "focus", target], cwd)?;
        }
        Dispatch::NextWorkspace => {
            focus_neighbor("workspace", Neighbor::Next, cwd)?;
        }
        Dispatch::PrevWorkspace => {
            focus_neighbor("workspace", Neighbor::Prev, cwd)?;
        }
        Dispatch::NextTab => {
            focus_neighbor("tab", Neighbor::Next, cwd)?;
        }
        Dispatch::PrevTab => {
            focus_neighbor("tab", Neighbor::Prev, cwd)?;
        }
        Dispatch::NextAgent => {
            focus_neighbor("agent", Neighbor::Next, cwd)?;
        }
        Dispatch::PrevAgent => {
            focus_neighbor("agent", Neighbor::Prev, cwd)?;
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
fn focus_neighbor(kind: &str, neighbor: Neighbor, cwd: &Path) -> Result<()> {
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
    run_argv(&["herdr", kind, "focus", id], cwd)
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
    let out = Command::new(herdr_bin()?).args([kind, "list"]).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "herdr {kind} list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    extract_focused_id(&text, kind).context("could not determine focused id from list output")
}

/// Extract ordered (id, label) entries from a herdr JSON list envelope.
/// Id field is kind-specific: `workspace_id`, `tab_id`, or `terminal_id`.
/// Label is `label` (workspaces/tabs); for agents we synthesize
/// `<agent> · <cwd basename>` since agents have no `label` field.
fn extract_entries(text: &str, kind: &str) -> Result<Vec<(String, String)>> {
    let id_field = id_field_for_kind(kind);
    let v: serde_json::Value = serde_json::from_str(text).context("list output was not JSON")?;
    let arr = list_array_for_kind(&v, kind).context("list output had no array")?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let id = entry
            .get(id_field)
            .and_then(|i| i.as_str())
            .map(str::to_string);
        let label = match kind {
            "agent" => {
                let agent = entry
                    .get("agent")
                    .and_then(|s| s.as_str())
                    .unwrap_or("agent");
                let cwd = entry
                    .get("cwd")
                    .and_then(|s| s.as_str())
                    .map(|c| {
                        std::path::Path::new(c)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_else(|| c.to_string())
                    })
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

fn extract_focused_id(text: &str, kind: &str) -> Result<String> {
    let id_field = id_field_for_kind(kind);
    let v: serde_json::Value = serde_json::from_str(text).context("list output was not JSON")?;
    let arr = list_array_for_kind(&v, kind).context("list output had no array")?;
    let fallback = arr
        .iter()
        .find_map(|entry| entry.get(id_field).and_then(|id| id.as_str()));
    arr.iter()
        .find(|entry| entry.get("focused").and_then(|focused| focused.as_bool()) == Some(true))
        .and_then(|entry| entry.get(id_field).and_then(|id| id.as_str()))
        .or(fallback)
        .map(str::to_string)
        .context("list output had no id")
}

fn list_array_for_kind<'a>(
    v: &'a serde_json::Value,
    kind: &str,
) -> Option<&'a Vec<serde_json::Value>> {
    let plural = match kind {
        "workspace" => "workspaces",
        "tab" => "tabs",
        "agent" => "agents",
        "pane" => "panes",
        other => other,
    };
    v.get("result")
        .and_then(|r| r.get(plural))
        .and_then(|w| w.as_array())
        .or_else(|| v.as_array())
}

fn id_field_for_kind(kind: &str) -> &'static str {
    match kind {
        "workspace" => "workspace_id",
        "tab" => "tab_id",
        "agent" => "terminal_id",
        _ => "id",
    }
}

/// Run an argv, resolving `argv[0] == "herdr"` to the real binary path. String
/// slices are promoted to owned for the child. The child runs with `cwd` as
/// its working directory.
fn run_argv(argv: &[&str], cwd: &Path) -> Result<()> {
    let mut owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    if owned.first().is_some_and(|first| first == "herdr") {
        owned[0] = herdr_bin()?;
    }
    let (cmd, args) = owned.split_first().context("empty argv")?;
    Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .spawn()?
        .wait()?;
    Ok(())
}

fn vec_into(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatchable_actions_map_to_herdr_0_7_cli() {
        assert!(matches!(
            dispatch_for_action("new_workspace"),
            Some(Dispatch::Cli(_))
        ));
        assert!(matches!(
            dispatch_for_action("split_vertical"),
            Some(Dispatch::Cli(ref argv)) if argv == &vec_into(&["herdr", "pane", "split", "--direction", "right", "--focus"])
        ));
        assert!(matches!(
            dispatch_for_action("split_horizontal"),
            Some(Dispatch::Cli(ref argv)) if argv == &vec_into(&["herdr", "pane", "split", "--direction", "down", "--focus"])
        ));
        assert!(matches!(
            dispatch_for_action("focus_pane_left"),
            Some(Dispatch::Cli(ref argv)) if argv == &vec_into(&["herdr", "pane", "focus", "--direction", "left"])
        ));
        assert!(matches!(
            dispatch_for_action("zoom"),
            Some(Dispatch::Cli(ref argv)) if argv == &vec_into(&["herdr", "pane", "zoom", "--current", "--toggle"])
        ));
    }

    #[test]
    fn id_or_prompt_required_actions_are_reference_only() {
        for action in [
            "rename_workspace",
            "close_workspace",
            "rename_tab",
            "close_tab",
            "rename_pane",
            "close_pane",
            "cycle_pane_next",
            "cycle_pane_previous",
        ] {
            assert!(
                dispatch_for_action(action).is_none(),
                "{action} should stay reference-only until palette can supply the required target/prompt"
            );
        }
    }

    #[test]
    fn prev_next_map_to_neighbor_dispatch() {
        assert!(matches!(
            dispatch_for_action("next_workspace"),
            Some(Dispatch::NextWorkspace)
        ));
        assert!(matches!(
            dispatch_for_action("previous_tab"),
            Some(Dispatch::PrevTab)
        ));
        assert!(matches!(
            dispatch_for_action("next_agent"),
            Some(Dispatch::NextAgent)
        ));
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
    fn extract_focused_id_uses_focused_field_before_fallback() {
        let ws = r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"one","focused":false},{"workspace_id":"w2","label":"two","focused":true}]}}"#;
        assert_eq!(extract_focused_id(ws, "workspace").unwrap(), "w2");

        let agents = r#"{"result":{"agents":[{"terminal_id":"term_1","focused":false},{"terminal_id":"term_2","focused":true}]}}"#;
        assert_eq!(extract_focused_id(agents, "agent").unwrap(), "term_2");
    }

    #[test]
    fn extract_entries_maps_kind_specific_id_fields() {
        let ws = r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"toolbox"}]}}"#;
        assert_eq!(
            extract_entries(ws, "workspace").unwrap(),
            vec![("w1".into(), "toolbox".into())]
        );

        let tabs = r#"{"result":{"tabs":[{"tab_id":"w1:t1","label":"logs"}]}}"#;
        assert_eq!(
            extract_entries(tabs, "tab").unwrap(),
            vec![("w1:t1".into(), "logs".into())]
        );
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
        assert_eq!(
            extract_entries(flat, "workspace").unwrap(),
            vec![("w1".into(), "a".into())]
        );
    }

    #[test]
    fn extract_current_pane_cwd_prefers_foreground_cwd() {
        let current = r#"{"result":{"pane":{"pane_id":"w1:p1","cwd":"/Users/x/base","foreground_cwd":"/Users/x/foreground"}}}"#;
        assert_eq!(
            extract_current_pane_cwd(current),
            Some(PathBuf::from("/Users/x/foreground"))
        );

        let cwd_only = r#"{"result":{"pane":{"pane_id":"w1:p1","cwd":"/Users/x/base"}}}"#;
        assert_eq!(
            extract_current_pane_cwd(cwd_only),
            Some(PathBuf::from("/Users/x/base"))
        );
    }

    #[cfg(unix)]
    mod cwd_propagation {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::sync::Mutex;

        static HERDR_BIN_LOCK: Mutex<()> = Mutex::new(());

        fn lock_herdr_bin() -> std::sync::MutexGuard<'static, ()> {
            HERDR_BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn write_fake_herdr(tmp: &Path, focused_cwd: &str) -> PathBuf {
            let script = tmp.join("fake_herdr");
            let log = tmp.join("log.txt");
            let json = format!(
                r#"{{"id":"cli:pane:list","result":{{"panes":[{{"pane_id":"w1:p1","cwd":"/wrong","focused":false}},{{"pane_id":"w1:p2","cwd":"{}","focused":true}}],"type":"pane_list"}}}}"#,
                focused_cwd
            );
            let content = format!(
                r#"#!/bin/sh
if [ "$1" = "pane" ] && [ "$2" = "list" ]; then
  printf '%s\n' '{}'
else
  printf '%s\n' "$*" >> {}
  pwd >> {}
fi
"#,
                json,
                log.display(),
                log.display()
            );
            fs::write(&script, content).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            script
        }

        fn write_fake_herdr_with_current_and_focused(
            tmp: &Path,
            current_cwd: &str,
            focused_cwd: &str,
        ) -> PathBuf {
            let script = tmp.join("fake_herdr");
            let log = tmp.join("log.txt");
            let current_json = format!(
                r#"{{"id":"cli:pane:current","result":{{"pane":{{"pane_id":"w1:p1","cwd":"/base","foreground_cwd":"{}","focused":false}},"type":"pane_current"}}}}"#,
                current_cwd
            );
            let list_json = format!(
                r#"{{"id":"cli:pane:list","result":{{"panes":[{{"pane_id":"w1:p1","cwd":"{}","focused":true}}],"type":"pane_list"}}}}"#,
                focused_cwd
            );
            let content = format!(
                r#"#!/bin/sh
if [ "$1" = "pane" ] && [ "$2" = "current" ]; then
  printf '%s\n' '{}'
elif [ "$1" = "pane" ] && [ "$2" = "list" ]; then
  printf '%s\n' '{}'
else
  printf '%s\n' "$*" >> {}
  pwd >> {}
fi
"#,
                current_json,
                list_json,
                log.display(),
                log.display()
            );
            fs::write(&script, content).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            script
        }

        #[test]
        fn focused_pane_cwd_prefers_current_pane_over_focused_list() {
            let _guard = lock_herdr_bin();
            let tmp = tempfile::tempdir().unwrap();
            let script = write_fake_herdr_with_current_and_focused(
                tmp.path(),
                "/Users/x/current",
                "/Users/x/focused",
            );
            std::env::set_var("HERDR_BIN_PATH", &script);
            let cwd = focused_pane_cwd();
            std::env::remove_var("HERDR_BIN_PATH");
            drop(_guard);
            assert_eq!(cwd, Some(PathBuf::from("/Users/x/current")));
        }

        #[test]
        fn focused_pane_cwd_reads_focused_pane() {
            let _guard = lock_herdr_bin();
            let tmp = tempfile::tempdir().unwrap();
            let script = write_fake_herdr(tmp.path(), "/Users/x/focused");
            std::env::set_var("HERDR_BIN_PATH", &script);
            let cwd = focused_pane_cwd();
            std::env::remove_var("HERDR_BIN_PATH");
            drop(_guard);
            assert_eq!(cwd, Some(PathBuf::from("/Users/x/focused")));
        }

        #[test]
        fn inject_cwd_adds_flag_to_creation_commands() {
            let cwd = Path::new("/Users/x/focused");

            let mut pane = vec![
                "herdr".into(),
                "pane".into(),
                "split".into(),
                "--direction".into(),
                "right".into(),
                "--focus".into(),
            ];
            inject_cwd_for_creation(&mut pane, cwd);
            assert_eq!(
                pane,
                vec![
                    "herdr",
                    "pane",
                    "split",
                    "--direction",
                    "right",
                    "--focus",
                    "--cwd",
                    "/Users/x/focused"
                ]
            );

            let mut workspace = vec![
                "herdr".into(),
                "workspace".into(),
                "create".into(),
                "--focus".into(),
            ];
            inject_cwd_for_creation(&mut workspace, cwd);
            assert_eq!(
                workspace,
                vec![
                    "herdr",
                    "workspace",
                    "create",
                    "--focus",
                    "--cwd",
                    "/Users/x/focused"
                ]
            );

            let mut tab = vec![
                "herdr".into(),
                "tab".into(),
                "create".into(),
                "--focus".into(),
            ];
            inject_cwd_for_creation(&mut tab, cwd);
            assert_eq!(
                tab,
                vec![
                    "herdr",
                    "tab",
                    "create",
                    "--focus",
                    "--cwd",
                    "/Users/x/focused"
                ]
            );

            let mut plugin = vec![
                "herdr".into(),
                "plugin".into(),
                "pane".into(),
                "open".into(),
                "--plugin".into(),
                "ramarivera.palette".into(),
                "--entrypoint".into(),
                "shell".into(),
            ];
            inject_cwd_for_creation(&mut plugin, cwd);
            assert_eq!(
                plugin,
                vec![
                    "herdr",
                    "plugin",
                    "pane",
                    "open",
                    "--plugin",
                    "ramarivera.palette",
                    "--entrypoint",
                    "shell",
                    "--cwd",
                    "/Users/x/focused"
                ]
            );
        }

        #[test]
        fn inject_cwd_skips_non_creation_commands() {
            let cwd = Path::new("/Users/x/focused");
            let mut argv = vec![
                "herdr".into(),
                "pane".into(),
                "focus".into(),
                "--direction".into(),
                "left".into(),
            ];
            inject_cwd_for_creation(&mut argv, cwd);
            assert_eq!(argv, vec!["herdr", "pane", "focus", "--direction", "left"]);
        }

        #[test]
        fn inject_cwd_does_not_duplicate_existing_flag() {
            let cwd = Path::new("/Users/x/focused");
            let mut argv = vec![
                "herdr".into(),
                "pane".into(),
                "split".into(),
                "--cwd".into(),
                "/other".into(),
                "--focus".into(),
            ];
            inject_cwd_for_creation(&mut argv, cwd);
            assert_eq!(
                argv,
                vec!["herdr", "pane", "split", "--cwd", "/other", "--focus"]
            );
        }

        #[test]
        fn dispatch_run_sets_child_cwd_and_injects_cwd_flag() {
            let _guard = lock_herdr_bin();
            let tmp = tempfile::tempdir().unwrap();
            let cwd = tmp.path().to_path_buf();
            let script = write_fake_herdr(tmp.path(), cwd.display().to_string().as_str());
            std::env::set_var("HERDR_BIN_PATH", &script);

            let dispatch = Dispatch::Cli(vec![
                "herdr".into(),
                "pane".into(),
                "split".into(),
                "--direction".into(),
                "right".into(),
                "--focus".into(),
            ]);
            run(&dispatch, &cwd).unwrap();

            std::env::remove_var("HERDR_BIN_PATH");
            drop(_guard);

            let log = fs::read_to_string(tmp.path().join("log.txt")).unwrap();
            assert!(log.contains(&format!(
                "pane split --direction right --focus --cwd {}",
                cwd.display()
            )));
            assert!(log.contains(cwd.display().to_string().as_str()));
        }
    }
}
