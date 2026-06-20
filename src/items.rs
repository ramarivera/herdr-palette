//! Unified palette item model.
//!
//! A palette fuses several Herdr surfaces into one fuzzyable list:
//!   - Built-in keybinding actions (reused from `herdr_pretty_which::model`)
//!   - Plugin actions discovered via `herdr plugin action list` (each carries a
//!     fully-resolved `command[]` we can run directly)
//!   - Jump targets: workspaces, tabs, agents (via their list + focus commands)
//!   - User `[[keys.command]]` entries
//!
//! Each item carries an optional [`Dispatch`]. Items with `dispatch = None` are
//! reference-only (they render greyed with their keybinding shown) because Herdr
//! v1 exposes no programmatic path to trigger them from a plugin — the socket
//! API can create/focus/split/close panes and invoke plugin actions, but cannot
//! replay an arbitrary keybinding chord like `prefix+?` (help) or `prefix+s`
//! (settings). See `crate::dispatch` for the tier A/B/C rationale.

use herdr_pretty_which::model::{Binding, BindingStatus, Category, CommandBinding, KeyValue};

/// What kind of source an [`Item`] came from. Drives category labeling and
/// rendering accents, never dispatch logic (dispatch lives in [`Dispatch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// A Herdr built-in keybinding action (`[keys]` section).
    Binding,
    /// An action declared by some installed plugin (`plugin.action.list`).
    PluginAction,
    /// A user `[[keys.command]]` entry.
    CustomCommand,
    /// A jump destination from `workspace list`.
    JumpWorkspace,
    /// A jump destination from `tab list`.
    JumpTab,
    /// A jump destination from `agent list`.
    JumpAgent,
}

impl ItemKind {
    pub fn category_label(self) -> &'static str {
        match self {
            ItemKind::Binding => "Keybinding",
            ItemKind::PluginAction => "Plugin",
            ItemKind::CustomCommand => "Custom",
            ItemKind::JumpWorkspace => "Workspace",
            ItemKind::JumpTab => "Tab",
            ItemKind::JumpAgent => "Agent",
        }
    }
}

/// How to make an [`Item`] actually happen when the user presses Enter.
///
/// `Cli` covers both built-in `herdr <subcommand>` dispatch AND plugin-action
/// dispatch, because plugin actions already ship a fully-resolved `command[]`
/// (e.g. `["herdr","plugin","pane","open","--plugin","ramarivera.pretty-which",
/// "--entrypoint","overlay","--placement","overlay","--focus"]`). Next/Prev
/// variants resolve the live ordered list then focus the neighbor.
#[derive(Debug, Clone)]
pub enum Dispatch {
    /// Run an argv vector. Binary is `argv[0]` (normally `"herdr"`); resolved
    /// via `HERDR_BIN_PATH` or PATH at dispatch time.
    Cli(Vec<String>),
    /// Focus a workspace by id (`herdr workspace focus <id>`).
    FocusWorkspace(String),
    /// Focus a tab by id (`herdr tab focus <id>`).
    FocusTab(String),
    /// Focus an agent by target (`herdr agent focus <target>`).
    FocusAgent(String),
    /// Cycle to the neighbor (list + resolve + focus).
    NextWorkspace,
    PrevWorkspace,
    NextTab,
    PrevTab,
    NextAgent,
    PrevAgent,
}

/// One fuzzyable row in the palette.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: ItemKind,
    pub title: String,
    /// Secondary line: hint, plugin id, or target detail.
    pub subtitle: String,
    /// Keybinding display chips, e.g. `["prefix+n"]`. Empty for non-keybindings.
    pub keys: Vec<String>,
    /// When `None`, the item is reference-only and renders greyed.
    pub dispatch: Option<Dispatch>,
}

impl Item {
    /// Build the fuzzy-match haystack. Biased toward title, then subtitle, then
    /// keys, so typing "split" ranks "Split vertical" first.
    pub fn haystack(&self) -> String {
        let mut s = String::with_capacity(self.title.len() + self.subtitle.len() + 16);
        s.push_str(&self.title);
        s.push(' ');
        s.push_str(&self.subtitle);
        s.push(' ');
        s.push_str(self.kind.category_label());
        if !self.keys.is_empty() {
            s.push(' ');
            s.push_str(&self.keys.join(" "));
        }
        s
    }

    /// True when this row can actually be fired (Enter will do something).
    pub fn is_dispatchable(&self) -> bool {
        self.dispatch.is_some()
    }
}

/// Convert a Pretty Which [`Binding`] into a palette [`Item`], computing its
/// dispatch tier. Bindings with no programmatic path in Herdr v1 get
/// `dispatch = None` and render as reference rows.
pub fn item_from_binding(binding: &Binding) -> Item {
    let dispatch = crate::dispatch::dispatch_for_action(&binding.action);
    Item {
        kind: ItemKind::Binding,
        title: binding.label.clone(),
        subtitle: binding.hint.clone(),
        keys: binding.keys.clone(),
        dispatch,
    }
}

/// Convert a user `[[keys.command]]` [`CommandBinding`] into an [`Item`].
/// Plugin-action-typed commands are dispatchable via their command; shell/pane
/// commands are left reference-only in v1 because faithfully replaying a
/// temp-pane command from outside the keybinding layer is better left to its
/// chord.
pub fn item_from_command(cmd: &CommandBinding) -> Item {
    let name = cmd
        .name
        .clone()
        .unwrap_or_else(|| "Unnamed command".to_string());
    let subtitle = cmd
        .description
        .clone()
        .or_else(|| cmd.command.clone())
        .unwrap_or_else(|| "Custom Herdr command".to_string());
    let keys = cmd.key.as_ref().map(|kv| kv.keys()).unwrap_or_default();
    // Plugin-action-typed commands carry a resolvable argv string, but we
    // cannot shell-split it safely from outside the keybinding layer, so keep
    // the chord as the dispatch path and mark these reference-only. Typed
    // plugin actions discovered via `herdr plugin action list` are handled
    // separately (and ARE dispatchable, via their real `command[]`).
    let dispatch: Option<Dispatch> = None;
    Item {
        kind: ItemKind::CustomCommand,
        title: name,
        subtitle,
        keys,
        dispatch,
    }
}

/// A discovered plugin action row from `plugin.action.list`. The action ships a
/// fully-resolved `command[]`, so we store and run it directly via
/// [`Dispatch::Cli`] — no synthetic invoke indirection.
pub fn item_from_plugin_action(
    plugin_id: &str,
    action_id: &str,
    title: Option<&str>,
    command: &[String],
) -> Item {
    let qualified = if action_id.contains('.') {
        action_id.to_string()
    } else {
        format!("{plugin_id}.{action_id}")
    };
    let label = title
        .map(str::to_string)
        .unwrap_or_else(|| qualified.clone());
    Item {
        kind: ItemKind::PluginAction,
        title: label,
        subtitle: qualified,
        keys: Vec::new(),
        dispatch: Some(Dispatch::Cli(command.to_vec())),
    }
}

/// A jump target (workspace/tab/agent). The id/target flows into the focus
/// dispatch.
pub fn item_from_jump(kind: ItemKind, title: &str, id: &str) -> Item {
    let dispatch = match kind {
        ItemKind::JumpWorkspace => Some(Dispatch::FocusWorkspace(id.to_string())),
        ItemKind::JumpTab => Some(Dispatch::FocusTab(id.to_string())),
        ItemKind::JumpAgent => Some(Dispatch::FocusAgent(id.to_string())),
        _ => None,
    };
    Item {
        kind,
        title: title.to_string(),
        subtitle: id.to_string(),
        keys: Vec::new(),
        dispatch,
    }
}

/// Whether a Pretty Which binding is worth showing as a reference-only row
/// (disabled bindings and keyless discovered actions are noise in a palette).
#[allow(dead_code)]
pub fn binding_is_reference(binding: &Binding) -> bool {
    binding.status == BindingStatus::Disabled
        || (matches!(binding.category, Category::Discovered) && binding.keys.is_empty())
}

/// Helper kept for callers that already have a `KeyValue` they want flattened.
#[allow(dead_code)]
pub fn kv_keys(kv: &KeyValue) -> Vec<String> {
    kv.keys()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, key: &str, ctype: Option<&str>, command: Option<&str>) -> CommandBinding {
        CommandBinding {
            name: Some(name.to_string()),
            description: None,
            key: Some(KeyValue::One(key.to_string())),
            r#type: ctype.map(str::to_string),
            command: command.map(str::to_string),
        }
    }

    #[test]
    fn plugin_action_item_runs_real_command_array() {
        let command = vec![
            "herdr".to_string(),
            "plugin".to_string(),
            "pane".to_string(),
            "open".to_string(),
            "--plugin".to_string(),
            "ramarivera.pretty-which".to_string(),
        ];
        let item = item_from_plugin_action(
            "ramarivera.pretty-which",
            "open",
            Some("Open pretty which"),
            &command,
        );
        assert_eq!(item.subtitle, "ramarivera.pretty-which.open");
        assert!(matches!(
            item.dispatch,
            Some(Dispatch::Cli(ref argv)) if argv == &command
        ));
    }

    #[test]
    fn shell_command_is_reference_only_in_v1() {
        let item = item_from_command(&cmd("lazygit", "prefix+alt+g", Some("pane"), Some("lazygit")));
        assert!(item.dispatch.is_none());
        assert!(!item.is_dispatchable());
    }

    #[test]
    fn haystack_includes_title_subtitle_kind_and_keys() {
        let item = Item {
            kind: ItemKind::Binding,
            title: "Split vertical".into(),
            subtitle: "Split side by side.".into(),
            keys: vec!["prefix+v".into()],
            dispatch: None,
        };
        let h = item.haystack();
        assert!(h.contains("Split vertical"));
        assert!(h.contains("prefix+v"));
        assert!(h.contains("Keybinding"));
    }

    #[test]
    fn jump_items_are_dispatchable() {
        let ws = item_from_jump(ItemKind::JumpWorkspace, "api", "w1");
        assert!(matches!(ws.dispatch, Some(Dispatch::FocusWorkspace(_))));
        let tab = item_from_jump(ItemKind::JumpTab, "logs", "w1:t2");
        assert!(matches!(tab.dispatch, Some(Dispatch::FocusTab(_))));
        assert!(ws.is_dispatchable());
    }
}
