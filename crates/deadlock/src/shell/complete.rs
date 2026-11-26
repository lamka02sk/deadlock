//! The SQL editor's completion provider: suggestions from the sidebar's schema tree for
//! the tab's connection. A schema a statement names that is not loaded yet loads in the
//! background, and the open popup refreshes when it lands. Also the object links:
//! Ctrl/Cmd over a table, view or routine name underlines it, and the click opens it
//! as the sidebar does.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use deadlock_core::complete::{self, Catalog, ItemKind};
use deadlock_core::connection::{ConnectionConfig, Engine};
use deadlock_core::schema::{Load, NodeId};
use gpui_kit::component::input::{
    CompletionProvider, DefinitionProvider, EditorState, Rope, RopeExt as _, highlight_contains,
};
use gpui_kit::{AnyWindowHandle, App, Entity, Focusable as _, Task, WeakEntity, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, TextEdit,
};

use super::{Conn, Shell};
use crate::query::QueryTab;

/// Text around the caret the provider reads: a statement longer than this is cut.
const WINDOW: usize = 64 * 1024;

/// The connection an editor completes for, read at each request.
type Connection = Rc<dyn Fn(&App) -> Option<ConnectionConfig>>;

/// The schema unqualified names resolve in, read at each request.
type Schema = Rc<dyn Fn(&App) -> Option<String>>;

struct Sql {
    shell: WeakEntity<Shell>,
    connection: Connection,
    schema: Schema,
    editor: WeakEntity<EditorState>,
}

/// The object the pointer names, found while Ctrl/Cmd hovers: its connection and link.
type Target = Rc<RefCell<Option<(String, complete::Link)>>>;

/// Object links for Ctrl/Cmd-click.
struct Links {
    shell: WeakEntity<Shell>,
    connection: Connection,
    schema: Schema,
    target: Target,
}

/// `rope` around `offset` as a string, and where it starts: a statement starting more
/// than [`WINDOW`] bytes before is read from the middle.
fn around(rope: &Rope, offset: usize) -> (String, usize) {
    // shortcut: fine for hand-written SQL, upgrade if huge scripts misdetect.
    let mut start = offset.saturating_sub(WINDOW);
    while !rope.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (offset + WINDOW / 4).min(rope.len());
    while !rope.is_char_boundary(end) {
        end += 1;
    }
    (rope.slice(start..end).to_string(), start)
}

/// Gives the tab's editor SQL completions.
pub(super) fn install(shell: WeakEntity<Shell>, tab: &Entity<QueryTab>, cx: &mut App) {
    let editor = tab.read(cx).editor.clone();
    let tab = tab.downgrade();
    let t = tab.clone();
    let connection: Connection =
        Rc::new(move |cx| t.upgrade().and_then(|t| t.read(cx).connection.clone()));
    let schema: Schema = Rc::new(move |cx| {
        tab.upgrade()
            .and_then(|t| t.read(cx).schema().map(str::to_owned))
    });
    install_editor(shell, &editor, connection, schema, cx);
}

/// Gives `editor` SQL completions for the connection `connection` returns.
pub(super) fn install_editor(
    shell: WeakEntity<Shell>,
    editor: &Entity<EditorState>,
    connection: Connection,
    schema: Schema,
    cx: &mut App,
) {
    let target: Target = Rc::default();
    let links = Links {
        shell: shell.clone(),
        connection: Rc::clone(&connection),
        schema: Rc::clone(&schema),
        target: Rc::clone(&target),
    };
    let show = open_target(shell.clone(), target);
    let provider = Sql {
        shell,
        connection,
        schema,
        editor: editor.downgrade(),
    };
    editor.update(cx, |e, _| {
        let lsp = e.lsp_mut();
        lsp.completion_provider = Some(Rc::new(provider));
        lsp.definition_provider = Some(Rc::new(links));
        lsp.show_document = Some(show);
        // The editor's query can start at a `.`; paint the part after it.
        lsp.set_completion_highlighter(Rc::new(|query: &str, item: &CompletionItem| {
            highlight_contains(query.rsplit('.').next().unwrap_or(query), item)
        }));
    });
}

impl CompletionProvider for Sql {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let (shell, connection, schema, editor) = (
            self.shell.clone(),
            Rc::clone(&self.connection),
            Rc::clone(&self.schema),
            self.editor.clone(),
        );
        let text = text.clone();
        let handle = window.window_handle();
        let invoked = trigger.trigger_kind == CompletionTriggerKind::INVOKED;
        // On the next turn: the shell or the tab may be mid-update while the editor asks.
        cx.spawn(async move |cx| {
            let items = cx.update(|cx| {
                let config = connection(cx);
                let current = schema(cx);
                let (items, load) =
                    suggest(&shell, config.as_ref(), current, &text, offset, invoked, cx);
                if let Some((conn, missing)) = load {
                    load_then_refresh(&shell, &conn, missing, editor, handle, offset, cx);
                }
                items
            });
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, cx: &mut App) -> bool {
        let mut chars = new_text.chars();
        let typed = chars.next().filter(|_| chars.next().is_none());
        crate::settings::get(cx).autocomplete
            && typed.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.'))
    }
}

/// The popup's items, and the connection's schemas to load for more.
fn suggest(
    shell: &WeakEntity<Shell>,
    config: Option<&ConnectionConfig>,
    current: Option<String>,
    rope: &Rope,
    offset: usize,
    invoked: bool,
    cx: &App,
) -> (Vec<CompletionItem>, Option<(String, Vec<String>)>) {
    let Some(shell) = shell.upgrade() else {
        return (Vec::new(), None);
    };
    let engine = config.map_or(Engine::Mysql, |c| c.engine);
    if engine == Engine::Redis {
        return (Vec::new(), None);
    }
    let shell = shell.read(cx);
    let tree = config.and_then(|c| shell.tree.connections.iter().find(|t| t.id == c.id));
    let catalog = Catalog {
        schemas: tree.and_then(|t| t.nodes.as_deref()).unwrap_or_default(),
        hidden: tree.map_or(&[], |t| &t.hidden),
        current: current.as_deref(),
    };
    let (text, start) = around(rope, offset);
    let case = crate::settings::get(cx).keyword_case;
    let Some(found) = complete::complete(&text, offset - start, engine, &catalog, case) else {
        return (Vec::new(), None);
    };
    let load = config
        .filter(|_| !found.missing.is_empty())
        .map(|c| (c.id.clone(), found.missing.clone()));
    let typed = &text[found.replace.clone()];
    // A finished word closes the popup, so Enter after it still breaks the line.
    if !invoked
        && found
            .items
            .iter()
            .any(|i| i.insert.eq_ignore_ascii_case(typed))
    {
        return (Vec::new(), load);
    }
    let range = lsp_types::Range {
        start: rope.offset_to_position(start + found.replace.start),
        end: rope.offset_to_position(start + found.replace.end),
    };
    let items = found
        .items
        .into_iter()
        .map(|i| CompletionItem {
            kind: Some(match i.kind {
                ItemKind::Keyword => CompletionItemKind::KEYWORD,
                ItemKind::Function => CompletionItemKind::FUNCTION,
                ItemKind::Schema => CompletionItemKind::MODULE,
                ItemKind::Table | ItemKind::View => CompletionItemKind::STRUCT,
                ItemKind::Column => CompletionItemKind::FIELD,
            }),
            detail: Some(i.detail),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: i.insert,
            })),
            label: i.label,
            ..Default::default()
        })
        .collect();
    (items, load)
}

/// Starts loading the schemas of `conn` that are not loaded, if it is connected; the
/// ids that load.
fn load_missing(
    shell: &WeakEntity<Shell>,
    conn: &str,
    missing: Vec<String>,
    cx: &mut App,
) -> Vec<NodeId> {
    let Some(entity) = shell.upgrade() else {
        return Vec::new();
    };
    entity.update(cx, |s, cx| {
        if !matches!(s.conns.get(conn), Some(Conn::Connected(_))) {
            return Vec::new();
        }
        let ids: Vec<NodeId> = missing
            .into_iter()
            .map(|db| vec![conn.to_owned(), db])
            // A failed load is not retried on every keystroke.
            .filter(|id| !s.tree.status.contains_key(id) && s.tree.node(id).is_some())
            .collect();
        for id in &ids {
            s.load(id.clone(), cx);
        }
        ids
    })
}

/// Loads the schemas of `conn` that are not loaded, if it is connected, then asks the
/// editor again while the caret has not moved.
fn load_then_refresh(
    shell: &WeakEntity<Shell>,
    conn: &str,
    missing: Vec<String>,
    editor: WeakEntity<EditorState>,
    window: AnyWindowHandle,
    offset: usize,
    cx: &mut App,
) {
    let ids = load_missing(shell, conn, missing, cx);
    if ids.is_empty() {
        return;
    }
    let shell = shell.clone();
    cx.spawn(async move |cx| {
        // shortcut: polls the tree; an event from the sidebar's load would save the timer.
        for _ in 0..100 {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let loading = shell.read_with(cx, |s, _| {
                ids.iter()
                    .any(|id| s.tree.status.get(id) == Some(&Load::Loading))
            });
            if !loading.unwrap_or(false) {
                break;
            }
        }
        let _ = window.update(cx, |_, window, cx| {
            let _ = editor.update(cx, |e, cx| {
                if e.focus_handle(cx).is_focused(window) && e.cursor() == offset {
                    e.dismiss_completion_overlay(cx);
                    e.show_completions(window, cx);
                }
            });
        });
    })
    .detach();
}

impl DefinitionProvider for Links {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<lsp_types::LocationLink>>> {
        let (shell, connection, schema, target) = (
            self.shell.clone(),
            Rc::clone(&self.connection),
            Rc::clone(&self.schema),
            Rc::clone(&self.target),
        );
        let rope = text.clone();
        // On the next turn: the shell or the tab may be mid-update while the editor asks.
        cx.spawn(async move |cx| {
            let link = cx.update(|cx| {
                let config = connection(cx)?;
                let current = schema(cx);
                let (link, missing) = link_at(&shell, &config, current, &rope, offset, cx);
                if !missing.is_empty() {
                    // The next hover finds it once it has loaded.
                    load_missing(&shell, &config.id, missing, cx);
                }
                link.map(|l| (config.id, l))
            });
            *target.borrow_mut() = link.clone();
            let Some((_, link)) = link else {
                return Ok(Vec::new());
            };
            let origin = lsp_types::Range {
                start: rope.offset_to_position(link.range.start),
                end: rope.offset_to_position(link.range.end),
            };
            Ok(vec![lsp_types::LocationLink {
                origin_selection_range: Some(origin),
                target_uri: "deadlock:object".parse()?,
                target_range: lsp_types::Range::default(),
                target_selection_range: lsp_types::Range::default(),
            }])
        })
    }
}

/// The object named at `offset` (its range in `rope`), and the schemas to load first.
fn link_at(
    shell: &WeakEntity<Shell>,
    config: &ConnectionConfig,
    current: Option<String>,
    rope: &Rope,
    offset: usize,
    cx: &App,
) -> (Option<complete::Link>, Vec<String>) {
    let mut missing = Vec::new();
    let Some(shell) = shell.upgrade() else {
        return (None, missing);
    };
    let tree = shell
        .read(cx)
        .tree
        .connections
        .iter()
        .find(|t| t.id == config.id);
    let catalog = Catalog {
        schemas: tree.and_then(|t| t.nodes.as_deref()).unwrap_or_default(),
        hidden: tree.map_or(&[], |t| &t.hidden),
        current: current.as_deref(),
    };
    let (text, start) = around(rope, offset);
    let link = complete::link_at(&text, offset - start, config.engine, &catalog, &mut missing).map(
        |mut l| {
            l.range = start + l.range.start..start + l.range.end;
            l
        },
    );
    (link, missing)
}

/// The click on a link: opens the object found while hovering, as the sidebar does.
fn open_target(
    shell: WeakEntity<Shell>,
    target: Target,
) -> gpui_kit::component::input::ShowDocumentHandler {
    Rc::new(move |_, window, cx| {
        let Some((conn, link)) = target.borrow().clone() else {
            return false;
        };
        let shell = shell.clone();
        // Outside the editor's update: opening a tab moves focus and redraws.
        window.defer(cx, move |window, cx| {
            let id = [conn, link.schema, link.group, link.name];
            let _ = shell.update(cx, |s, cx| s.open_node(&id, link.kind, window, cx));
        });
        true
    })
}
