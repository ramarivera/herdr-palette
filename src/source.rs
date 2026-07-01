//! Collects every palette [`Item`] by fusing the live Herdr surfaces.
//!
//! Sources, in collection order:
//!   1. Built-in keybinding actions (from `herdr_pretty_which`, config-aware)
//!   2. Plugin actions (`herdr plugin action list --json`)
//!   3. User `[[keys.command]]` entries (from config)
//!   4. Jump targets: workspaces, tabs, agents (live, via list --json)
//!
//! Every source is best-effort and degrades gracefully — a missing plugin
//! surface or a failed `agent list` just yields fewer rows, never a crash.

use crate::items::{
    item_from_binding, item_from_command, item_from_jump, item_from_plugin_action, Item, ItemKind,
};
use anyhow::{Context, Result};
use herdr_pretty_which::config::load_herdr_config;
use herdr_pretty_which::discover::discover_default_config_actions;
use herdr_pretty_which::model::effective_bindings_with_discovery;
use herdr_pretty_which::theme::Palette;
use std::path::PathBuf;
use std::process::Command;

/// The full collected palette, plus the resolved config source (for display
/// path), the theme name (for the header), the resolved [`Palette`] (so the
/// caller doesn't have to re-load the config to render), and the focused
/// pane's cwd (used as the working directory for shell commands and new
/// surface creation).
#[allow(dead_code)]
pub struct PaletteData {
    pub items: Vec<Item>,
    pub config_path: String,
    pub theme_name: String,
    pub palette: Palette,
    pub cwd: PathBuf,
}

/// Load config, discover actions, and collect every dispatchable + reference
/// row. `config_path` overrides the default lookup when provided (mirrors
/// `herdr-pretty-which --config`).
pub fn collect(config_path: Option<PathBuf>) -> Result<PaletteData> {
    let source = load_herdr_config(config_path)?;
    let discovered = discover_default_config_actions();
    let bindings = effective_bindings_with_discovery(&source.config.keys, Some(&discovered));
    let theme_name = source
        .config
        .theme
        .name
        .clone()
        .unwrap_or_else(|| "terminal".to_string());

    let mut items: Vec<Item> = Vec::new();

    // 1. Built-in keybinding actions.
    for b in &bindings {
        items.push(item_from_binding(b));
    }

    // 2. Plugin actions.
    for pa in plugin_actions().unwrap_or_default() {
        items.push(pa);
    }

    // 3. User `[[keys.command]]`.
    for cmd in &source.config.keys.command {
        items.push(item_from_command(cmd));
    }

    // 4. Jump targets (live).
    for j in jump_workspaces().unwrap_or_default() {
        items.push(j);
    }
    for j in jump_tabs().unwrap_or_default() {
        items.push(j);
    }
    for j in jump_agents().unwrap_or_default() {
        items.push(j);
    }

    let config_path = display_path(&source.path);
    let palette = Palette::from_theme(&source.config.theme);
    let cwd = focused_pane_cwd();

    Ok(PaletteData {
        items,
        config_path,
        theme_name,
        palette,
        cwd,
    })
}

/// Resolve the focused pane's cwd, falling back to the current process cwd.
fn focused_pane_cwd() -> PathBuf {
    crate::dispatch::focused_pane_cwd()
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
}

/// Parsed row from `herdr plugin action list`.
struct RawPluginAction {
    plugin_id: String,
    action_id: String,
    title: Option<String>,
    command: Vec<String>,
}

fn plugin_actions() -> Result<Vec<Item>> {
    let out = Command::new(crate::dispatch::herdr_bin()?)
        .args(["plugin", "action", "list"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "plugin action list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let raws = parse_plugin_actions(&text).context("could not parse plugin action list")?;
    Ok(raws
        .into_iter()
        .map(|r| {
            item_from_plugin_action(&r.plugin_id, &r.action_id, r.title.as_deref(), &r.command)
        })
        .collect())
}

fn parse_plugin_actions(text: &str) -> Result<Vec<RawPluginAction>> {
    let v: serde_json::Value = serde_json::from_str(text).context("plugin list not JSON")?;
    let arr = v
        .get("result")
        .and_then(|r| r.get("actions"))
        .and_then(|a| a.as_array())
        .or_else(|| v.as_array())
        .context("plugin list had no actions array")?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let plugin_id = entry
            .get("plugin_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let action_id = entry
            .get("action_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let title = entry
            .get("title")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        let command = entry
            .get("command")
            .and_then(|c| c.as_array())
            .map(|c| {
                c.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !plugin_id.is_empty() && !action_id.is_empty() && !command.is_empty() {
            out.push(RawPluginAction {
                plugin_id,
                action_id,
                title,
                command,
            });
        }
    }
    Ok(out)
}

fn jump_workspaces() -> Result<Vec<Item>> {
    let entries = crate::dispatch::list_entries("workspace")?;
    Ok(entries
        .into_iter()
        .map(|(id, label)| item_from_jump(ItemKind::JumpWorkspace, &label_or_id(&label, &id), &id))
        .collect())
}

fn jump_tabs() -> Result<Vec<Item>> {
    let entries = crate::dispatch::list_entries("tab")?;
    Ok(entries
        .into_iter()
        .map(|(id, label)| item_from_jump(ItemKind::JumpTab, &label_or_id(&label, &id), &id))
        .collect())
}

fn jump_agents() -> Result<Vec<Item>> {
    let entries = crate::dispatch::list_entries("agent")?;
    Ok(entries
        .into_iter()
        .map(|(id, label)| item_from_jump(ItemKind::JumpAgent, &label_or_id(&label, &id), &id))
        .collect())
}

fn label_or_id(label: &str, id: &str) -> String {
    if label.is_empty() {
        id.to_string()
    } else {
        label.to_string()
    }
}

/// Render a path for display: `~/...` under home, else as-is.
pub fn display_path(path: &std::path::Path) -> String {
    let home = dirs::home_dir();
    match home {
        Some(home) if path.starts_with(&home) => {
            let rest = path.strip_prefix(&home).unwrap_or(path);
            format!("~/{}", rest.display())
        }
        _ => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plugin_actions_handles_real_shape() {
        let real = r#"{"id":"cli:plugin","result":{"actions":[
            {"action_id":"open","command":["herdr","plugin","pane","open","--plugin","ramarivera.pretty-which"],"contexts":["workspace","tab","pane","global"],"platforms":["linux","macos"],"plugin_id":"ramarivera.pretty-which","title":"Open pretty which"},
            {"action_id":"open","command":["herdr","plugin","pane","open","--plugin","roxasroot.pretty-help"],"contexts":["workspace","tab","pane","global"],"platforms":["linux","macos"],"plugin_id":"roxasroot.pretty-help","title":"Open pretty help"}
        ],"type":"plugin_action_list"}}"#;
        let parsed = parse_plugin_actions(real).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].plugin_id, "ramarivera.pretty-which");
        assert_eq!(parsed[0].action_id, "open");
        assert_eq!(parsed[0].command[0], "herdr");
        assert_eq!(parsed[1].plugin_id, "roxasroot.pretty-help");
    }

    #[test]
    fn parse_plugin_actions_skips_incomplete_rows() {
        let partial = r#"{"result":{"actions":[
            {"action_id":"open","plugin_id":"x","command":[]},
            {"action_id":"open","plugin_id":"y","command":["herdr","ok"]}
        ]}}"#;
        let parsed = parse_plugin_actions(partial).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].plugin_id, "y");
    }

    #[test]
    fn display_path_tildifies_home() {
        let home = dirs::home_dir().unwrap();
        let p = display_path(&home.join(".config").join("herdr").join("config.toml"));
        assert!(p.starts_with("~/"));
        assert!(p.contains("herdr/config.toml"));
    }
}
