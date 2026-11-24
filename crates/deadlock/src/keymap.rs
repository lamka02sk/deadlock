//! Every key binding of the app in one list, with its default and the user's override
//! from the settings. [`apply`] rebinds them all; [`label`] is what tooltips and the
//! Settings tab show. Text editing keys inside inputs belong to gpui-component and are
//! not listed.

use std::rc::Rc;

use gpui_kit::{Action, App, Global, KeyBinding, KeyBindingContextPredicate, Keystroke};

use crate::settings::SettingsState;

/// `cmd-<key>` on macOS, `ctrl-<key>` elsewhere.
macro_rules! secondary {
    ($key:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $key)
        } else {
            concat!("ctrl-", $key)
        }
    };
}

/// One bindable action.
pub struct Entry {
    /// Stable id, the key in `settings.toml`.
    pub id: &'static str,
    pub group: &'static str,
    pub name: &'static str,
    pub default: &'static str,
    /// The key contexts it is bound in (`None`: everywhere).
    contexts: &'static [Option<&'static str>],
    action: fn() -> Box<dyn Action>,
}

const SHELL: &[Option<&str>] = &[Some("Shell")];
/// The bindings that must also win inside the query editor.
const SHELL_AND_EDITOR: &[Option<&str>] = &[Some("Shell"), Some("QueryEditor > Input")];

macro_rules! entry {
    ($id:literal, $group:literal, $name:literal, $default:expr, $contexts:expr, $action:expr) => {
        Entry {
            id: $id,
            group: $group,
            name: $name,
            default: $default,
            contexts: $contexts,
            action: || Box::new($action),
        }
    };
}

/// Every action the app binds, grouped as the code binds them.
pub static ENTRIES: &[Entry] = &[
    entry!(
        "new-query",
        "Window",
        "New query",
        secondary!("n"),
        SHELL,
        crate::shell::NewQuery
    ),
    entry!(
        "open-file",
        "Window",
        "Open file",
        secondary!("o"),
        SHELL,
        crate::shell::OpenFile
    ),
    entry!(
        "save",
        "Window",
        "Save (Apply in an object editor)",
        secondary!("s"),
        SHELL,
        crate::shell::SaveFile
    ),
    entry!(
        "run",
        "Window",
        "Run statement",
        secondary!("enter"),
        SHELL_AND_EDITOR,
        crate::shell::RunStatement
    ),
    entry!(
        "run-all",
        "Window",
        "Run file",
        secondary!("shift-enter"),
        SHELL,
        crate::shell::RunAll
    ),
    entry!(
        "stop",
        "Window",
        "Stop running statement",
        "ctrl-f2",
        SHELL,
        crate::shell::StopRun
    ),
    entry!(
        "console",
        "Window",
        "Open console",
        secondary!("shift-n"),
        SHELL,
        crate::shell::OpenConsole
    ),
    entry!(
        "format",
        "Window",
        "Format SQL",
        secondary!("alt-l"),
        SHELL,
        crate::shell::FormatSql
    ),
    entry!(
        "commit",
        "Window",
        "Commit transaction",
        secondary!("alt-shift-enter"),
        SHELL_AND_EDITOR,
        crate::shell::CommitTx
    ),
    entry!(
        "explain",
        "Window",
        "Explain plan",
        secondary!("shift-e"),
        SHELL_AND_EDITOR,
        crate::shell::ExplainPlan
    ),
    entry!(
        "explain-measured",
        "Window",
        "Explain plan, measured",
        secondary!("alt-shift-e"),
        SHELL_AND_EDITOR,
        crate::shell::ExplainAnalyze
    ),
    // Over the editor's Replace, which Ctrl+F's bar still offers.
    entry!(
        "history",
        "Window",
        "Query history",
        "ctrl-h",
        &[Some("Shell"), Some("Shell > Input")],
        crate::shell::ToggleHistory
    ),
    entry!(
        "files",
        "Window",
        "Files panel",
        secondary!("alt-b"),
        SHELL,
        crate::shell::ToggleFiles
    ),
    entry!(
        "files-filter",
        "Window",
        "Filter files",
        secondary!("alt-f"),
        SHELL,
        crate::shell::FilterFiles
    ),
    entry!(
        "refresh",
        "Window",
        "Refresh schema",
        "f5",
        SHELL,
        crate::shell::Refresh
    ),
    entry!(
        "search",
        "Window",
        "Search connections and objects",
        secondary!("shift-f"),
        SHELL,
        crate::shell::SearchObjects
    ),
    entry!(
        "search-everywhere",
        "Window",
        "Search everywhere (also Shift twice)",
        secondary!("shift-p"),
        &[Some("Shell"), Some("Shell > Input")],
        crate::shell::SearchEverywhere
    ),
    entry!(
        "settings",
        "Window",
        "Settings",
        secondary!(","),
        &[Some("Shell"), Some("Shell > Input")],
        crate::shell::OpenSettings
    ),
    entry!(
        "open-project",
        "Application",
        "Open project",
        secondary!("shift-o"),
        &[None],
        crate::app::OpenProject
    ),
    entry!(
        "open-folder",
        "Open-project window",
        "Open folder",
        secondary!("o"),
        &[Some(crate::welcome::CONTEXT)],
        crate::app::OpenFolder
    ),
    entry!(
        "quit",
        "Application",
        "Quit",
        secondary!("q"),
        &[None],
        crate::app::Quit
    ),
    entry!(
        "copy",
        "Results",
        "Copy selection",
        secondary!("c"),
        &[Some("Results")],
        crate::query::CopySelection
    ),
    entry!(
        "copy-headers",
        "Results",
        "Copy selection with headers",
        secondary!("shift-c"),
        &[Some("Results")],
        crate::results::CopyWithHeaders
    ),
    entry!(
        "select-all-cells",
        "Results",
        "Select all cells",
        secondary!("a"),
        &[Some("Results")],
        crate::results::SelectAll
    ),
    entry!(
        "extend-up",
        "Results",
        "Extend selection up",
        "shift-up",
        &[Some("Results")],
        crate::results::ExtendUp
    ),
    entry!(
        "extend-down",
        "Results",
        "Extend selection down",
        "shift-down",
        &[Some("Results")],
        crate::results::ExtendDown
    ),
    entry!(
        "extend-left",
        "Results",
        "Extend selection left",
        "shift-left",
        &[Some("Results")],
        crate::results::ExtendLeft
    ),
    entry!(
        "extend-right",
        "Results",
        "Extend selection right",
        "shift-right",
        &[Some("Results")],
        crate::results::ExtendRight
    ),
    entry!(
        "paste-cells",
        "Table data",
        "Paste into the selection",
        secondary!("v"),
        &[Some("TableData > Results")],
        crate::query::PasteCells
    ),
    entry!(
        "pick-date",
        "Cell editor",
        "Pick date",
        "alt-down",
        &[Some("CellEditor > Input")],
        crate::query::PickDate
    ),
    entry!(
        "edit-cell",
        "Table data",
        "Edit cell",
        "enter",
        &[Some("TableData")],
        crate::query::EditCell
    ),
    entry!(
        "set-null",
        "Table data",
        "Set NULL",
        "ctrl-alt-n",
        &[Some("TableData")],
        crate::query::SetNull
    ),
    entry!(
        "set-default",
        "Table data",
        "Set DEFAULT",
        "ctrl-alt-d",
        &[Some("TableData")],
        crate::query::SetDefault
    ),
    entry!(
        "add-row",
        "Table data",
        "Add row",
        "alt-insert",
        &[Some("TableData")],
        crate::query::AddRow
    ),
    entry!(
        "delete-rows",
        "Table data",
        "Delete rows",
        "delete",
        &[Some("TableData")],
        crate::query::DeleteRows
    ),
    entry!(
        "undo-change",
        "Table data",
        "Undo change",
        secondary!("z"),
        &[Some("TableData")],
        crate::query::UndoChange
    ),
    entry!(
        "next-cell",
        "Cell editor",
        "Next cell",
        "tab",
        &[Some("CellEditor > Input")],
        crate::query::NextCell
    ),
    entry!(
        "prev-cell",
        "Cell editor",
        "Previous cell",
        "shift-tab",
        &[Some("CellEditor > Input")],
        crate::query::PrevCell
    ),
    entry!(
        "tree-prev",
        "Schema tree",
        "Previous row",
        "up",
        &[Some("Tree")],
        crate::shell::SelectPrev
    ),
    entry!(
        "tree-next",
        "Schema tree",
        "Next row",
        "down",
        &[Some("Tree")],
        crate::shell::SelectNext
    ),
    entry!(
        "tree-first",
        "Schema tree",
        "First row",
        "home",
        &[Some("Tree")],
        crate::shell::SelectFirst
    ),
    entry!(
        "tree-last",
        "Schema tree",
        "Last row",
        "end",
        &[Some("Tree")],
        crate::shell::SelectLast
    ),
    entry!(
        "tree-expand",
        "Schema tree",
        "Expand",
        "right",
        &[Some("Tree")],
        crate::shell::ExpandNode
    ),
    entry!(
        "tree-collapse",
        "Schema tree",
        "Collapse",
        "left",
        &[Some("Tree")],
        crate::shell::CollapseNode
    ),
    entry!(
        "tree-toggle",
        "Schema tree",
        "Open or toggle",
        "enter",
        &[Some("Tree")],
        crate::shell::ToggleNode
    ),
    entry!(
        "history-up",
        "History panel",
        "Previous statement",
        "up",
        &[Some("History")],
        crate::history::Up
    ),
    entry!(
        "history-down",
        "History panel",
        "Next statement",
        "down",
        &[Some("History")],
        crate::history::Down
    ),
    entry!(
        "history-insert",
        "History panel",
        "Insert at caret",
        "enter",
        &[Some("History")],
        crate::history::InsertSelected
    ),
    entry!(
        "files-rename",
        "Files panel",
        "Rename",
        "f2",
        &[Some("Files")],
        crate::shell::RenameFile
    ),
    entry!(
        "files-delete",
        "Files panel",
        "Delete",
        "delete",
        &[Some("Files")],
        crate::shell::DeleteFile
    ),
    entry!(
        "everywhere-prev",
        "Search everywhere",
        "Previous result",
        "up",
        &[Some("Everywhere > Input")],
        crate::shell::EverywherePrev
    ),
    entry!(
        "everywhere-next",
        "Search everywhere",
        "Next result",
        "down",
        &[Some("Everywhere > Input")],
        crate::shell::EverywhereNext
    ),
    entry!(
        "show-completions",
        "Editor",
        "Show completions",
        "ctrl-space",
        &[Some("Input")],
        gpui_kit::component::input::ShowCompletions
    ),
    entry!(
        "dialog-cancel",
        "Dialog",
        "Cancel",
        "escape",
        &[Some("Dialog")],
        crate::dialog::Cancel
    ),
    entry!(
        "dialog-confirm",
        "Dialog",
        "Default action",
        "enter",
        &[Some("Dialog")],
        crate::dialog::Confirm
    ),
    entry!(
        "dialog-press",
        "Dialog",
        "Press the focused button",
        "space",
        &[Some("Dialog")],
        crate::dialog::Press
    ),
    entry!(
        "dialog-next",
        "Dialog",
        "Next button",
        "tab",
        &[Some("Dialog")],
        crate::dialog::NextButton
    ),
    entry!(
        "dialog-prev",
        "Dialog",
        "Previous button",
        "shift-tab",
        &[Some("Dialog")],
        crate::dialog::PrevButton
    ),
];

/// gpui-component's own bindings, kept to bind again under ours.
struct Base(Vec<KeyBinding>);

impl Global for Base {}

/// Remembers gpui-component's bindings, then binds the app's. Call after
/// `gpui_kit::init` and the settings.
pub fn init(cx: &mut App) {
    let base: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
    cx.set_global(Base(base));
    apply(cx);
}

pub fn entry(id: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|e| e.id == id)
}

/// The keystrokes of action `id` now: the user's, else the default.
pub fn keys(id: &str, cx: &App) -> String {
    let user = cx
        .try_global::<SettingsState>()
        .and_then(|s| s.settings.shortcuts.get(id).cloned());
    user.or_else(|| entry(id).map(|e| e.default.to_owned()))
        .unwrap_or_default()
}

/// The binding of action `id` as people read it: `Ctrl+Shift+N`.
pub fn label(id: &str, cx: &App) -> String {
    display(&keys(id, cx))
}

/// `ctrl-shift-enter` → `Ctrl+Shift+Enter`.
pub fn display(keys: &str) -> String {
    keys.split_whitespace()
        .map(|stroke| {
            stroke
                .split('-')
                .filter(|p| !p.is_empty())
                .map(|part| match part {
                    "ctrl" => "Ctrl".to_owned(),
                    "cmd" | "platform" => "Cmd".to_owned(),
                    "alt" => "Alt".to_owned(),
                    "shift" => "Shift".to_owned(),
                    "fn" => "Fn".to_owned(),
                    "escape" => "Esc".to_owned(),
                    "pageup" => "Page Up".to_owned(),
                    "pagedown" => "Page Down".to_owned(),
                    p => {
                        let mut c = p.chars();
                        c.next()
                            .map(|f| f.to_uppercase().chain(c).collect())
                            .unwrap_or_default()
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Actions in the same scope cannot share a key: the window's and the app's bindings
/// are all live at once; each other group has its own context.
fn scope(group: &str) -> &str {
    match group {
        "Window" | "Application" => "global",
        g => g,
    }
}

/// The action that already uses `keys` where `id` would, if any.
pub fn conflict(id: &str, keys: &str, cx: &App) -> Option<&'static Entry> {
    let me = entry(id)?;
    ENTRIES
        .iter()
        .find(|e| e.id != id && scope(e.group) == scope(me.group) && self::keys(e.id, cx) == keys)
}

/// The keystroke as a binding string (`ctrl-shift-n`), or `None` for a lone modifier.
pub fn keystroke_text(k: &Keystroke) -> Option<String> {
    if k.is_modifier_key() || k.key.is_empty() {
        return None;
    }
    let m = &k.modifiers;
    let mut out = String::new();
    for (on, name) in [
        (m.control, "ctrl-"),
        (m.alt, "alt-"),
        (m.shift, "shift-"),
        (m.platform, "cmd-"),
        (m.function, "fn-"),
    ] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(&k.key);
    Some(out)
}

/// Binds gpui-component's keys, then every app action with its current keys.
pub fn apply(cx: &mut App) {
    let base = cx
        .try_global::<Base>()
        .map(|b| b.0.clone())
        .unwrap_or_default();
    let mut ours = Vec::new();
    for e in ENTRIES {
        let keys = keys(e.id, cx);
        for context in e.contexts {
            let predicate = context
                .and_then(|c| KeyBindingContextPredicate::parse(c).ok())
                .map(Rc::new);
            match KeyBinding::load(&keys, (e.action)(), predicate, false, None, cx) {
                Ok(b) => ours.push(b),
                Err(err) => eprintln!("deadlock: shortcut {}: {err}", e.id),
            }
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(base);
    cx.bind_keys(ours);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_read_like_the_design() {
        assert_eq!(display("ctrl-shift-n"), "Ctrl+Shift+N");
        assert_eq!(display("ctrl-enter"), "Ctrl+Enter");
        assert_eq!(display("f5"), "F5");
        assert_eq!(display("escape"), "Esc");
        assert_eq!(display("ctrl-alt-shift-enter"), "Ctrl+Alt+Shift+Enter");
    }

    #[test]
    fn ids_are_unique_and_defaults_parse() {
        let mut ids: Vec<&str> = ENTRIES.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ENTRIES.len());
        for e in ENTRIES {
            for stroke in e.default.split_whitespace() {
                assert!(Keystroke::parse(stroke).is_ok(), "{}: {stroke}", e.id);
            }
        }
    }

    #[test]
    fn default_bindings_do_not_conflict() {
        for a in ENTRIES {
            for b in ENTRIES {
                if a.id != b.id && scope(a.group) == scope(b.group) {
                    assert_ne!(a.default, b.default, "{} and {}", a.id, b.id);
                }
            }
        }
    }
}
