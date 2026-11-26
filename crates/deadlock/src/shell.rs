//! A project window: toolbar, connections sidebar, tab strip, main area and status bar.

mod complete;
mod config;
mod delete;
mod diagram;
mod edit;
mod everywhere;
mod files;
mod history;
mod import;
mod keys;
mod objects;
mod schema;
mod sessions;
mod settings;
mod sidebar;

pub(crate) use history::history_cleared;
mod tabs;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deadlock_core::connection::keychain::{self, ssh_entry};
use deadlock_core::connection::{ConnectionConfig, Environment, new_id};
use deadlock_core::driver::{Driver, Link};
use deadlock_core::ssh::{SshAuth, TunnelError};

use crate::connection_form::{Failure, ask_trust, try_connect};
use crate::ui::Clickable as _;
use deadlock_core::project::{Pattern, Project};
use deadlock_core::schema::{Load, NodeId, Row, Tree};
use gpui_kit::component::Selectable;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Div, Entity, FocusHandle, FontWeight,
    Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, Render, RenderOnce, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Subscription, Task, TextRun,
    UniformListScrollHandle, Window, actions, black, canvas, checkerboard, div, fill, font, hsla,
    img, linear_color_stop, linear_gradient, pattern_slash, point, px, relative, size,
};

pub(crate) use schema::{SchemaNext, SchemaPick, SchemaPrev};
pub(crate) use sidebar::Cache;
use sidebar::{Nav, Search};

use crate::app::Db;
use crate::assets::{APP_LOGO, Icon};
use crate::connection_form::{ConnectionForm, FormEvent};
use crate::query::TxMode;
use crate::theme::{
    MONO_FONT, Palette, RADIUS, SIDEBAR_WIDTH, UI_FONT, row_height, small_size, status_bar_height,
    toolbar_height, ui_size,
};
use crate::ui::{Kind, button, button_icon, dot, icon_button};

actions!(
    shell,
    [
        Refresh,
        SearchObjects,
        SelectPrev,
        SelectNext,
        SelectFirst,
        SelectLast,
        ExpandNode,
        CollapseNode,
        ToggleNode,
        NewQuery,
        OpenFile,
        SaveFile,
        RunStatement,
        RunAll,
        StopRun,
        OpenConsole,
        FormatSql,
        CommitTx,
        ExplainPlan,
        ExplainAnalyze,
        ToggleHistory,
        OpenSettings,
        SearchEverywhere,
        EverywherePrev,
        EverywhereNext,
        ToggleFiles,
        FilterFiles,
        RenameFile,
        DeleteFile
    ]
);

/// The password dialog for a connection without a stored password.
struct Prompt {
    id: String,
    /// "Keep the password in the system keychain".
    keep: bool,
    /// Clear and focus the field on the next frame, which has the window.
    fresh: bool,
    /// The label of the SSH password or passphrase field, when the tunnel needs one.
    ssh: Option<&'static str>,
}

/// A connection's live state. Absent from the map: not connected.
enum Conn {
    Connecting,
    Connected(Arc<dyn Driver>),
    Failed(String),
}

/// Bounds are written this long after the last move or resize.
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// What the window does once its close checks pass.
enum CloseThen {
    Close,
    /// Shows this project in the window instead.
    Switch(PathBuf),
    /// Deletes the project's Deadlock data, then closes.
    Delete,
}

/// What the main area shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Front {
    None,
    Query(usize),
    Form,
    Sessions(usize),
    Config(usize),
    Diagram(usize),
    /// An object editor tab.
    Object(usize),
    Import,
    Settings,
}

pub struct Shell {
    project: Project,
    window: AnyWindowHandle,
    /// Open transactions were just dealt with: the close in progress goes on.
    tx_settled: bool,
    /// Pending table changes were just dealt with: the close in progress goes on.
    pending_settled: bool,
    /// Unapplied object edits were just discarded: the close in progress goes on.
    objects_settled: bool,
    tx_menu_open: bool,
    export_menu_open: bool,
    /// The toolbar's schema menu.
    schema_menu: schema::SchemaMenu,
    /// A sidebar row was clicked after the front tab last changed: the console opens
    /// on that row's connection and schema.
    sidebar_last: bool,
    focus: FocusHandle,
    save: Option<Task<()>>,
    conns: HashMap<String, Conn>,
    /// What happens once the close checks pass.
    close_then: CloseThen,
    /// Delete project is removing `.deadlock/`: nothing writes into it any more.
    deleted: bool,
    /// Passwords typed this session, for connections that keep none in the keychain.
    passwords: HashMap<String, String>,
    /// The New connection tab, while open.
    form: Option<(Entity<ConnectionForm>, Subscription)>,
    /// Editor tabs, left to right.
    tabs: Vec<(Entity<crate::query::QueryTab>, Subscription)>,
    /// Sessions and locks tabs, after the editor tabs.
    sessions: Vec<(Entity<crate::sessions::SessionsTab>, Subscription)>,
    /// Server configuration tabs, after the Sessions and locks tabs.
    configs: Vec<(Entity<crate::config::ConfigTab>, Subscription)>,
    /// Schema diagram tabs, one per connection, after the configuration tabs.
    diagrams: Vec<(Entity<crate::diagram::DiagramTab>, Subscription)>,
    /// Object editor tabs, one per object, after the diagram tabs.
    objects: Vec<(Entity<crate::object::ObjectTab>, Subscription)>,
    /// Object tabs from `state.json`, waiting for their connection to connect.
    restored_objects: Vec<(String, deadlock_core::objects::ObjectRef)>,
    /// Sessions waiting on a lock, per connection, from the periodic check.
    waiting: HashMap<String, usize>,
    _watch: Task<()>,
    front: Front,
    /// The last `Untitled-<n>` number given out.
    untitled: usize,
    /// The window may close: unsaved files were dealt with.
    may_close: bool,
    switcher_open: bool,
    tree: Tree,
    /// The tree as the sidebar draws it, rebuilt on every change.
    rows: Vec<Row>,
    /// Matching objects while searching.
    found: usize,
    query_active: bool,
    /// Nodes loaded from the server this session; the rest came from the cache.
    fresh: HashSet<NodeId>,
    selected: Option<NodeId>,
    /// A table just created: selected once its schema reloads with it.
    reveal: Option<NodeId>,
    list: UniformListScrollHandle,
    tree_focus: FocusHandle,
    search: Option<Search>,
    /// The Search everywhere palette, while open.
    everywhere: Option<everywhere::Everywhere>,
    shift: everywhere::DoubleShift,
    prompt: Option<Prompt>,
    prompt_input: Entity<InputState>,
    prompt_ssh: Entity<InputState>,
    /// Connections waiting on a typed password, and whether to keep it in the keychain.
    prompted: HashMap<String, bool>,
    /// Move focus to the tree on the next frame.
    focus_tree: bool,
    /// The Redis key browser's picks, searches, jobs and New key dialog.
    keys: keys::Keys,
    /// The Import tab, while open.
    import: Option<(Entity<crate::import::ImportTab>, Subscription)>,
    /// The History panel, once opened.
    history: Option<(Entity<crate::history::HistoryPanel>, Subscription)>,
    /// The Settings tab, while open.
    settings_tab: Option<Entity<crate::settings_tab::SettingsTab>>,
    /// The Files panel's tree, filter, rename and watcher.
    files: files::Files,
    _settings: Subscription,
    _bounds: Subscription,
    _prompt: Subscription,
}

impl Shell {
    pub fn new(project: Project, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let bounds = cx.observe_window_bounds(window, |this, window, cx| {
            this.project.state.window = Some(crate::app::to_state(window.window_bounds()));
            this.schedule_save(cx);
        });
        // Closing the window drops the view; flush a save that is still waiting.
        cx.on_release(|this, cx| {
            if this.deleted {
                return;
            }
            this.flush_consoles(cx);
            this.save_now(cx);
            // Drivers close their sockets on the runtime, not the UI thread.
            let conns = std::mem::take(&mut this.conns);
            drop(cx.global::<Db>().0.spawn(async move { drop(conns) }));
        })
        .detach();
        let prompt_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Enter password")
        });
        let prompt_ssh = cx.new(|cx| InputState::new(window, cx).masked(true));
        let prompt = cx.subscribe(&prompt_input, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::PressEnter { .. }) {
                this.submit_password(cx);
            }
        });
        let mut this = Self {
            project,
            window: window.window_handle(),
            tx_settled: false,
            pending_settled: false,
            objects_settled: false,
            tx_menu_open: false,
            export_menu_open: false,
            schema_menu: schema::SchemaMenu::new(window, cx),
            sidebar_last: false,
            focus,
            save: None,
            conns: HashMap::new(),
            close_then: CloseThen::Close,
            deleted: false,
            passwords: HashMap::new(),
            form: None,
            tabs: Vec::new(),
            sessions: Vec::new(),
            configs: Vec::new(),
            diagrams: Vec::new(),
            objects: Vec::new(),
            restored_objects: Vec::new(),
            waiting: HashMap::new(),
            _watch: Self::watch_waits(cx),
            front: Front::None,
            untitled: 0,
            may_close: false,
            switcher_open: false,
            tree: Tree::default(),
            rows: Vec::new(),
            found: 0,
            query_active: false,
            fresh: HashSet::new(),
            selected: None,
            reveal: None,
            list: UniformListScrollHandle::new(),
            tree_focus: cx.focus_handle(),
            search: None,
            everywhere: None,
            shift: everywhere::DoubleShift::default(),
            prompt: None,
            prompt_input,
            prompt_ssh,
            prompted: HashMap::new(),
            focus_tree: false,
            keys: keys::Keys::default(),
            history: None,
            import: None,
            settings_tab: None,
            files: files::Files::new(cx),
            _settings: cx
                .observe_global_in::<crate::settings::SettingsState>(window, |this, window, cx| {
                    this.apply_settings(window, cx)
                }),
            _bounds: bounds,
            _prompt: prompt,
        };
        this.tree_connections();
        this.rows = this.tree.rows();
        this.load_cache(cx);
        this.restore_tabs(window, cx);
        if this.project.state.files_open {
            this.watch_files(cx);
        }
        let shell = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            shell
                .update(cx, |this, cx| this.close(window, cx))
                .unwrap_or(true)
        });
        this
    }

    /// The close button and Quit: closes the window rather than switching it.
    pub(crate) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.close_then = CloseThen::Close;
        self.should_close(window, cx)
    }

    /// Ends the close checks: switches the window to the chosen project, deletes the
    /// project, or closes the window.
    fn finish_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = match std::mem::replace(&mut self.close_then, CloseThen::Close) {
            CloseThen::Close => return window.remove_window(),
            CloseThen::Delete => return self.finish_delete(cx),
            CloseThen::Switch(path) => path,
        };
        let handle = self.window;
        // Deferred: replacing the window's root cannot happen inside its own update.
        cx.defer(move |cx| {
            if let Err(e) = crate::app::switch_project(handle, &path, cx) {
                eprintln!("deadlock: cannot open {}: {e:#}", path.display());
            }
        });
    }

    /// Opens a project picked in the project menu: in this window, or in a new one.
    pub(crate) fn pick_project(
        &mut self,
        path: PathBuf,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switcher_open = false;
        cx.notify();
        let open = cx
            .global::<crate::app::Registry>()
            .projects
            .contains_key(&path);
        if new_window || open {
            // Deferred: focusing a window cannot happen inside its own update.
            cx.defer(move |cx| {
                if let Err(e) = crate::app::open_project(&path, false, cx) {
                    eprintln!("deadlock: cannot open {}: {e:#}", path.display());
                }
            });
            return;
        }
        self.close_then = CloseThen::Switch(path);
        if self.should_close(window, cx) {
            self.finish_close(window, cx);
        }
    }

    /// Asks about open transactions, then unsaved files, before the window closes.
    pub(crate) fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.may_close {
            return true;
        }
        let what = match self.close_then {
            CloseThen::Switch(_) => "switching projects",
            CloseThen::Close | CloseThen::Delete => "closing the window",
        };
        let tabs: Vec<_> = self.tabs.iter().map(|(t, _)| t.clone()).collect();
        if tabs.iter().any(|t| t.read(cx).pending_changes(cx) > 0)
            && !std::mem::take(&mut self.pending_settled)
        {
            self.settle_pending(
                tabs,
                format!("Submit the pending changes before {what}?"),
                cx,
                |this, window, cx| {
                    this.pending_settled = true;
                    if this.should_close(window, cx) {
                        this.finish_close(window, cx);
                    }
                },
            );
            return false;
        }
        self.pending_settled = false;
        if !std::mem::take(&mut self.tx_settled) {
            let tabs = self.tabs.iter().map(|(t, _)| t.clone()).collect();
            self.settle(
                tabs,
                format!("Commit or roll back before {what}?"),
                cx,
                |this, window, cx| {
                    this.tx_settled = true;
                    if this.should_close(window, cx) {
                        this.finish_close(window, cx);
                    }
                },
            );
            return false;
        }
        let mut objects = self.dirty_objects(cx);
        objects.extend(self.unapplied_tables(cx));
        if !objects.is_empty() && !std::mem::take(&mut self.objects_settled) {
            let answer = crate::dialog::ask(
                window,
                cx,
                crate::dialog::Ask::new(
                    "Discard the unapplied changes before closing?",
                    &["Discard", "Cancel"],
                )
                .text(format!(
                    "{} {} edits that were not applied.",
                    objects.join(", "),
                    if objects.len() == 1 { "has" } else { "have" }
                )),
            );
            let handle = window.window_handle();
            cx.spawn(async move |this, cx| {
                if answer.await != Some(0) {
                    return;
                }
                let _ = cx.update_window(handle, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.objects_settled = true;
                        if this.should_close(window, cx) {
                            this.finish_close(window, cx);
                        }
                    })
                });
            })
            .detach();
            return false;
        }
        self.objects_settled = false;
        let dirty = self.dirty_files(cx);
        if dirty.is_empty() {
            return true;
        }
        let n = dirty.len();
        let what = if n == 1 {
            "1 file".to_owned()
        } else {
            format!("{n} files")
        };
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Save changes to {what} before closing?"),
                &["Save", "Don't Save", "Cancel"],
            ),
        );
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let Some(answer) = answer.await else { return };
            let close = this
                .update(cx, |this, cx| match answer {
                    0 => this.save_files(&dirty, cx),
                    1 => true,
                    _ => false,
                })
                .unwrap_or(false);
            if close {
                let _ = this.update(cx, |this, _| this.may_close = true);
                let _ = cx.update_window(handle, |_, window, cx| {
                    this.update(cx, |this, cx| this.finish_close(window, cx))
                });
            }
        })
        .detach();
        false
    }

    /// Writes `state.json` shortly, once moves and keystrokes settle.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| this.save_now(cx));
        }));
    }

    fn config(&self, id: &str) -> Option<&ConnectionConfig> {
        self.project.config.connections.iter().find(|c| c.id == id)
    }

    /// Opens the New connection tab, or keeps the one that is open.
    pub(crate) fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form(None, false, window, cx);
    }

    /// Opens the form on saved connection `id`: an edit, or a new copy (`editing` off).
    pub(crate) fn edit_connection(
        &mut self,
        id: &str,
        editing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.config(id).cloned() else {
            return;
        };
        self.open_form(Some(config), editing, window, cx);
    }

    /// The single form tab: kept when it already shows the same target, else replaced.
    fn open_form(
        &mut self,
        source: Option<ConnectionConfig>,
        editing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = source.as_ref().filter(|_| editing).map(|c| c.id.clone());
        let same = self.form.as_ref().is_some_and(|(f, _)| {
            let open = f.read(cx).editing().map(|c| &c.id);
            match (&source, &target) {
                (None, _) => open.is_none(),
                (Some(_), Some(id)) => open == Some(id),
                // A duplicate always starts from the connection picked.
                (Some(_), None) => false,
            }
        });
        if !same {
            let form = cx.new(|cx| ConnectionForm::new(source.as_ref(), editing, window, cx));
            let sub = cx.subscribe_in(&form, window, Self::on_form);
            self.form = Some((form.clone(), sub));
            if let Some(id) = target {
                self.provide_known_password(&id, form, cx);
            }
        }
        self.front = Front::Form;
        cx.notify();
    }

    /// Hands the edit form the stored password, so Test connection works unchanged.
    fn provide_known_password(
        &mut self,
        id: &str,
        form: Entity<ConnectionForm>,
        cx: &mut Context<Self>,
    ) {
        if let Some(p) = self.passwords.get(id) {
            form.update(cx, |f, _| f.known_password = Some(p.clone()));
            return;
        }
        if !self.config(id).is_some_and(|c| c.keychain) {
            return;
        }
        let (root, id) = (self.project.root().to_owned(), id.to_owned());
        let rx = cx
            .global::<Db>()
            .0
            .spawn_blocking(move || keychain::load(&root, &id));
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(p))) = rx.await {
                form.update(cx, |f, _| f.known_password = Some(p));
            }
        })
        .detach();
    }

    fn on_form(
        &mut self,
        _: &Entity<ConnectionForm>,
        event: &FormEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let FormEvent::Saved {
            config,
            password,
            ssh_secret,
        } = event
        {
            let (config, password, ssh_secret) =
                ((**config).clone(), password.clone(), ssh_secret.clone());
            return self.save_edit(config, password, ssh_secret, window, cx);
        }
        if let FormEvent::Connected {
            config,
            password,
            ssh_secret,
            driver,
        } = event
        {
            let mut config = (**config).clone();
            config.id = new_id(&config.name, &self.project.config.connections);
            let id = config.id.clone();
            if config.keychain {
                let (root, id, pw) = (self.project.root().to_owned(), id.clone(), password.clone());
                let ssh_secret = ssh_secret.clone();
                drop(cx.global::<Db>().0.spawn_blocking(move || {
                    if let Err(e) = keychain::save(&root, &id, &pw) {
                        eprintln!("deadlock: keychain: {e}");
                    }
                    if let Some(s) = ssh_secret
                        && let Err(e) = keychain::save(&root, &ssh_entry(&id), &s)
                    {
                        eprintln!("deadlock: keychain: {e}");
                    }
                }));
            }
            self.passwords.insert(id.clone(), password.clone());
            self.project.config.connections.push(config);
            if let Err(e) = self.project.save_config() {
                eprintln!("deadlock: {e}");
            }
            self.conns
                .insert(id.clone(), Conn::Connected(Arc::clone(driver)));
            self.follow_link(&id, cx);
            self.tree_connections();
            self.tree.expanded.insert(vec![id.clone()]);
            self.selected = Some(vec![id.clone()]);
            self.reload(&id, cx);
        }
        self.close_form(cx);
    }

    fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        if self.front == Front::Form {
            self.front = match self.tabs.len() {
                0 => Front::None,
                n => Front::Query(n - 1),
            };
        }
        cx.notify();
    }

    /// Moves the front file tab to connection `id`, once its open transaction is dealt
    /// with.
    fn activate(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(tab) = self.moving_front(id, cx) {
            let question = format!(
                "Commit or roll back before moving {} to another connection?",
                self.tab_title(tab.read(cx))
            );
            let id = id.to_owned();
            self.settle(vec![tab], question, cx, move |this, _, cx| {
                this.rebind_front(&id, cx);
                cx.notify();
            });
            cx.notify();
            return;
        }
        self.rebind_front(id, cx);
        cx.notify();
    }

    fn connect(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(config) = self.config(id).cloned() else {
            return;
        };
        self.conns.insert(id.to_owned(), Conn::Connecting);
        let key = vec![id.to_owned()];
        if self.tree.children(&key).is_none() {
            self.tree.status.insert(key, Load::Loading);
        }
        self.rebuild(cx);
        let rt = Arc::clone(&cx.global::<Db>().0);
        let typed = self.passwords.get(id).cloned();
        let root = self.project.root().to_owned();
        let id = id.to_owned();
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let load = |entry: String| {
                let root = root.clone();
                rt.spawn_blocking(move || keychain::load(&root, &entry))
            };
            let password = match typed {
                Some(p) => Ok(Some(p)),
                None if config.keychain => match load(config.id.clone()).await {
                    Ok(Ok(p)) => Ok(p),
                    Ok(Err(e)) => Err(format!("keychain: {e}")),
                    Err(_) => Err("keychain task failed".into()),
                },
                None => Ok(None),
            };
            // The tunnel's password or passphrase: given this session, else the keychain.
            let ssh = &config.ssh;
            let mut ssh_missing = None;
            if config.ssh_tunnel
                && ssh.auth != SshAuth::Agent
                && deadlock_core::ssh::secret(ssh).is_none()
            {
                let saved = if config.keychain {
                    load(ssh_entry(&config.id))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten()
                } else {
                    None
                };
                match saved {
                    Some(s) => deadlock_core::ssh::remember(ssh, s),
                    None if ssh.auth == SshAuth::Password => ssh_missing = Some("SSH password"),
                    None => ssh_missing = Some("Key passphrase (empty for none)"),
                }
            }
            let result = match password {
                Ok(Some(password)) if ssh_missing.is_none() => rt
                    .spawn(
                        async move { try_connect(&config, &password).await.map(|d| (d, password)) },
                    )
                    .await
                    .unwrap_or_else(|_| Err(Failure::Other("connection task failed".into()))),
                Ok(_) => {
                    let _ = this.update(cx, |this, cx| this.ask_password(&id, ssh_missing, cx));
                    return;
                }
                Err(e) => Err(Failure::Other(e)),
            };
            let result = match result {
                Ok(r) => Ok(r),
                Err(Failure::Other(e)) => Err(e),
                // Thin cannot sign in to this account: offer the Thick driver in place.
                Err(Failure::OldVerifier(e)) => {
                    let asked = cx.update_window(window, |_, window, cx| {
                        crate::dialog::ask(
                            window,
                            cx,
                            crate::dialog::Ask::new(
                                "This account uses an old password type",
                                &["Switch this connection to the Thick driver", "Cancel"],
                            )
                            .text(e.clone()),
                        )
                    });
                    let switch = match asked {
                        Ok(a) => a.await == Some(0),
                        Err(_) => return,
                    };
                    if switch {
                        let _ = this.update(cx, |this, cx| this.switch_to_thick(&id, cx));
                        return;
                    }
                    Err(e)
                }
                Err(Failure::UnknownHost(key)) => {
                    let asked =
                        cx.update_window(window, |_, window, cx| ask_trust(&key, window, cx));
                    let answer = match asked {
                        Ok(a) => a.await,
                        Err(_) => return,
                    };
                    match answer {
                        Ok(true) => {
                            let _ = this.update(cx, |this, cx| this.connect(&id, cx));
                            return;
                        }
                        Ok(false) => Err(TunnelError::UnknownHost(key).to_string()),
                        Err(e) => Err(e),
                    }
                }
            };
            let _ = this.update(cx, |this, cx| this.connected(id, result, cx));
        })
        .detach();
    }

    fn connected(
        &mut self,
        id: String,
        result: Result<(Arc<dyn Driver>, String), String>,
        cx: &mut Context<Self>,
    ) {
        if self.config(&id).is_none() {
            // Deleted while it connected.
            if let Ok((driver, _)) = result {
                drop(cx.global::<Db>().0.spawn(async move { drop(driver) }));
            }
            return;
        }
        let keep = self.prompted.remove(&id);
        match result {
            Ok((driver, password)) => {
                // Kept for the editor tabs' own sessions.
                self.passwords.insert(id.clone(), password);
                self.conns.insert(id.clone(), Conn::Connected(driver));
                self.follow_link(&id, cx);
                self.resume_tabs(&id, None, cx);
                self.reopen_objects(&id, cx);
                self.tree.status.remove(&vec![id.clone()]);
                if let Some(keep) = keep {
                    self.remember_password(&id, keep, cx);
                }
                self.reload(&id, cx);
            }
            Err(e) => {
                // A typed password that failed is asked for again next time.
                if keep.is_some() {
                    self.passwords.remove(&id);
                }
                self.tree
                    .status
                    .insert(vec![id.clone()], Load::Failed(e.clone()));
                self.resume_tabs(&id, Some(&e), cx);
                self.conns.insert(id, Conn::Failed(e));
            }
        }
        self.connect_restored(cx);
        // Search everywhere may be waiting for this server.
        self.run_everywhere(cx);
        self.rebuild(cx);
    }

    /// Follows connection `id`'s reconnects: the dot and status bar show them, and one
    /// that failed for good turns the connection failed, so connecting again works.
    fn follow_link(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(Conn::Connected(d)) = self.conns.get(id) else {
            return;
        };
        let Some(mut rx) = d.link() else {
            return;
        };
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            while rx.changed().await.is_ok() {
                let link = rx.borrow_and_update().clone();
                let current = this.update(cx, |this, cx| {
                    // Only while this driver is still the connection's.
                    let current = matches!(
                        this.conns.get(&id),
                        Some(Conn::Connected(d)) if d.link().is_some_and(|l| l.same_channel(&rx))
                    );
                    if current {
                        this.link_changed(&id, link, cx);
                    }
                    current
                });
                if !matches!(current, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn link_changed(&mut self, id: &str, link: Link, cx: &mut Context<Self>) {
        let Link::Lost(e) = link else {
            cx.notify();
            return;
        };
        let e = format!("the connection was lost, and reconnecting failed: {e}");
        self.tree
            .status
            .insert(vec![id.to_owned()], Load::Failed(e.clone()));
        if let Some(Conn::Connected(d)) = self.conns.insert(id.to_owned(), Conn::Failed(e)) {
            // The driver closes its socket on the runtime, not the UI thread.
            drop(cx.global::<Db>().0.spawn(async move { drop(d) }));
        }
        self.rebuild(cx);
    }

    /// Saves connection `id` with the Thick Oracle driver and connects again.
    fn switch_to_thick(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(c) = self
            .project
            .config
            .connections
            .iter_mut()
            .find(|c| c.id == id)
        {
            c.thick = true;
        }
        if let Err(e) = self.project.save_config() {
            eprintln!("deadlock: {e}");
        }
        // Tabs open their sessions from the saved settings, so they follow.
        self.connect(id, cx);
    }

    /// No password in the keychain or typed this session: ask for one.
    fn ask_password(&mut self, id: &str, ssh: Option<&'static str>, cx: &mut Context<Self>) {
        self.conns.remove(id);
        self.tree.status.remove(&vec![id.to_owned()]);
        let keep = self.config(id).is_some_and(|c| c.keychain);
        self.prompt = Some(Prompt {
            id: id.to_owned(),
            keep,
            fresh: true,
            ssh,
        });
        self.rebuild(cx);
    }

    fn submit_password(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let password = self.prompt_input.read(cx).value().to_string();
        self.passwords.insert(prompt.id.clone(), password);
        if prompt.ssh.is_some()
            && let Some(c) = self.config(&prompt.id)
        {
            let secret = self.prompt_ssh.read(cx).value().to_string();
            deadlock_core::ssh::remember(&c.ssh, secret);
        }
        self.prompted.insert(prompt.id.clone(), prompt.keep);
        self.focus_tree = true;
        self.connect(&prompt.id, cx);
    }

    fn cancel_prompt(&mut self, cx: &mut Context<Self>) {
        if let Some(prompt) = self.prompt.take() {
            self.resume_tabs(&prompt.id, Some("no password was given"), cx);
        }
        self.focus_tree = true;
        cx.notify();
    }

    /// After a typed password worked: stores it when asked to, and records the choice.
    fn remember_password(&mut self, id: &str, keep: bool, cx: &mut Context<Self>) {
        if keep && let Some(password) = self.passwords.get(id).cloned() {
            let (root, id) = (self.project.root().to_owned(), id.to_owned());
            let ssh_secret = self
                .config(&id)
                .filter(|c| c.ssh_tunnel && c.ssh.auth != SshAuth::Agent)
                .and_then(|c| deadlock_core::ssh::secret(&c.ssh));
            drop(cx.global::<Db>().0.spawn_blocking(move || {
                if let Err(e) = keychain::save(&root, &id, &password) {
                    eprintln!("deadlock: keychain: {e}");
                }
                if let Some(s) = ssh_secret
                    && let Err(e) = keychain::save(&root, &ssh_entry(&id), &s)
                {
                    eprintln!("deadlock: keychain: {e}");
                }
            }));
        }
        let Some(config) = self
            .project
            .config
            .connections
            .iter_mut()
            .find(|c| c.id == id)
        else {
            return;
        };
        if config.keychain != keep {
            config.keychain = keep;
            if let Err(e) = self.project.save_config() {
                eprintln!("deadlock: {e}");
            }
        }
    }

    /// `MySQL 8.4`, `connecting…` or `not connected`, as the sidebar shows it.
    fn state_label(&self, id: &str) -> String {
        match self.conns.get(id) {
            Some(Conn::Connected(d)) if reconnecting(d) => "reconnecting…".into(),
            Some(Conn::Connected(d)) => short_version(d.server_version()),
            Some(Conn::Connecting) => "connecting…".into(),
            Some(Conn::Failed(_)) => "failed".into(),
            None => "not connected".into(),
        }
    }

    fn save_now(&mut self, cx: &App) {
        self.save = None;
        if self.deleted {
            return;
        }
        self.sync_tabs(cx);
        if let Err(e) = self.project.save_state() {
            eprintln!("deadlock: {e}");
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        if let Some(prompt) = self.prompt.as_mut().filter(|pr| pr.fresh) {
            prompt.fresh = false;
            self.prompt_input.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.focus(window, cx);
            });
            self.prompt_ssh
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
        if std::mem::take(&mut self.focus_tree) {
            window.focus(&self.tree_focus, cx);
        }
        let history = self.project.state.history_open.then(|| {
            let panel = self.history_panel(window, cx);
            let current = self.active(cx);
            panel.update(cx, |p, cx| p.set_current(current, cx));
            panel
        });
        let history_width = px(self.project.state.history_width.unwrap_or(history::WIDTH));
        self.focus_rename(window, cx);
        let files_open = self.project.state.files_open;
        let files_panel = files_open.then(|| files::panel(self, cx));
        let files_width = px(self.project.state.files_width.unwrap_or(files::WIDTH));
        let shell = cx.entity().downgrade();
        div()
            .key_context("Shell")
            .track_focus(&self.focus)
            .on_modifiers_changed(cx.listener(Self::on_modifiers))
            .capture_key_down(cx.listener(|this, e: &gpui_kit::KeyDownEvent, _, _| {
                if !e.keystroke.is_modifier_key() {
                    this.shift.interrupt();
                }
            }))
            .capture_any_mouse_down(cx.listener(|this, _, _, _| this.shift.interrupt()))
            .on_action(cx.listener(|this, _: &SearchEverywhere, window, cx| {
                this.open_everywhere(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(
                cx.listener(|this, _: &SearchObjects, window, cx| this.open_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.on_nav(Nav::Prev, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.on_nav(Nav::Next, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.on_nav(Nav::First, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.on_nav(Nav::Last, cx)))
            .on_action(cx.listener(|this, _: &ExpandNode, _, cx| this.on_nav(Nav::Expand, cx)))
            .on_action(cx.listener(|this, _: &CollapseNode, _, cx| this.on_nav(Nav::Collapse, cx)))
            .on_action(cx.listener(|this, _: &ToggleNode, window, cx| {
                if !this.open_selected_table(window, cx) {
                    this.on_nav(Nav::Toggle, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewQuery, window, cx| this.new_query(window, cx)))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| this.open_file(window, cx)))
            .on_action(cx.listener(|this, _: &SaveFile, window, cx| {
                // In an object editor, Save applies.
                if let Front::Object(i) = this.front
                    && let Some((tab, _)) = this.objects.get(i)
                {
                    tab.clone().update(cx, |t, cx| t.apply(window, cx));
                } else {
                    this.save_front(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RunStatement, _, cx| {
                this.run_front(crate::query::Scope::Caret, cx)
            }))
            .on_action(
                cx.listener(|this, _: &RunAll, _, cx| this.run_front(crate::query::Scope::All, cx)),
            )
            .on_action(cx.listener(|this, _: &StopRun, _, cx| this.stop_front(cx)))
            .on_action(
                cx.listener(|this, _: &OpenConsole, window, cx| this.open_console(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FormatSql, window, cx| this.format_front(window, cx)))
            .on_action(cx.listener(|this, _: &CommitTx, _, cx| this.end_front_tx(true, cx)))
            .on_action(cx.listener(|this, _: &ExplainPlan, _, cx| this.explain_front(false, cx)))
            .on_action(cx.listener(|this, _: &ExplainAnalyze, _, cx| this.explain_front(true, cx)))
            .on_action(
                cx.listener(|this, _: &ToggleHistory, window, cx| this.toggle_history(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleFiles, window, cx| this.toggle_files(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FilterFiles, window, cx| this.filter_files(window, cx)),
            )
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .text_color(p.tx)
            .child(toolbar(self, window, cx))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("shell")
                        .child(
                            resizable_panel()
                                .size(SIDEBAR_WIDTH)
                                .size_range(px(180.)..px(560.))
                                .fixed()
                                .child(sidebar::sidebar(self, cx)),
                        )
                        .child(resizable_panel().child(main_area(self, cx)))
                        .child(
                            resizable_panel()
                                .visible(history.is_some())
                                .size(history_width)
                                .size_range(px(320.)..px(900.))
                                .fixed()
                                .children(history),
                        )
                        .child(
                            resizable_panel()
                                .visible(files_open)
                                .size(files_width)
                                .size_range(px(200.)..px(480.))
                                .fixed()
                                .children(files_panel),
                        )
                        .on_resize(move |state, _, cx| {
                            let sizes = state.read(cx).sizes().clone();
                            let _ = shell.update(cx, |this, cx| {
                                let s = &mut this.project.state;
                                if s.history_open
                                    && let Some(&w) = sizes.get(2)
                                {
                                    s.history_width = Some(f32::from(w));
                                }
                                if s.files_open
                                    && let Some(&w) = sizes.get(3)
                                {
                                    s.files_width = Some(f32::from(w));
                                }
                                this.schedule_save(cx);
                            });
                        }),
                ),
            )
            .child(status_bar(self, &p, cx))
            .children(everywhere::palette(self, cx))
            .when_some(self.prompt.as_ref(), |d, prompt| {
                d.child(password_prompt(self, prompt, cx))
            })
            .children(keys::new_key_dialog(self, window, cx))
    }
}

/// A small dialog over the window: password field, keychain checkbox, Cancel / Connect.
fn password_prompt(shell: &Shell, prompt: &Prompt, cx: &mut Context<Shell>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let config = shell.config(&prompt.id);
    let name = config.map_or(String::new(), |c| c.name.clone());
    let address = config.map(ConnectionConfig::address).unwrap_or_default();
    div()
        .id("password-backdrop")
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(hsla(0., 0., 0., 0.3))
        .on_action(cx.listener(|this, _: &input::Escape, _, cx| this.cancel_prompt(cx)))
        .child(
            div()
                .w(px(360.))
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .bg(p.bg)
                .border_1()
                .border_color(p.line)
                .rounded(RADIUS)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("Connect to {name}")),
                        )
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_size(small_size())
                                .text_color(p.tx3)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(address),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .font_weight(FontWeight::MEDIUM)
                        .child("Password")
                        .child(
                            div()
                                .font_weight(FontWeight::NORMAL)
                                .child(Styled::h(Input::new(&shell.prompt_input), px(32.))),
                        ),
                )
                .when_some(prompt.ssh, |d, label| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(label)
                            .child(
                                div()
                                    .font_weight(FontWeight::NORMAL)
                                    .child(Styled::h(Input::new(&shell.prompt_ssh), px(32.))),
                            ),
                    )
                })
                .child(
                    Checkbox::new("prompt-keychain")
                        .checked(prompt.keep)
                        .label("Keep the password in the system keychain")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            if let Some(p) = &mut this.prompt {
                                p.keep = *checked;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            button("prompt-cancel", Kind::Outline, true, "Cancel (Esc)", cx)
                                .child("Cancel")
                                .clickable(cx.listener(|this, _, _, cx| this.cancel_prompt(cx))),
                        )
                        .child(
                            button("prompt-connect", Kind::Primary, true, "Connect (Enter)", cx)
                                .child("Connect")
                                .clickable(cx.listener(|this, _, _, cx| this.submit_password(cx))),
                        ),
                ),
        )
}

fn separator(p: &Palette) -> impl IntoElement {
    div().w(px(1.)).h(px(20.)).mx(px(4.)).flex_none().bg(p.line)
}

/// `MySQL 8.4.3` → `MySQL 8.4`, as the sidebar shows versions.
fn short_version(full: &str) -> String {
    let (product, number) = full.rsplit_once(' ').unwrap_or(("", full));
    let short: Vec<&str> = number.splitn(3, '.').take(2).collect();
    format!("{product} {}", short.join(".")).trim().to_owned()
}

/// The connection lost its session and is opening a new one.
fn reconnecting(d: &Arc<dyn Driver>) -> bool {
    d.link().is_some_and(|l| *l.borrow() == Link::Reconnecting)
}

fn state_dot(conn: Option<&Conn>, p: &Palette) -> Hsla {
    match conn {
        Some(Conn::Connected(d)) if reconnecting(d) => p.warn,
        Some(Conn::Connected(_)) => p.ok,
        Some(Conn::Failed(_)) => p.danger,
        Some(Conn::Connecting) | None => p.tx3,
    }
}

/// A button that opens a menu; [`Popover`] needs its trigger to know whether the menu
/// is open. The button is made without a tooltip: the third field is the tooltip,
/// shown only while the menu is closed so it never covers the menu.
#[derive(IntoElement)]
pub(crate) struct Trigger(
    pub(crate) Stateful<Div>,
    pub(crate) bool,
    pub(crate) &'static str,
);

impl Selectable for Trigger {
    fn selected(mut self, selected: bool) -> Self {
        self.1 = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.1
    }
}

impl RenderOnce for Trigger {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Self(button, open, tip) = self;
        button.when(!open && !tip.is_empty(), |b| {
            b.tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
            })
        })
    }
}

/// The project's name; opens the list of recent projects. A pick switches this window
/// to it; Ctrl+click (Cmd+click) or the row's button opens it in a new window.
fn switcher(shell: &Shell, cx: &mut Context<Shell>) -> impl IntoElement + use<> {
    let p = *cx.global::<Palette>();
    let trigger = button("project", Kind::Outline, true, "", cx)
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child(SharedString::from(shell.project.name())),
        )
        .child(button_icon(
            "project",
            Icon::ChevronDown,
            Kind::Outline,
            true,
            cx,
        ));

    let root = shell.project.root();
    let items: Vec<(PathBuf, String, Option<Hsla>, bool)> = cx
        .global::<crate::app::Registry>()
        .recent
        .entries()
        .iter()
        .map(|e| {
            let color = e.color.as_deref().and_then(crate::color::parse_hex);
            (e.path.clone(), e.label(), color, e.path == root)
        })
        .collect();
    let modifier = if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    };
    let new_window = format!("Open in a new window ({modifier}+click)");
    let new_window_tip = SharedString::from(format!(
        "Pick or make a folder. {modifier}+click opens it in a new window."
    ));
    let window_handle = shell.window;
    let this = cx.entity().downgrade();
    Popover::new("switcher")
        .p(px(4.))
        .min_w(px(240.))
        .open(shell.switcher_open)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.switcher_open = *open;
            cx.notify();
        }))
        .trigger(Trigger(trigger, shell.switcher_open, "Switch project"))
        .content(move |_, _, cx| {
            let rows = items
                .iter()
                .enumerate()
                .map(|(ix, (path, name, color, current))| {
                    let place = crate::welcome::tilde(path);
                    let (this, pick) = (this.clone(), path.clone());
                    let (that, path) = (this.clone(), path.clone());
                    let open_new = icon_button(
                        "new-window",
                        Icon::Export,
                        None,
                        Kind::Ghost,
                        true,
                        new_window.clone(),
                        cx,
                    )
                    .clickable(move |_, window, cx| {
                        cx.stop_propagation();
                        let _ = that.update(cx, |this, cx| {
                            this.pick_project(path.clone(), true, window, cx);
                        });
                    });
                    div()
                        .id(("switch", ix))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .h(row_height())
                        .pl(px(8.))
                        .rounded(RADIUS)
                        .whitespace_nowrap()
                        .when(*current, |d| d.bg(p.sel).font_weight(FontWeight::MEDIUM))
                        .when(!*current, |d| d.hover(|s| s.bg(p.hover)))
                        .clickable(move |e, window, cx| {
                            let new = e.modifiers().control || e.modifiers().platform;
                            let _ = this.update(cx, |this, cx| {
                                this.pick_project(pick.clone(), new, window, cx);
                            });
                        })
                        // Every row keeps the dot's place, so the names line up.
                        .child(dot(color.unwrap_or_else(gpui_kit::transparent_black)))
                        .child(SharedString::from(name.clone()))
                        .child(type_label(&place, &p))
                        .child(open_new)
                });
            let action = |id: &'static str, icon: Icon, label: &'static str| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(row_height())
                    .px(px(8.))
                    .rounded(RADIUS)
                    .whitespace_nowrap()
                    .hover(|s| s.bg(p.hover))
                    .child(crate::assets::icon(icon, p.tx2))
                    .child(label)
            };
            let (that, handle) = (this.clone(), window_handle);
            let tip = new_window_tip.clone();
            let new_project = action("new-project", Icon::Plus, "New project…")
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
                .clickable(move |e, _, cx| {
                    let new = e.modifiers().control || e.modifiers().platform;
                    let _ = that.update(cx, |this, cx| {
                        this.switcher_open = false;
                        cx.notify();
                    });
                    crate::app::new_project(handle, new, cx);
                });
            let that = this.clone();
            let settings = action("project-settings", Icon::Config, "Project settings…").clickable(
                move |_, window, cx| {
                    let _ = that.update(cx, |this, cx| this.open_project_settings(window, cx));
                },
            );
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .when(items.is_empty(), |d| {
                    d.child(
                        div()
                            .p(px(8.))
                            .text_color(p.tx3)
                            .child("No recent projects."),
                    )
                })
                .children(rows)
                .child(div().my(px(3.)).h(px(1.)).bg(p.line))
                .child(new_project)
                .child(settings)
        })
}

/// The design's `.ty`: right-aligned 11.5 px mono in the third text color.
fn type_label(text: &str, p: &Palette) -> Div {
    div()
        .ml_auto()
        .pl(px(8.))
        .font_family(MONO_FONT)
        .text_size(px(11.5))
        .text_color(p.tx3)
        .child(SharedString::from(text.to_owned()))
}

/// A toolbar entry, left to right.
enum Item {
    Tool {
        id: &'static str,
        icon: Icon,
        label: Option<&'static str>,
        kind: Kind,
        enabled: bool,
        tip: SharedString,
    },
    Separator,
    TxMode,
    /// The front tab's schema, before Run.
    Schema,
    Spacer,
}

fn tool(
    id: &'static str,
    icon: Icon,
    label: Option<&'static str>,
    enabled: bool,
    tip: impl Into<SharedString>,
) -> Item {
    Item::Tool {
        id,
        icon,
        label,
        kind: Kind::Ghost,
        enabled,
        tip: tip.into(),
    }
}

/// Labels go first from the right of this list: least important first.
const DROP_LABELS: [&str; 10] = [
    "history",
    "export",
    "import",
    "rollback",
    "commit",
    "format",
    "explain",
    "console",
    "new-query",
    "run",
];

/// Width of `text` in the UI font.
fn text_width(window: &Window, text: &str, weight: FontWeight) -> Pixels {
    let run = TextRun {
        len: text.len(),
        font: gpui_kit::Font {
            weight,
            ..font(UI_FONT)
        },
        color: black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_owned()), ui_size(), &[run], None)
        .width
}

/// Enabled and disabled as the design's toolbar `context = none`: no connection, no tab.
/// Stays on one row: when the window is too narrow, labels drop in [`DROP_LABELS`]
/// order and the buttons keep their icon and tooltip.
fn toolbar(shell: &Shell, window: &Window, cx: &mut Context<Shell>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let table_tab = shell
        .front_tab()
        .is_some_and(|t| t.read(cx).table.is_some());
    let (has_tab, can_run, running, sql) =
        shell.front_tab().map_or((false, false, false, false), |t| {
            let t = t.read(cx);
            let engine = t.connection.as_ref().map(|c| c.engine);
            (
                true,
                engine.is_some() && !t.is_running() && !table_tab,
                t.is_running(),
                engine.is_some_and(|e| e != deadlock_core::connection::Engine::Redis),
            )
        });
    let (tx_open, tx_mode) = shell.front_tab().map_or((false, TxMode::Auto), |t| {
        let t = t.read(cx);
        (t.tx_open && !t.is_running(), t.tx_mode)
    });
    let import = shell.import_target(cx);
    let k = |id: &str| crate::keymap::label(id, cx);
    let settings_tip = SharedString::from(format!("Settings ({})", k("settings")));
    let items = [
        tool(
            "new-query",
            Icon::NewQuery,
            Some("New query"),
            true,
            format!("New query ({})", k("new-query")),
        ),
        tool(
            "console",
            Icon::Console,
            Some("Console"),
            shell.console_target(cx).is_some(),
            format!("Open console ({})", k("console")),
        ),
        tool(
            "open-file",
            Icon::Folder,
            None,
            true,
            format!("Open file ({})", k("open-file")),
        ),
        tool(
            "save",
            Icon::Save,
            None,
            has_tab && !table_tab,
            format!("Save ({})", k("save")),
        ),
        Item::Separator,
        Item::Schema,
        Item::Tool {
            id: "run",
            icon: Icon::Play,
            label: Some("Run"),
            kind: Kind::Primary,
            enabled: can_run,
            tip: format!("Run statement ({})", k("run")).into(),
        },
        tool(
            "stop",
            Icon::Stop,
            None,
            running,
            format!("Stop running statement ({})", k("stop")),
        ),
        tool(
            "explain",
            Icon::Explain,
            Some("Explain"),
            can_run && sql && shell.front_can_explain(cx),
            format!(
                "Explain plan ({}); measured: {}",
                k("explain"),
                k("explain-measured")
            ),
        ),
        tool(
            "format",
            Icon::Format,
            Some("Format"),
            has_tab && sql && !table_tab,
            format!("Format SQL ({})", k("format")),
        ),
        Item::Separator,
        tool(
            "commit",
            Icon::Commit,
            Some("Commit"),
            tx_open,
            format!("Commit transaction ({})", k("commit")),
        ),
        tool(
            "rollback",
            Icon::Rollback,
            Some("Roll back"),
            tx_open,
            "Roll back transaction",
        ),
        Item::TxMode,
        Item::Separator,
        tool(
            "refresh",
            Icon::Refresh,
            None,
            true,
            format!("Refresh schema ({})", k("refresh")),
        ),
        tool(
            "import",
            Icon::Import,
            Some("Import"),
            import.is_ok(),
            import
                .as_ref()
                .err()
                .copied()
                .unwrap_or("Import a CSV file into a table"),
        ),
        tool(
            "export",
            Icon::Export,
            Some("Export"),
            shell
                .front_tab()
                .is_some_and(|t| !t.read(cx).is_running() && t.read(cx).export_grid(cx).is_some()),
            "Export the result or table to a file",
        ),
        Item::Spacer,
        tool(
            "history",
            Icon::History,
            Some("History"),
            true,
            format!("Query history ({})", k("history")),
        ),
        Item::Separator,
        tool(
            "files",
            Icon::PanelRight,
            None,
            true,
            format!("Files panel ({})", k("files")),
        ),
    ];

    // Widths from `.b`: 1 px border and 8 px padding a side, 16 px icon, 6 px gaps.
    let w = |text: &str, weight| text_width(window, text, weight);
    let switcher_w = px(2. + 16. + 6. + 16.) + w(&shell.project.name(), FontWeight::MEDIUM);
    let chip = schema::chip_label(shell, cx);
    let items: Vec<Item> = items
        .into_iter()
        .filter(|i| chip.is_some() || !matches!(i, Item::Schema))
        .collect();
    let mut total = px(24. + 32. + 9.) + switcher_w + px(4. * (items.len() + 2) as f32);
    let mut labels: Vec<(&str, Pixels)> = Vec::new();
    for item in &items {
        let base = match item {
            Item::Tool {
                id, label, kind, ..
            } => {
                if let Some(l) = label {
                    let weight = if *kind == Kind::Primary {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    };
                    let lw = px(6.) + w(l, weight);
                    labels.push((id, lw));
                    total += lw;
                }
                px(34.)
            }
            Item::Separator => px(9.),
            Item::TxMode => {
                px(2. + 16. + 6. + 6. + 16.)
                    + w("Tx", FontWeight::NORMAL)
                    + w(tx_mode.label(), FontWeight::NORMAL)
            }
            Item::Schema => chip
                .as_ref()
                .map_or(px(0.), |(l, _)| schema::chip_width(window, l)),
            Item::Spacer => px(0.),
        };
        total += base;
    }
    let available = window.viewport_size().width;
    let mut hidden: Vec<&str> = Vec::new();
    for id in DROP_LABELS {
        if total <= available {
            break;
        }
        if let Some((_, lw)) = labels.iter().find(|(l, _)| *l == id) {
            total -= *lw;
            hidden.push(id);
        }
    }

    let children = items.into_iter().map(|item| match item {
        Item::Tool {
            id,
            icon,
            label,
            kind,
            enabled,
            tip,
        } => {
            let label = label.filter(|_| !hidden.contains(&id));
            // The Export menu's trigger shows its tooltip itself.
            let tip = if id == "export" && enabled {
                SharedString::default()
            } else {
                tip
            };
            // `.b.pr`: the Files toggle is pressed while the panel is open.
            let b = if id == "files" && shell.project.state.files_open {
                button(id, kind, enabled, tip, cx)
                    .bg(p.sel)
                    .text_color(p.tx)
                    .child(crate::assets::icon(icon, p.tx))
            } else {
                icon_button(id, icon, label, kind, enabled, tip, cx)
            };
            let b = match id {
                _ if !enabled => b,
                "refresh" => b.clickable(cx.listener(|this, _, _, cx| this.refresh(cx))),
                "new-query" => {
                    b.clickable(cx.listener(|this, _, window, cx| this.new_query(window, cx)))
                }
                "console" => {
                    b.clickable(cx.listener(|this, _, window, cx| this.open_console(window, cx)))
                }
                "open-file" => {
                    b.clickable(cx.listener(|this, _, window, cx| this.open_file(window, cx)))
                }
                "save" => b.clickable(cx.listener(|this, _, _, cx| this.save_front(cx))),
                "run" => b.clickable(
                    cx.listener(|this, _, _, cx| this.run_front(crate::query::Scope::Caret, cx)),
                ),
                "stop" => b.clickable(cx.listener(|this, _, _, cx| this.stop_front(cx))),
                "explain" => {
                    b.clickable(cx.listener(|this, _, _, cx| this.explain_front(false, cx)))
                }
                "commit" => b.clickable(cx.listener(|this, _, _, cx| this.end_front_tx(true, cx))),
                "rollback" => {
                    b.clickable(cx.listener(|this, _, _, cx| this.end_front_tx(false, cx)))
                }
                "format" => {
                    b.clickable(cx.listener(|this, _, window, cx| this.format_front(window, cx)))
                }
                "export" => return export_menu(shell, b, cx),
                "import" => b.clickable(cx.listener(|this, _, window, cx| {
                    if let Ok((id, table)) = this.import_target(cx) {
                        this.open_import(&id, table, window, cx);
                    }
                })),
                "history" => {
                    b.clickable(cx.listener(|this, _, window, cx| this.toggle_history(window, cx)))
                }
                "files" => {
                    b.clickable(cx.listener(|this, _, window, cx| this.toggle_files(window, cx)))
                }
                _ => b,
            };
            b.into_any_element()
        }
        Item::Separator => separator(&p).into_any_element(),
        Item::Spacer => div().flex_1().into_any_element(),
        Item::TxMode => tx_menu(shell, has_tab && sql && !running, tx_mode, cx).into_any_element(),
        Item::Schema => schema::chip(shell, cx),
    });
    let children: Vec<_> = children.collect();
    let config = &shell.project.config;
    let look = config.color.as_deref().and_then(crate::color::parse_hex);
    div()
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .h(toolbar_height())
        .px(px(12.))
        .overflow_hidden()
        .bg(p.panel)
        .border_b_1()
        .border_color(p.line)
        .when_some(look, |d, c| title_band(d, c, config.pattern, &p))
        .child(
            div()
                .id("app-logo")
                .flex_none()
                .mr(px(8.))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(settings_tip.clone())
                        .build(window, cx)
                })
                .child(img(APP_LOGO).size(px(24.)))
                .clickable(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
        )
        .child(switcher(shell, cx))
        .child(separator(&p))
        .children(children)
}

/// The project color as a gradient from the left edge, with `pattern` over its left
/// part fading out. The toolbar's items, added after, paint on top.
fn title_band(d: Div, c: Hsla, pattern: Pattern, p: &Palette) -> Div {
    let reach = 0.4;
    // The gradient's own tint where the pattern ends, so the fade lands on it.
    let end = crate::color::over(c, BAND_ALPHA * (1. - reach / BAND_REACH), p.panel);
    d.bg(linear_gradient(
        90.,
        linear_color_stop(crate::color::over(c, BAND_ALPHA, p.panel), 0.),
        linear_color_stop(p.panel, BAND_REACH),
    ))
    .when_some(pattern_layer(pattern, c, p), |d, layer| {
        d.child(layer.w(relative(reach))).child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(relative(reach))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(end.alpha(0.), 0.),
                    linear_color_stop(end, 1.),
                )),
        )
    })
}

/// How strong the title bar's color starts at the left edge, and where it has faded out.
pub(crate) const BAND_ALPHA: f32 = 0.22;
const BAND_REACH: f32 = 0.55;

/// `pattern` in the project color `c`, as an absolute layer over the left of its parent;
/// the caller sets its width. `None` for [`Pattern::None`].
pub(crate) fn pattern_layer(pattern: Pattern, c: Hsla, p: &Palette) -> Option<Div> {
    let layer = div().absolute().top_0().bottom_0().left_0();
    // Dark panels need more of the color for the pattern to show.
    let strength = if p.panel.l < 0.5 { 1.6 } else { 1. };
    Some(match pattern {
        Pattern::None => return None,
        Pattern::Stripes => layer.bg(pattern_slash(c.alpha(0.18 * strength), 1., 6.)),
        Pattern::Checks => layer.bg(checkerboard(c.alpha(0.10 * strength), 6.)),
        Pattern::Dots => {
            let dot = c.alpha(0.26 * strength);
            layer.child(
                canvas(
                    |_, _, _| {},
                    move |b, _, window, _| {
                        // 2 px round dots on an 8 px grid; GPUI ships no dot pattern.
                        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
                        let mut y = 3.;
                        while y < h {
                            let mut x = 3.;
                            while x < w {
                                let at = b.origin + point(px(x), px(y));
                                window.paint_quad(
                                    fill(Bounds::new(at, size(px(2.), px(2.))), dot)
                                        .corner_radii(px(1.)),
                                );
                                x += 8.;
                            }
                            y += 8.;
                        }
                    },
                )
                .size_full(),
            )
        }
    })
}

/// The Export button and its menu, for the front tab's table or result.
fn export_menu(
    shell: &Shell,
    trigger: Stateful<Div>,
    cx: &mut Context<Shell>,
) -> gpui_kit::AnyElement {
    let p = *cx.global::<Palette>();
    let Some(tab) = shell.front_tab().cloned() else {
        return trigger.into_any_element();
    };
    let Some(grid) = tab.read(cx).export_grid(cx) else {
        return trigger.into_any_element();
    };
    let offer = tab.read(cx).export_offer(&grid, cx);
    let all = tab.read(cx).export_all;
    let this = cx.entity().downgrade();
    let weak = tab.downgrade();
    Popover::new("export-menu")
        .p(px(4.))
        .open(shell.export_menu_open)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.export_menu_open = *open;
            cx.notify();
        }))
        .trigger(Trigger(
            trigger,
            shell.export_menu_open,
            "Export the result or table to a file",
        ))
        .content(move |_, _, _| {
            let (pick_tab, all_tab) = (weak.clone(), weak.clone());
            let (shell, grid) = (this.clone(), grid.clone());
            crate::export::menu(
                offer,
                all,
                move |v, cx| {
                    let _ = all_tab.update(cx, |t, cx| {
                        t.export_all = v;
                        cx.notify();
                    });
                    let _ = shell.update(cx, |_, cx| cx.notify());
                },
                {
                    let shell = this.clone();
                    move |f, all, cx| {
                        let _ = shell.update(cx, |s, cx| {
                            s.export_menu_open = false;
                            cx.notify();
                        });
                        let _ = pick_tab.update(cx, |t, cx| t.export(grid.clone(), f, all, cx));
                    }
                },
                &p,
            )
        })
        .into_any_element()
}

/// The Tx button and its menu: Auto or Manual for the front tab's session.
fn tx_menu(
    shell: &Shell,
    enabled: bool,
    mode: TxMode,
    cx: &mut Context<Shell>,
) -> impl IntoElement + use<> {
    let p = *cx.global::<Palette>();
    let trigger = button("tx-mode", Kind::Ghost, enabled, "", cx)
        .child(div().text_color(p.tx3).child("Tx"))
        .child(mode.label())
        .child(button_icon(
            "tx-mode",
            Icon::ChevronDown,
            Kind::Ghost,
            enabled,
            cx,
        ));
    if !enabled {
        return trigger.into_any_element();
    }
    let this = cx.entity().downgrade();
    Popover::new("tx-menu")
        .p(px(4.))
        .min_w(px(260.))
        .open(shell.tx_menu_open)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.tx_menu_open = *open;
            cx.notify();
        }))
        .trigger(Trigger(trigger, shell.tx_menu_open, "Transaction mode"))
        .content(move |_, _, _| {
            let rows = [
                (TxMode::Auto, "Each statement commits"),
                (TxMode::Manual, "Commit or roll back yourself"),
            ]
            .into_iter()
            .map(|(m, what)| {
                let this = this.clone();
                div()
                    .id(m.label())
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(row_height())
                    .px(px(8.))
                    .rounded(RADIUS)
                    .whitespace_nowrap()
                    .when(m == mode, |d| d.bg(p.sel))
                    .hover(|s| s.bg(p.hover))
                    .clickable(move |_, _, cx| {
                        let _ = this.update(cx, |this, cx| this.set_front_tx_mode(m, cx));
                    })
                    .child(div().font_weight(FontWeight::MEDIUM).child(m.label()))
                    .child(type_label(what, &p))
            });
            div().flex().flex_col().gap(px(1.)).children(rows)
        })
        .into_any_element()
}

/// A tab's short environment badge text; Development shows none.
fn env_tag(env: Environment) -> Option<&'static str> {
    match env {
        Environment::Production => Some("PROD"),
        Environment::Staging => Some("STAGE"),
        Environment::Development => None,
    }
}

/// The environment marker on a tab: warn-colored PROD, neutral STAGE, nothing for dev.
pub(crate) fn env_marker(env: Environment, p: &Palette) -> Option<Div> {
    let tag = env_tag(env)?;
    let prod = env == Environment::Production;
    Some(
        div()
            .flex_none()
            .px(px(6.))
            .rounded(px(4.))
            .border_1()
            .border_color(if prod { p.warn_line } else { p.line2 })
            .when(prod, |d| d.bg(p.warn_bg))
            .text_color(if prod { p.warn } else { p.tx2 })
            .text_size(px(11.))
            .font_weight(FontWeight::MEDIUM)
            .child(tag),
    )
}

fn main_area(shell: &Shell, cx: &mut Context<Shell>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let body = match shell.front {
        Front::Query(i) => shell.tabs.get(i).map(|(t, _)| t.clone().into_any_element()),
        Front::Form => shell
            .form
            .as_ref()
            .map(|(f, _)| f.clone().into_any_element()),
        Front::Import => shell
            .import
            .as_ref()
            .map(|(t, _)| t.clone().into_any_element()),
        Front::Settings => shell
            .settings_tab
            .as_ref()
            .map(|t| t.clone().into_any_element()),
        Front::Sessions(i) => shell
            .sessions
            .get(i)
            .map(|(t, _)| t.clone().into_any_element()),
        Front::Config(i) => shell
            .configs
            .get(i)
            .map(|(t, _)| t.clone().into_any_element()),
        Front::Diagram(i) => shell
            .diagrams
            .get(i)
            .map(|(t, _)| t.clone().into_any_element()),
        Front::Object(i) => shell
            .objects
            .get(i)
            .map(|(t, _)| t.clone().into_any_element()),
        Front::None => None,
    };
    let body = body.unwrap_or_else(|| {
        div()
            .size_full()
            .flex()
            .items_center()
            .flex_col()
            .gap(px(8.))
            .justify_center()
            .text_color(p.tx3)
            .child(format!(
                "New query ({}), open a file ({}) or open the console ({}).",
                crate::keymap::label("new-query", cx),
                crate::keymap::label("open-file", cx),
                crate::keymap::label("console", cx),
            ))
            .child(
                div()
                    .id("open-settings")
                    .text_color(p.link)
                    .hover(|s| s.underline())
                    .child(format!(
                        "Settings ({})",
                        crate::keymap::label("settings", cx)
                    ))
                    .clickable(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            )
            .into_any_element()
    });
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(p.bg)
        .child(tabs::tab_strip(shell, cx))
        .child(div().flex_1().min_h_0().child(body))
}

fn status_bar(shell: &Shell, p: &Palette, cx: &mut Context<Shell>) -> impl IntoElement {
    let active = shell.active(cx);
    let active = active.as_deref().and_then(|id| shell.config(id));
    let conn = active.and_then(|c| shell.conns.get(&c.id));
    let (label, version): (SharedString, Option<SharedString>) = match (active, conn) {
        (Some(c), Some(Conn::Connected(d))) if reconnecting(d) => (
            format!("Connection to {} was lost: reconnecting…", c.name).into(),
            None,
        ),
        (Some(c), Some(Conn::Connected(d))) => (
            format!("Connected to {}", c.name).into(),
            Some(d.server_version().to_owned().into()),
        ),
        (Some(c), Some(Conn::Connecting)) => (format!("Connecting to {}…", c.name).into(), None),
        (Some(c), Some(Conn::Failed(e))) => (format!("{}: {e}", c.name).into(), None),
        (Some(c), None) => (format!("{} is not connected", c.name).into(), None),
        (None, _) => ("No connection".into(), None),
    };
    let st = || {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(status_bar_height() - px(2.))
            .px(px(8.))
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_color(p.tx2)
    };
    let note = crate::settings::note(cx);
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .h(status_bar_height())
        .px(px(8.))
        .bg(p.panel)
        .border_t_1()
        .border_color(p.line)
        .text_size(small_size())
        .child(st().child(dot(state_dot(conn, p))).child(label))
        .children(version.map(|v| st().child(v)))
        .when(active.is_some_and(|c| c.read_only), |d| {
            d.child(st().child("Read-only"))
        })
        .when_some(note, |d, (text, warn)| {
            d.child(st().when(warn, |d| d.text_color(p.warn)).child(text))
        })
        .when(shell.front_tab().is_some_and(|t| t.read(cx).tx_open), |d| {
            d.child(
                st().text_color(p.warn)
                    .font_weight(FontWeight::MEDIUM)
                    .child(dot(p.warn))
                    .child("Uncommitted changes"),
            )
        })
        .when_some(
            active
                .map(|c| (c.id.clone(), shell.waiting_on(&c.id)))
                .filter(|(_, n)| *n > 0),
            |d, (id, n)| {
                let label = if n == 1 {
                    "1 session waiting on a lock".to_owned()
                } else {
                    format!("{n} sessions waiting on a lock")
                };
                d.child(
                    st().id("waiting-link")
                        .rounded(px(4.))
                        .text_color(p.warn)
                        .font_weight(FontWeight::MEDIUM)
                        .hover(|s| s.bg(p.hover))
                        .child(crate::assets::icon(Icon::Lock, p.warn))
                        .child(label)
                        .clickable(cx.listener(move |this, _, window, cx| {
                            this.open_sessions(&id, window, cx);
                        })),
                )
            },
        )
        .child(div().flex_1())
        .when_some(shell.front_tab(), |d, tab| {
            let t = tab.read(cx);
            if t.table.is_some() {
                return d.children(t.rows_status(cx).map(|s| st().child(s)));
            }
            let (line, col) = t.position(cx);
            let file = match &t.kind {
                crate::query::Kind::File(Some(path)) => {
                    Some(crate::files::shown_path(shell.project.root(), path))
                }
                _ => None,
            };
            d.children(file.map(|f| st().child(f)))
                .child(st().child(format!("Line {line}, column {col}")))
                .child(st().child("UTF-8"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_versions_are_major_minor() {
        assert_eq!(short_version("MySQL 8.4.3"), "MySQL 8.4");
        assert_eq!(short_version("Oracle 23.26.4.0.0"), "Oracle 23.26");
        assert_eq!(short_version("Redis 8"), "Redis 8");
    }
}
