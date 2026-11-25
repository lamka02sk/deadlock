//! The object editor tab: one view, procedure, function, package, trigger, event,
//! sequence, synonym or type. Header with status chips and facts, section buttons
//! (Properties, Source, or Spec / Body for source-first objects, Errors on Oracle), the
//! form built from [`form::layout`] over a draft, and the "Statements to run" footer
//! from [`deadlock_core::objects::plan`], recomputed on every edit. Apply, Drop, Rename,
//! Recompile and Revert to last valid all run a plan through
//! [`Driver::apply_object`] on the sidebar's session, behind the confirmation rules in
//! [`form::needs_confirm`].

mod form;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use deadlock_core::connection::{ConnectionConfig, Environment};
use deadlock_core::driver::{Driver, ObjectPath};
use deadlock_core::objects::{
    self, ApplyOutcome, Backup, ChangePlan, FinalState, Flavor, ObjectDefinition, ObjectDraft,
    ObjectError, ObjectKind, ObjectRef, Param, ParamMode, Properties, StepStatus,
};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{self, Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable as _, IndexPath};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Div, Entity, EventEmitter,
    FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px, relative,
};

use self::form::{Banner, Field, Tone, Widget};
use crate::app::Db;
use crate::assets::{Icon, icon};
use crate::query::table::highlighted;
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, editor_size, row_height, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind as ButtonKind, button, icon_button};

/// Something to do once the object has loaded: the sidebar menu's Rename, Drop and
/// Recompile open the tab first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Then {
    Rename,
    Drop,
    Recompile,
}

/// What the tab opens on.
pub(crate) enum Target {
    Existing(ObjectRef, Option<Then>),
    /// A new object from [`objects::template`].
    New(Box<ObjectDraft>),
}

/// Gives an editor the connection's SQL completions.
pub(crate) type Complete = Rc<dyn Fn(&Entity<EditorState>, &mut App)>;

pub(crate) enum ObjectEvent {
    /// Title, dirty dot or state changed: the strip repaints.
    Changed,
    /// Tree nodes to reload, relative to the connection.
    Refresh(Vec<ObjectPath>),
    /// Discard on a new object: close the tab (asking first when edited).
    Close,
    /// The object was dropped: the tab goes.
    Dropped,
    /// Open backup: the original's statements, written when it went missing.
    OpenFile(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Properties,
    Source,
    Spec,
    Body,
    Errors,
}

enum Load {
    Loading,
    Failed(String),
    Ready,
}

/// Why a plan runs: decides the prompt and what happens after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Apply,
    Revert,
    Drop,
    Rename,
    Recompile,
}

type Choice = Entity<SelectState<Vec<String>>>;

struct ParamRow {
    mode: Option<Choice>,
    name: Entity<InputState>,
    data_type: Entity<InputState>,
    default: Entity<InputState>,
}

/// The form's inputs, made from the draft.
#[derive(Default)]
struct Form {
    inputs: HashMap<&'static str, Entity<InputState>>,
    selects: HashMap<&'static str, Choice>,
    params: Vec<ParamRow>,
    subs: Vec<Subscription>,
}

pub struct ObjectTab {
    pub connection: ConnectionConfig,
    driver: Arc<dyn Driver>,
    pub flavor: Flavor,
    root: PathBuf,
    /// The object as the server knows it (for a new one: its kind and schema).
    pub object: ObjectRef,
    /// Not created yet: Apply reads Create.
    pub new: bool,
    load: Load,
    def: Option<ObjectDefinition>,
    /// The definition from before the last Apply: Revert to last valid puts it back.
    held: Option<ObjectDefinition>,
    /// What the draft started from; the dirty mark compares against it.
    base: ObjectDraft,
    draft: ObjectDraft,
    plan: Result<ChangePlan, String>,
    preview: String,
    section: Section,
    footer_open: bool,
    running: Option<(Purpose, ChangePlan)>,
    last: Option<ApplyOutcome>,
    banner: Option<Banner>,
    form: Form,
    editor: Option<Entity<EditorState>>,
    /// Packages and types: the body part.
    editor2: Option<Entity<EditorState>>,
    editor_subs: Vec<Subscription>,
    renaming: Option<(Entity<InputState>, Subscription)>,
    then: Option<Then>,
    copied: bool,
    /// Gives an editor the connection's SQL completions.
    complete: Complete,
}

impl EventEmitter<ObjectEvent> for ObjectTab {}

impl ObjectTab {
    #[allow(
        clippy::too_many_arguments,
        reason = "everything a tab needs, given once"
    )]
    pub(crate) fn new(
        connection: ConnectionConfig,
        driver: Arc<dyn Driver>,
        flavor: Flavor,
        root: PathBuf,
        target: Target,
        complete: Complete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (object, draft, then, new) = match target {
            Target::Existing(o, then) => {
                let mut d = objects::template(flavor, o.kind, &o.schema);
                d.object.name.clone_from(&o.name);
                (o, d, then, false)
            }
            Target::New(d) => (d.object.clone(), *d, None, true),
        };
        let mut this = Self {
            connection,
            driver,
            flavor,
            root,
            object,
            new,
            load: if new { Load::Ready } else { Load::Loading },
            def: None,
            held: None,
            base: draft.clone(),
            draft,
            plan: Err(String::new()),
            preview: String::new(),
            section: Section::Properties,
            footer_open: true,
            running: None,
            last: None,
            banner: None,
            form: Form::default(),
            editor: None,
            editor2: None,
            editor_subs: Vec::new(),
            renaming: None,
            then,
            copied: false,
            complete,
        };
        if new {
            this.first_view();
            this.rebuild(window, cx);
        } else {
            this.reload(true, window, cx);
        }
        this.replan();
        this
    }

    /// `orders_audit_bu`, or `New trigger` until it has a name.
    pub fn title(&self) -> String {
        if self.new && self.draft.object.name.trim().is_empty() {
            format!("New {}", self.object.kind.label())
        } else if self.new {
            self.draft.object.name.clone()
        } else {
            self.object.name.clone()
        }
    }

    /// Edits not applied: closing asks first.
    pub fn dirty(&self) -> bool {
        form::is_dirty(&self.base, &self.draft)
    }

    /// The tab's dot: unapplied edits, or an object not created yet.
    pub fn marked(&self) -> bool {
        self.new || self.dirty()
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The tab edits `object` of connection `conn`.
    pub fn is(&self, conn: &str, object: &ObjectRef) -> bool {
        !self.new && self.connection.id == conn && self.object == *object
    }

    /// Whether the tab edits a trigger on table `table` of connection `conn`.
    pub(crate) fn is_on_table(&self, conn: &str, table: &deadlock_core::edit::TableRef) -> bool {
        self.connection.id == conn
            && (self.base.is_on_table(table) || self.draft.is_on_table(table))
    }

    /// Table `from` of connection `conn` is `to` now: a trigger on it follows, keeping
    /// unapplied edits, and reads itself again from the server.
    pub(crate) fn table_renamed(
        &mut self,
        conn: &str,
        from: &deadlock_core::edit::TableRef,
        to: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection.id != conn || !self.draft.follow_table_rename(from, to) {
            return;
        }
        self.base.follow_table_rename(from, to);
        self.build_form(window, cx);
        if self.new {
            self.changed(cx);
        } else {
            self.reload(false, window, cx);
        }
    }

    fn source_first(&self) -> bool {
        matches!(self.draft.props, Properties::Source(_))
    }

    /// The section and footer an object opens with.
    fn first_view(&mut self) {
        let source = self.source_first();
        self.section = if source {
            Section::Spec
        } else {
            Section::Properties
        };
        // The preview would repeat the editor for source-first objects.
        self.footer_open = !source;
    }

    /// Why nothing can be changed here, if so.
    fn read_only(&self) -> Option<String> {
        self.def
            .as_ref()
            .and_then(|d| d.read_only.clone())
            .or_else(|| {
                self.connection
                    .read_only
                    .then(|| "The connection is read-only.".to_owned())
            })
    }

    /// Fields are off: not loaded, read-only, or a plan is running.
    fn locked(&self) -> bool {
        !matches!(self.load, Load::Ready) || self.running.is_some() || self.read_only().is_some()
    }

    /// The menu's Rename, Drop and Recompile wait for the definition.
    fn later(&mut self, then: Then) -> bool {
        let waiting = !self.new && self.def.is_none();
        if waiting {
            self.then = Some(then);
        }
        waiting
    }

    fn replan(&mut self) {
        self.plan = objects::plan(self.flavor, self.def.as_ref(), &self.draft);
        self.preview = self
            .plan
            .as_ref()
            .map(ChangePlan::preview)
            .unwrap_or_default();
    }

    /// The draft changed: the preview follows.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.replan();
        cx.emit(ObjectEvent::Changed);
        cx.notify();
    }

    /// Reads the object from the server. `reset`: the draft starts over from it.
    fn reload(&mut self, reset: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.def.is_none() {
            self.load = Load::Loading;
        }
        let (driver, object) = (Arc::clone(&self.driver), self.object.clone());
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { driver.object(&object).await.map_err(|e| e.to_string()) });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the object task stopped".into()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.loaded(result, reset, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn loaded(
        &mut self,
        result: Result<ObjectDefinition, String>,
        reset: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(def) => {
                let first = self.def.is_none();
                self.base = def.draft();
                if reset || first {
                    self.draft = self.base.clone();
                }
                self.object = def.object.clone();
                self.def = Some(def);
                self.new = false;
                self.load = Load::Ready;
                if first {
                    self.first_view();
                }
                if reset || first {
                    self.rebuild(window, cx);
                }
                match self.then.take() {
                    Some(Then::Rename) => self.start_rename(window, cx),
                    Some(Then::Drop) => self.ask_drop(window, cx),
                    Some(Then::Recompile) => self.recompile(window, cx),
                    None => {}
                }
            }
            Err(e) if self.def.is_none() => self.load = Load::Failed(e),
            Err(e) => self.banner = Some(Banner::error(format!("Reading it again failed: {e}"))),
        }
        self.changed(cx);
    }

    /// Makes the inputs and editors again from the draft.
    fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.build_form(window, cx);
        self.build_editors(window, cx);
    }

    fn build_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut f = Form::default();
        for g in form::layout(self.flavor, &self.draft, self.new) {
            for field in g.rows.iter().flatten() {
                let value = (field.get)(&self.draft);
                let set = field.set;
                match &field.widget {
                    Widget::Text { placeholder, .. } => {
                        let placeholder = *placeholder;
                        let state = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(placeholder)
                                .default_value(value)
                        });
                        f.subs
                            .push(cx.subscribe(&state, move |this, s, e: &InputEvent, cx| {
                                if matches!(e, InputEvent::Change) {
                                    let v = s.read(cx).value().to_string();
                                    set(&mut this.draft, &v);
                                    this.changed(cx);
                                }
                            }));
                        f.inputs.insert(field.key, state);
                    }
                    Widget::Select(options) => {
                        let items: Vec<String> = options.iter().map(|o| (*o).to_owned()).collect();
                        let at = items.iter().position(|o| *o == value);
                        let state = cx.new(|cx| {
                            SelectState::new(
                                items,
                                at.map(|r| IndexPath::default().row(r)),
                                window,
                                cx,
                            )
                        });
                        let key = field.key;
                        f.subs.push(cx.subscribe_in(
                            &state,
                            window,
                            move |this, _, e: &SelectEvent<Vec<String>>, window, cx| {
                                let SelectEvent::Confirm(Some(v)) = e else {
                                    return;
                                };
                                set(&mut this.draft, v);
                                // Runs swaps the event's schedule fields.
                                if key == form::RUNS {
                                    this.build_form(window, cx);
                                }
                                this.changed(cx);
                            },
                        ));
                        f.selects.insert(field.key, state);
                    }
                    Widget::Check | Widget::Fixed => {}
                }
            }
        }
        if let Properties::Routine(r) = &self.draft.props {
            let modes = form::param_modes(self.flavor, self.draft.object.kind);
            for (i, p) in r.params.iter().enumerate() {
                let mode = modes.as_ref().map(|modes| {
                    let items: Vec<String> = modes.iter().map(|m| (*m).to_owned()).collect();
                    let label = form::mode_label(self.flavor, p.mode);
                    let at = items.iter().position(|m| m == label);
                    let state = cx.new(|cx| {
                        SelectState::new(items, at.map(|r| IndexPath::default().row(r)), window, cx)
                    });
                    f.subs.push(cx.subscribe(
                        &state,
                        move |this, _, e: &SelectEvent<Vec<String>>, cx| {
                            if let SelectEvent::Confirm(Some(v)) = e
                                && let Some(m) = form::parse_mode(v)
                            {
                                this.edit_param(i, |p| p.mode = m);
                                this.changed(cx);
                            }
                        },
                    ));
                    state
                });
                let mut input =
                    |value: &str, placeholder: &'static str, set: fn(&mut Param, String)| {
                        let value = value.to_owned();
                        let state = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(placeholder)
                                .default_value(value)
                        });
                        f.subs
                            .push(cx.subscribe(&state, move |this, s, e: &InputEvent, cx| {
                                if matches!(e, InputEvent::Change) {
                                    let v = s.read(cx).value().to_string();
                                    this.edit_param(i, |p| set(p, v));
                                    this.changed(cx);
                                }
                            }));
                        state
                    };
                let name = input(&p.name, "name", |p, v| p.name = v);
                let data_type = input(&p.data_type, "type", |p, v| p.data_type = v);
                let default = input(&p.default, "none", |p, v| p.default = v);
                f.params.push(ParamRow {
                    mode,
                    name,
                    data_type,
                    default,
                });
            }
        }
        self.form = f;
    }

    fn edit_param(&mut self, i: usize, edit: impl FnOnce(&mut Param)) {
        if let Properties::Routine(r) = &mut self.draft.props
            && let Some(p) = r.params.get_mut(i)
        {
            edit(p);
        }
    }

    fn add_param(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Properties::Routine(r) = &mut self.draft.props {
            r.params.push(Param::default());
        }
        self.build_form(window, cx);
        self.changed(cx);
    }

    fn remove_param(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Properties::Routine(r) = &mut self.draft.props
            && i < r.params.len()
        {
            r.params.remove(i);
        }
        self.build_form(window, cx);
        self.changed(cx);
    }

    fn build_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor_subs.clear();
        let first = form::body(&self.draft).map(|(_, get, set)| (get(&self.draft), set));
        let second =
            (self.source_first() && form::source_has_body(self.draft.object.kind)).then(|| {
                let (get, set) = form::source_body();
                (get(&self.draft), set)
            });
        self.editor = first.map(|(v, set)| self.make_editor(v, set, window, cx));
        self.editor2 = second.map(|(v, set)| self.make_editor(v, set, window, cx));
    }

    fn make_editor(
        &mut self,
        value: String,
        set: form::Set,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<EditorState> {
        let wrap = crate::settings::get(cx).wrap;
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .soft_wrap(wrap)
                .folding(false)
                .default_value(value)
        });
        self.editor_subs
            .push(cx.subscribe(&editor, move |this, e, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    let v = e.read(cx).value().to_string();
                    set(&mut this.draft, &v);
                    this.changed(cx);
                }
            }));
        (self.complete)(&editor, cx);
        editor
    }

    /// Ctrl+S and the Apply (Create) button.
    pub(crate) fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(plan) = self.plan.clone() else {
            return;
        };
        if plan.is_empty() || self.locked() {
            return;
        }
        self.run(Purpose::Apply, plan, window, cx);
    }

    /// Revert: the edits go, the form shows the object as loaded.
    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft = self.base.clone();
        self.rebuild(window, cx);
        self.changed(cx);
    }

    /// Revert to last valid: the definition held from before the last Apply goes back.
    fn revert_to_valid(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(current), Some(held)) = (&self.def, &self.held) else {
            return;
        };
        match objects::revert_plan(current, held) {
            Ok(p) if !p.is_empty() => self.run(Purpose::Revert, p, window, cx),
            Ok(_) => {}
            Err(e) => self.banner = Some(Banner::error(e)),
        }
        cx.notify();
    }

    pub(crate) fn ask_drop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.later(Then::Drop) || self.new || self.locked() {
            return;
        }
        let plan = objects::drop_plan(self.flavor, &self.object);
        self.run(Purpose::Drop, plan, window, cx);
    }

    pub(crate) fn recompile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.later(Then::Recompile) || self.new || self.locked() {
            return;
        }
        match objects::recompile_plan(self.flavor, &self.object) {
            Ok(p) => self.run(Purpose::Recompile, p, window, cx),
            Err(e) => self.banner = Some(Banner::error(e)),
        }
        cx.notify();
    }

    pub(crate) fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.later(Then::Rename) || self.new || self.locked() {
            return;
        }
        if let Err(reason) = self.object.kind.can_rename(self.flavor) {
            self.banner = Some(Banner::error(reason));
            cx.notify();
            return;
        }
        let name = self.object.name.clone();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let sub = cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::PressEnter { .. }) {
                this.finish_rename(window, cx);
            }
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        self.renaming = Some((input, sub));
        cx.notify();
    }

    fn finish_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = self.renaming.take() else {
            return;
        };
        let to = input.read(cx).value().to_string();
        let Some(def) = &self.def else { return };
        match objects::rename_plan(def, &to) {
            Ok(p) if !p.is_empty() => self.run(Purpose::Rename, p, window, cx),
            Ok(_) => {}
            Err(e) => self.banner = Some(Banner::error(e)),
        }
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
    }

    /// The confirmation for `plan`.
    fn confirm(&self, purpose: Purpose, plan: &ChangePlan) -> crate::dialog::Ask {
        let name = &self.object.name;
        let conn = &self.connection.name;
        let kind = self.object.kind.label();
        let n = plan.statement_count();
        let statements = if n == 1 {
            "1 statement".to_owned()
        } else {
            format!("{n} statements")
        };
        let (title, button) = match purpose {
            Purpose::Drop => (format!("Drop {kind} {name}?"), format!("Drop {kind}")),
            Purpose::Rename => (
                format!("Rename {name} to {} on {conn}?", plan.object.name),
                format!("Rename ({statements})"),
            ),
            Purpose::Recompile => (format!("Recompile {name} on {conn}?"), "Recompile".into()),
            Purpose::Revert => (
                format!("Revert {name} on {conn} to the last valid source?"),
                format!("Apply {statements}"),
            ),
            Purpose::Apply if self.new => (
                format!("Create {kind} {} on {conn}?", plan.object.name),
                format!("Create ({statements})"),
            ),
            Purpose::Apply => (
                format!("Change {name} on {conn}?"),
                format!("Apply {statements}"),
            ),
        };
        let text = match (purpose, plan.non_atomic_warning()) {
            (_, Some(w)) => w,
            (Purpose::Drop, None) => format!(
                "The {kind} is gone at once. Copy its CREATE statement first to run it again."
            ),
            (_, None) => format!("Runs {statements} on {conn}."),
        };
        crate::dialog::Ask::new(title, &[button.as_str(), "Cancel"])
            .text(text)
            .statement(if purpose == Purpose::Drop {
                plan.preview()
            } else {
                form::steps_text(plan)
            })
            .production(self.connection.environment == Environment::Production)
            .danger(purpose == Purpose::Drop)
    }

    /// Asks first where the rules say so (Drop always), then runs `plan`.
    fn run(
        &mut self,
        purpose: Purpose,
        plan: ChangePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ask =
            purpose == Purpose::Drop || form::needs_confirm(self.connection.environment, &plan);
        let answer = ask.then(|| crate::dialog::ask(window, cx, self.confirm(purpose, &plan)));
        cx.spawn_in(window, async move |this, cx| {
            if let Some(answer) = answer
                && answer.await != Some(0)
            {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| this.start(purpose, plan, window, cx));
        })
        .detach();
    }

    fn start(
        &mut self,
        purpose: Purpose,
        plan: ChangePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.running.is_some() {
            return;
        }
        self.banner = None;
        self.last = None;
        if purpose == Purpose::Apply {
            self.held.clone_from(&self.def);
        }
        self.renaming = None;
        self.running = Some((purpose, plan.clone()));
        let driver = Arc::clone(&self.driver);
        let backup = Backup {
            root: self.root.clone(),
            connection: self.connection.id.clone(),
        };
        let rx = cx.global::<Db>().0.spawn(async move {
            driver
                .apply_object(&plan, Some(&backup))
                .await
                .map_err(|e| e.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the apply task stopped".into()));
            let _ = this.update_in(cx, |this, window, cx| this.finished(result, window, cx));
        })
        .detach();
        cx.emit(ObjectEvent::Changed);
        cx.notify();
    }

    fn finished(
        &mut self,
        result: Result<ApplyOutcome, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((purpose, plan)) = self.running.take() else {
            return;
        };
        let o = match result {
            Ok(o) => o,
            Err(e) => {
                self.banner = Some(Banner::error(format!("Nothing was applied: {e}")));
                return self.changed(cx);
            }
        };
        cx.emit(ObjectEvent::Refresh(o.refresh.clone()));
        let verb = match purpose {
            Purpose::Apply if self.new => "Created",
            Purpose::Apply => "Applied the changes to",
            Purpose::Revert => "Reverted",
            Purpose::Drop => "Dropped",
            Purpose::Rename => "Renamed",
            Purpose::Recompile => "Recompiled",
        };
        let name = if self.new {
            plan.object.name.clone()
        } else {
            self.object.name.clone()
        };
        let mut banner = form::outcome_banner(&o, &name, &self.connection.name, verb);
        banner.revert &= self.held.is_some();
        match (&o.state, purpose) {
            (FinalState::Changed, Purpose::Drop) => {
                cx.emit(ObjectEvent::Dropped);
                return;
            }
            (FinalState::Changed | FinalState::Invalid | FinalState::Partial, _) => {
                self.object = plan.object.clone();
                self.new = false;
                let reset = !matches!(o.state, FinalState::Partial);
                self.reload(reset, window, cx);
                if o.state == FinalState::Invalid {
                    self.section = Section::Errors;
                }
            }
            // Gone: Apply creates it again from the draft.
            (FinalState::Missing { .. }, _) => {
                self.def = None;
                self.new = true;
            }
            _ => {}
        }
        self.banner = Some(banner);
        self.last = Some(o);
        self.changed(cx);
    }

    /// Puts the caret on the error's line in the editor it belongs to.
    fn goto(&mut self, e: &ObjectError, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.source_first();
        let Some(line) = e.body_line.or(if source { e.line } else { None }) else {
            return;
        };
        let body = e.part.ends_with("BODY") && self.editor2.is_some();
        let editor = if body {
            self.editor2.clone()
        } else {
            self.editor.clone()
        };
        let Some(editor) = editor else { return };
        self.section = match (source, body) {
            (true, true) => Section::Body,
            (true, false) => Section::Spec,
            _ => Section::Properties,
        };
        let column = e.position.map_or(0, |p| p.saturating_sub(1));
        editor.update(cx, |s, cx| {
            s.set_cursor_position(
                input::Position {
                    line: line.saturating_sub(1),
                    character: column,
                },
                window,
                cx,
            );
        });
        cx.notify();
    }

    fn copy(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The errors to list: the last apply's, else the server's compile errors.
    fn errors(&self) -> &[ObjectError] {
        match &self.last {
            Some(o) if !o.errors.is_empty() => &o.errors,
            _ => self.def.as_ref().map_or(&[], |d| &d.errors),
        }
    }

    fn sections(&self) -> Vec<(Section, String)> {
        let mut out = Vec::new();
        if self.source_first() {
            let has_body = form::source_has_body(self.draft.object.kind);
            out.push((
                Section::Spec,
                if has_body { "Spec" } else { "Source" }.to_owned(),
            ));
            if has_body {
                out.push((Section::Body, "Body".to_owned()));
            }
        } else {
            out.push((Section::Properties, "Properties".to_owned()));
            if self.def.is_some() {
                out.push((Section::Source, "Source".to_owned()));
            }
        }
        if self.flavor == Flavor::Oracle && self.def.is_some() {
            let n = self.errors().len();
            out.push((
                Section::Errors,
                if n == 0 {
                    "Errors".to_owned()
                } else {
                    format!("Errors {n}")
                },
            ));
        }
        out
    }
}

/// `.chip`: 11 px medium caps in a 4 px rounded border.
fn chip(text: String, color: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .rounded(px(4.))
        .border_1()
        .border_color(color.opacity(0.5))
        .text_color(color)
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .child(text)
}

/// `.sh`: a section title with a hint or a button on the right.
fn section_head(
    title: impl Into<SharedString>,
    dot: bool,
    right: Option<AnyElement>,
    p: &Palette,
) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .px(px(16.))
        .pt(px(20.))
        .pb(px(8.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into())
                .when(dot, |d| d.child(changed_dot(p))),
        )
        .children(right)
}

fn hint(text: impl Into<SharedString>, p: &Palette) -> Div {
    div()
        .text_size(small_size())
        .text_color(p.tx3)
        .line_height(relative(1.5))
        .child(text.into())
}

fn changed_dot(p: &Palette) -> Div {
    div().size(px(6.)).flex_none().rounded_full().bg(p.warn)
}

/// `.f`: a labelled field, `flex: 1 1 160px; min-width: 120px`.
fn labeled(label: &'static str, dot: bool, field: impl IntoElement, p: &Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .flex_grow(1.)
        .flex_shrink(1.)
        .flex_basis(px(160.))
        .min_w(px(120.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(small_size())
                .text_color(p.tx2)
                .child(label)
                .when(dot, |d| d.child(changed_dot(p))),
        )
        .child(field)
}

/// `.row`: fields side by side, wrapping.
fn fields_row(children: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_end()
        .gap_x(px(12.))
        .gap_y(px(10.))
        .px(px(16.))
        .pb(px(10.))
        .children(children)
}

fn text_input(state: &Entity<InputState>, mono: bool, locked: bool) -> Input {
    let i = Styled::h(Input::new(state), row_height()).disabled(locked);
    if mono {
        i.font_family(MONO_FONT).text_size(data_size())
    } else {
        i
    }
}

/// A read-only value in an input's box.
fn fixed_box(value: String, p: &Palette) -> Div {
    div()
        .flex()
        .items_center()
        .h(row_height())
        .px(px(8.))
        .border_1()
        .border_color(p.line2)
        .rounded(RADIUS)
        .bg(p.panel)
        .text_color(p.tx2)
        .font_family(MONO_FONT)
        .text_size(data_size())
        .overflow_hidden()
        .whitespace_nowrap()
        .child(value)
}

/// The SQL editor in its box.
fn editor_box(editor: &Entity<EditorState>, locked: bool, p: &Palette) -> Editor {
    Editor::new(editor)
        .bordered(false)
        .readonly(locked)
        .h_full()
        .font_family(MONO_FONT)
        .text_size(editor_size())
        .line_height(px(22.))
        .py(px(8.))
        .bg(p.bg)
}

impl ObjectTab {
    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let sections = self.sections().into_iter().map(|(s, label)| {
            let on = self.section == s;
            let id: &'static str = match s {
                Section::Properties => "object-properties",
                Section::Source => "object-source",
                Section::Spec => "object-spec",
                Section::Body => "object-body",
                Section::Errors => "object-errors",
            };
            button(id, ButtonKind::Outline, true, "", cx)
                .when(on, |b| b.bg(p.tx).border_color(p.tx).text_color(p.bg))
                .child(label)
                .clickable(cx.listener(move |this, _, _, cx| {
                    this.section = s;
                    cx.notify();
                }))
        });
        let name = if self.new {
            format!("{}.{}", self.draft.object.schema, self.title())
        } else {
            format!("{}.{}", self.object.schema, self.object.name)
        };
        let mut kind = {
            let l = self.object.kind.label();
            let mut c = l.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        };
        if let Properties::Trigger(t) = &self.draft.props
            && !t.table.is_empty()
        {
            kind = format!("{kind} on {}", t.table);
        }
        let dim = |t: String| div().text_color(p.tx3).whitespace_nowrap().child(t);
        let mut head = div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap_x(px(16.))
            .gap_y(px(8.))
            .px(px(16.))
            .py(px(10.))
            .min_h(px(49.))
            .border_b_1()
            .border_color(p.line)
            .child(div().flex().gap(px(4.)).children(sections))
            .child(
                div()
                    .font_family(MONO_FONT)
                    .text_size(data_size())
                    .child(name),
            )
            .child(dim(kind));
        if let Some(def) = &self.def {
            for (text, ok) in form::status_chips(def) {
                head = head.child(chip(text, if ok { p.ok } else { p.danger }));
            }
            if let Some(on) = form::enabled_chip(self.flavor, &def.props) {
                head = head.child(chip(
                    if on { "ENABLED" } else { "DISABLED" }.into(),
                    if on { p.ok } else { p.tx3 },
                ));
            }
            for (label, value) in &def.facts {
                head = head.child(dim(format!("{label} {value}")));
            }
        }
        let locked = self.locked();
        let mut tools = div().flex().items_center().gap(px(4.)).ml_auto();
        if let Some((input, _)) = &self.renaming {
            tools = tools
                .on_action(cx.listener(|this, _: &input::Escape, _, cx| this.cancel_rename(cx)))
                .child(div().w(px(220.)).child(text_input(input, true, false)))
                .child(
                    button(
                        "rename-cancel",
                        ButtonKind::Outline,
                        true,
                        "Cancel (Esc)",
                        cx,
                    )
                    .child("Cancel")
                    .clickable(cx.listener(|this, _, _, cx| this.cancel_rename(cx))),
                )
                .child(
                    button("rename-go", ButtonKind::Primary, true, "Rename (Enter)", cx)
                        .child("Rename")
                        .clickable(
                            cx.listener(|this, _, window, cx| this.finish_rename(window, cx)),
                        ),
                );
        } else if let Some(def) = &self.def {
            if def.valid() == Some(false)
                && objects::recompile_plan(self.flavor, &self.object).is_ok()
            {
                tools = tools.child(
                    button(
                        "recompile",
                        ButtonKind::Outline,
                        !locked,
                        "ALTER … COMPILE",
                        cx,
                    )
                    .child("Recompile")
                    .when(!locked, |b| {
                        b.clickable(cx.listener(|this, _, window, cx| this.recompile(window, cx)))
                    }),
                );
            }
            if self.object.kind.can_rename(self.flavor).is_ok() {
                tools = tools.child(
                    button(
                        "rename",
                        ButtonKind::Ghost,
                        !locked,
                        "Rename the object",
                        cx,
                    )
                    .child("Rename…")
                    .when(!locked, |b| {
                        b.clickable(
                            cx.listener(|this, _, window, cx| this.start_rename(window, cx)),
                        )
                    }),
                );
            }
            let source = def.source.clone();
            tools = tools.child(
                button(
                    "copy-create",
                    ButtonKind::Ghost,
                    true,
                    "Copy the CREATE statement",
                    cx,
                )
                .child(if self.copied { "Copied" } else { "Copy CREATE" })
                .clickable(cx.listener(move |this, _, _, cx| this.copy(source.clone(), cx))),
            );
        }
        head.child(tools)
    }

    fn render_banners(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let line = |tone: Tone| {
            let (bg, fg) = match tone {
                Tone::Info => (p.panel, p.tx2),
                Tone::Warn => (p.warn_bg, p.warn),
                Tone::Error => (p.bg, p.danger),
            };
            div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(px(4.))
                .px(px(16.))
                .py(px(8.))
                .border_b_1()
                .border_color(p.line)
                .bg(bg)
                .text_size(small_size())
                .text_color(fg)
                .line_height(relative(1.5))
        };
        let mut out = Vec::new();
        if let Some(reason) = self.read_only() {
            out.push(
                line(Tone::Warn)
                    .child(format!(
                        "{reason} Properties are shown read-only; Apply and Drop are off."
                    ))
                    .into_any_element(),
            );
        }
        let notices = self
            .def
            .iter()
            .flat_map(|d| &d.notices)
            .chain(self.last.iter().flat_map(|o| &o.notices))
            .chain(self.plan.iter().flat_map(|p| &p.notices));
        for n in notices {
            out.push(line(Tone::Info).child(n.clone()).into_any_element());
        }
        if let Some(b) = &self.banner {
            let errors = b.errors.iter().enumerate().map(|(i, e)| {
                let at = e
                    .body_line
                    .map(|l| format!("body line {l}"))
                    .or_else(|| e.line.map(|l| format!("line {l}")));
                let e2 = e.clone();
                div()
                    .id(("banner-error", i))
                    .flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .child(SharedString::from(e.code.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(SharedString::from(e.message.clone())),
                    )
                    .when_some(at, |d, at| {
                        d.child(
                            div()
                                .id(("banner-line", i))
                                .flex_none()
                                .text_color(p.link)
                                .hover(|s| s.underline())
                                .child(at)
                                .clickable(cx.listener(move |this, _, window, cx| {
                                    this.goto(&e2, window, cx);
                                })),
                        )
                    })
            });
            let backup = b.backup.as_ref().map(|(text, path)| {
                let text = text.clone();
                let open = path.clone().map(|path| {
                    button(
                        "open-backup",
                        ButtonKind::Outline,
                        true,
                        "Open the backup file in a tab",
                        cx,
                    )
                    .child("Open backup")
                    .clickable(cx.listener(move |_, _, _, cx| {
                        cx.emit(ObjectEvent::OpenFile(path.clone()));
                    }))
                });
                div().flex().gap(px(8.)).children(open).child(
                    button(
                        "copy-backup",
                        ButtonKind::Outline,
                        true,
                        "Copy the original's CREATE",
                        cx,
                    )
                    .child(if self.copied { "Copied" } else { "Copy backup" })
                    .clickable(cx.listener(move |this, _, _, cx| this.copy(text.clone(), cx))),
                )
            });
            let steps = self.last.as_ref().filter(|o| o.steps.len() > 1).map(|o| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .text_color(p.tx2)
                    .children(o.steps.iter().enumerate().map(|(i, s)| {
                        let status = match s.status {
                            StepStatus::Done => "done",
                            StepStatus::Failed => "failed",
                            StepStatus::Skipped => "skipped",
                            StepStatus::NotRun => "not run",
                        };
                        let what = if s.description.is_empty() {
                            format!("{:?}", s.kind)
                        } else {
                            s.description.clone()
                        };
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_color(p.tx3)
                                    .child(format!("{}", i + 1)),
                            )
                            .child(div().flex_1().min_w_0().child(what))
                            .child(div().text_color(p.tx3).child(status))
                            .when(s.status != StepStatus::NotRun, |d| {
                                d.child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_color(p.tx3)
                                        .child(format!("{} ms", s.elapsed.as_millis())),
                                )
                            })
                    }))
            });
            out.push(
                line(b.tone)
                    .child(SharedString::from(b.text.clone()))
                    .children(errors)
                    .children(steps)
                    .children(backup)
                    .into_any_element(),
            );
        }
        out
    }

    fn render_field(
        &self,
        f: &Field,
        locked: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dot = !self.new && f.changed(&self.base, &self.draft);
        match &f.widget {
            Widget::Text { mono, .. } => match self.form.inputs.get(f.key) {
                Some(state) => {
                    labeled(f.label, dot, text_input(state, *mono, locked), p).into_any_element()
                }
                None => div().into_any_element(),
            },
            Widget::Select(_) => match self.form.selects.get(f.key) {
                Some(state) => labeled(
                    f.label,
                    dot,
                    Select::new(state).h(row_height()).disabled(locked),
                    p,
                )
                .into_any_element(),
                None => div().into_any_element(),
            },
            Widget::Fixed => {
                labeled(f.label, false, fixed_box((f.get)(&self.draft), p), p).into_any_element()
            }
            Widget::Check => {
                let on = (f.get)(&self.draft) == "true";
                let set = f.set;
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .flex_none()
                    .h(row_height())
                    .child(
                        Checkbox::new(SharedString::from(format!("check-{}", f.key)))
                            .checked(on)
                            .label(f.label)
                            .disabled(locked)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                set(&mut this.draft, if *checked { "true" } else { "false" });
                                this.changed(cx);
                            })),
                    )
                    .when(dot, |d| d.child(changed_dot(p)))
                    .into_any_element()
            }
        }
    }

    fn render_params(&self, locked: bool, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Properties::Routine(r) = &self.draft.props else {
            return Vec::new();
        };
        let oracle = self.flavor == Flavor::Oracle;
        let base = match &self.base.props {
            Properties::Routine(b) => b.params.as_slice(),
            _ => &[],
        };
        let add = icon_button(
            "add-param",
            Icon::Plus,
            Some("Add parameter"),
            ButtonKind::Outline,
            !locked,
            "Add a parameter at the end",
            cx,
        )
        .when(!locked, |b| {
            b.clickable(cx.listener(|this, _, window, cx| this.add_param(window, cx)))
        });
        let cell = |w: Option<f32>| {
            let d = div().px(px(4.)).min_w_0();
            match w {
                Some(w) => d.flex_none().w(px(w)),
                None => d.flex_1().flex_basis(px(0.)),
            }
        };
        let mut head = div()
            .flex()
            .items_center()
            .h(px(28.))
            .px(px(12.))
            .text_size(small_size())
            .text_color(p.tx2)
            .border_b_1()
            .border_color(p.line)
            .child(cell(Some(32.)).child("#"))
            .child(cell(Some(110.)).child("Mode"))
            .child(cell(None).child("Name"))
            .child(cell(None).child("Type"));
        if oracle {
            head = head
                .child(cell(None).child("Default"))
                .child(cell(Some(80.)).child("NOCOPY"));
        }
        head = head.child(cell(Some(36.)));
        let mut rows = vec![head.into_any_element()];
        for (i, (param, row)) in r.params.iter().zip(&self.form.params).enumerate() {
            let added = !self.new && base.get(i) != Some(param);
            let mode = match &row.mode {
                Some(m) => Select::new(m)
                    .h(row_height())
                    .disabled(locked)
                    .into_any_element(),
                None => fixed_box("IN".into(), p).into_any_element(),
            };
            let mut line = div()
                .id(("param", i))
                .flex()
                .items_center()
                .py(px(3.))
                .px(px(12.))
                .border_b_1()
                .border_color(p.line)
                .child(
                    cell(Some(32.))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .font_family(MONO_FONT)
                        .text_color(p.tx3)
                        .child(format!("{}", i + 1))
                        .when(added, |d| d.child(changed_dot(p))),
                )
                .child(cell(Some(110.)).child(mode))
                .child(cell(None).child(text_input(&row.name, true, locked)))
                .child(cell(None).child(text_input(&row.data_type, true, locked)));
            if oracle {
                let in_only = param.mode == ParamMode::In;
                line = line
                    .child(cell(None).child(text_input(&row.default, true, locked || !in_only)))
                    .child(
                        cell(Some(80.)).child(
                            Checkbox::new(("nocopy", i))
                                .checked(param.nocopy)
                                .disabled(locked || in_only)
                                .on_click(cx.listener(move |this, c: &bool, _, cx| {
                                    let c = *c;
                                    this.edit_param(i, |p| p.nocopy = c);
                                    this.changed(cx);
                                })),
                        ),
                    );
            }
            line = line.child(
                cell(Some(36.)).child(
                    div()
                        .id(("remove-param", i))
                        .size(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .when(!locked, |d| {
                            d.hover(|s| s.bg(p.hover)).clickable(cx.listener(
                                move |this, _, window, cx| this.remove_param(i, window, cx),
                            ))
                        })
                        .child(icon(Icon::Close, p.tx3).size(px(12.))),
                ),
            );
            rows.push(line.into_any_element());
        }
        let changed = !self.new && base != r.params.as_slice();
        let mode_hint = if oracle {
            "Mode: IN, OUT, IN OUT. Types take no length. Default and NOCOPY: IN only and OUT / IN OUT only."
        } else if self.draft.object.kind == ObjectKind::Function {
            "Functions take IN only. Row order is parameter order."
        } else {
            "Mode: IN, OUT, INOUT. Row order is parameter order."
        };
        vec![
            section_head("Parameters", changed, Some(add.into_any_element()), p).into_any_element(),
            div()
                .mx(px(16.))
                .border_t_1()
                .border_color(p.line)
                .children(rows)
                .into_any_element(),
            div()
                .px(px(16.))
                .pt(px(6.))
                .child(hint(mode_hint, p))
                .into_any_element(),
        ]
    }

    fn render_properties(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let locked = self.locked();
        let mut page = div().flex().flex_col().pb(px(24.));
        let routine = form::has_params(&self.draft);
        for (gi, g) in form::layout(self.flavor, &self.draft, self.new)
            .into_iter()
            .enumerate()
        {
            let right = g
                .hint
                .map(|h| hint(h, p).max_w(px(520.)).into_any_element());
            page = page.child(section_head(g.title, false, right, p));
            for row in &g.rows {
                let fields = row
                    .iter()
                    .map(|f| self.render_field(f, locked, p, cx))
                    .collect();
                page = page.child(fields_row(fields));
            }
            if routine && gi == 0 {
                page = page.children(self.render_params(locked, p, cx));
            }
        }
        if let Some(def) = self.def.as_ref().filter(|d| d.siblings.len() > 1)
            && let Properties::Trigger(t) = &def.props
        {
            let when = format!(
                "{} {} on {}",
                t.timing.sql(),
                t.events
                    .iter()
                    .map(|e| e.sql())
                    .collect::<Vec<_>>()
                    .join(" OR "),
                t.table
            );
            page = page
                .child(section_head(
                    when,
                    false,
                    Some(hint("Runs in this order.", p).into_any_element()),
                    p,
                ))
                .children(def.siblings.iter().enumerate().map(|(i, s)| {
                    let me = *s == def.object.name;
                    div()
                        .flex()
                        .gap(px(12.))
                        .px(px(16.))
                        .py(px(2.))
                        .font_family(MONO_FONT)
                        .text_size(data_size())
                        .child(div().text_color(p.tx3).child(format!("{}", i + 1)))
                        .child(s.clone())
                        .when(me, |d| d.child(div().text_color(p.tx3).child("this one")))
                }));
        }
        if let (Some((title, get, _)), Some(editor)) = (form::body(&self.draft), &self.editor) {
            let dot = !self.new && get(&self.base) != get(&self.draft);
            let empty = self
                .def
                .as_ref()
                .is_some_and(|d| d.read_only.is_some() && get(&self.draft).is_empty());
            page = page
                .child(section_head(
                    title,
                    dot,
                    Some(
                        hint(
                            "SQL editor. Ctrl+Space completes tables, columns and keywords.",
                            p,
                        )
                        .into_any_element(),
                    ),
                    p,
                ))
                .child(
                    div()
                        .mx(px(16.))
                        .h(px(320.))
                        .border_1()
                        .border_color(p.line2)
                        .rounded(RADIUS)
                        .overflow_hidden()
                        .when(empty, |d| {
                            d.child(
                                div()
                                    .p(px(12.))
                                    .text_color(p.tx3)
                                    .child("Body not available"),
                            )
                        })
                        .when(!empty, |d| d.child(editor_box(editor, locked, p))),
                );
        }
        div()
            .id("object-page")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(page)
            .into_any_element()
    }

    fn render_source(&self, p: &Palette) -> AnyElement {
        let text = self
            .def
            .as_ref()
            .map_or(String::new(), |d| d.source.clone());
        div()
            .id("object-source-page")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .px(px(16.))
            .py(px(16.))
            .font_family(MONO_FONT)
            .text_size(data_size())
            .line_height(px(21.))
            .whitespace_nowrap()
            .child(div().flex_none().child(highlighted(&text, p)))
            .into_any_element()
    }

    fn render_errors(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let errors = self.errors().to_vec();
        let rows = errors.into_iter().enumerate().map(|(i, e)| {
            let at = match (e.line, e.position) {
                (Some(l), Some(c)) => format!("{l}:{c}"),
                (Some(l), None) => l.to_string(),
                _ => String::new(),
            };
            let part = e.part.clone();
            let warn = e.warning;
            let code = e.code.clone();
            let message = e.message.clone();
            div()
                .id(("object-error", i))
                .flex()
                .gap(px(12.))
                .px(px(16.))
                .py(px(6.))
                .border_b_1()
                .border_color(p.line)
                .hover(|s| s.bg(p.hover))
                .child(
                    div()
                        .w(px(64.))
                        .flex_none()
                        .font_family(MONO_FONT)
                        .text_color(p.link)
                        .child(at),
                )
                .child(div().w(px(110.)).flex_none().text_color(p.tx3).child(part))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(!warn, |d| d.text_color(p.danger))
                        .child(div().font_family(MONO_FONT).child(code))
                        .child(message),
                )
                .clickable(cx.listener(move |this, _, window, cx| this.goto(&e, window, cx)))
        });
        let empty = self.errors().is_empty();
        div()
            .id("object-errors-page")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(section_head(
                "Errors",
                false,
                Some(
                    hint(
                        "ALL_ERRORS, after the last compile. Click one to put the cursor there.",
                        p,
                    )
                    .into_any_element(),
                ),
                p,
            ))
            .when(empty, |d| {
                d.child(
                    div()
                        .px(px(16.))
                        .text_color(p.tx3)
                        .child("No compile errors."),
                )
            })
            .children(rows)
            .into_any_element()
    }

    fn render_footer(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let locked = self.locked();
        let running = self.running.is_some();
        let k = crate::keymap::label("save", cx);
        let chevron = if self.footer_open {
            Icon::ChevronDown
        } else {
            Icon::ChevronRight
        };
        let toggle = button("statements-toggle", ButtonKind::Ghost, true, "", cx)
            .child(icon(chevron, p.tx2))
            .child("Statements to run")
            .clickable(cx.listener(|this, _, _, cx| {
                this.footer_open = !this.footer_open;
                cx.notify();
            }));
        let dim = |t: String| div().text_color(p.tx3).whitespace_nowrap().child(t);
        let pill = |t: &'static str, warn: bool| {
            div()
                .flex_none()
                .px(px(6.))
                .rounded(px(4.))
                .border_1()
                .border_color(if warn { p.warn_line } else { p.line2 })
                .when(warn, |d| d.bg(p.warn_bg))
                .text_color(if warn { p.warn } else { p.tx2 })
                .text_size(px(11.))
                .font_weight(FontWeight::MEDIUM)
                .child(t)
        };
        let mut bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(12.))
            .gap_y(px(6.))
            .px(px(12.))
            .py(px(8.))
            .child(toggle);
        let ready = match &self.plan {
            Ok(plan) if plan.is_empty() => {
                bar = bar.child(dim("No changes".into()));
                false
            }
            Ok(plan) => {
                bar = bar
                    .child(dim(form::count_label(plan)))
                    .when(!plan.verb().is_empty(), |d| {
                        d.child(pill(plan.verb(), false))
                    })
                    .when(!plan.atomic, |d| d.child(pill("Not atomic", true)))
                    .children(form::changes_label(plan).map(dim));
                true
            }
            Err(e) if e.is_empty() => false,
            Err(e) => {
                bar = bar.child(dim(e.clone()));
                false
            }
        };
        let can_apply = ready && !locked;
        let tip = format!(
            "Runs the statements below on {} ({k}){}",
            self.connection.name,
            match self.connection.environment {
                Environment::Production => "; Production asks first",
                Environment::Staging => "; Stage asks first when not atomic",
                Environment::Development => "",
            }
        );
        let mut buttons = div().flex().items_center().gap(px(8.)).ml_auto();
        if self.new {
            buttons = buttons.child(
                button(
                    "object-discard",
                    ButtonKind::Outline,
                    !running,
                    "Close without creating",
                    cx,
                )
                .child("Discard")
                .when(!running, |b| {
                    b.clickable(cx.listener(|_, _, _, cx| cx.emit(ObjectEvent::Close)))
                }),
            );
        } else {
            let drop_ok = !locked;
            buttons = buttons.child(
                button(
                    "object-drop",
                    ButtonKind::Outline,
                    drop_ok,
                    "Drop the object (asks first)",
                    cx,
                )
                .text_color(p.danger)
                .child("Drop…")
                .when(drop_ok, |b| {
                    b.clickable(cx.listener(|this, _, window, cx| this.ask_drop(window, cx)))
                }),
            );
            if self.banner.as_ref().is_some_and(|b| b.revert) {
                buttons = buttons.child(
                    button(
                        "object-revert-valid",
                        ButtonKind::Outline,
                        !running,
                        "Puts back the source held from before the last Apply",
                        cx,
                    )
                    .child("Revert to last valid")
                    .when(!running, |b| {
                        b.clickable(
                            cx.listener(|this, _, window, cx| this.revert_to_valid(window, cx)),
                        )
                    }),
                );
            }
            let revert_ok = self.dirty() && !running;
            buttons = buttons.child(
                button(
                    "object-revert",
                    ButtonKind::Outline,
                    revert_ok,
                    "Throw the edits away",
                    cx,
                )
                .child("Revert")
                .when(revert_ok, |b| {
                    b.clickable(cx.listener(|this, _, window, cx| this.revert(window, cx)))
                }),
            );
        }
        let label = match (running, self.new) {
            (true, _) => "Applying…",
            (false, true) => "Create",
            (false, false) => "Apply",
        };
        buttons = buttons.child(
            button("object-apply", ButtonKind::Primary, can_apply, tip, cx)
                .child(label)
                .when(can_apply, |b| {
                    b.clickable(cx.listener(|this, _, window, cx| this.apply(window, cx)))
                }),
        );
        bar = bar.child(buttons);
        let running_steps = self.running.as_ref().map(|(_, plan)| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(16.))
                .pb(px(8.))
                .text_size(small_size())
                .text_color(p.tx2)
                .children(plan.steps.iter().map(|s| {
                    let what = if s.description.is_empty() {
                        s.statements.first().cloned().unwrap_or_default()
                    } else {
                        s.description.clone()
                    };
                    let only = if s.conditional() {
                        " (only if needed)"
                    } else {
                        ""
                    };
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(format!("{what}{only}…"))
                }))
                .child(hint("Fields and editor are locked until it finishes.", p))
        });
        let preview = (self.footer_open
            && !self.preview.is_empty()
            && self.plan.as_ref().is_ok_and(|p| !p.is_empty()))
        .then(|| {
            div()
                .id("object-preview")
                .mx(px(12.))
                .mb(px(10.))
                .max_h(px(260.))
                .overflow_scroll()
                .px(px(12.))
                .py(px(8.))
                .bg(p.bg)
                .border_1()
                .border_color(p.line2)
                .rounded(RADIUS)
                .font_family(MONO_FONT)
                .text_size(data_size())
                .line_height(px(20.))
                .whitespace_nowrap()
                .child(div().flex_none().child(highlighted(&self.preview, p)))
        });
        let folded = (!self.footer_open && self.source_first() && ready).then(|| {
            div().px(px(16.)).pb(px(8.)).child(hint(
                "Oracle compiles on create. If compiling fails, the new source is still \
                     saved, marked INVALID, and its errors land in the Errors tab: nothing is \
                     restored automatically.",
                p,
            ))
        });
        div()
            .flex()
            .flex_col()
            .flex_none()
            .border_t_1()
            .border_color(p.line)
            .bg(p.panel)
            .child(bar)
            .children(running_steps)
            .children(preview)
            .children(folded)
    }
}

impl Render for ObjectTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .key_context("ObjectEditor");
        match &self.load {
            Load::Loading => {
                let bar = |w: f32| div().h(px(12.)).w(relative(w)).rounded(px(4.)).bg(p.hover);
                return root
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .p(px(16.))
                            .child(hint(
                                format!(
                                    "Reading {} from {}…",
                                    self.object.name, self.connection.name
                                ),
                                &p,
                            ))
                            .child(bar(0.6))
                            .child(bar(0.8))
                            .child(bar(0.4))
                            .child(bar(0.7)),
                    )
                    .into_any_element();
            }
            Load::Failed(e) => {
                let e = e.clone();
                return root
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap(px(12.))
                            .p(px(16.))
                            .child(
                                div()
                                    .text_color(p.danger)
                                    .child(format!("Reading {} failed: {e}", self.object.name)),
                            )
                            .child(
                                button(
                                    "object-retry",
                                    ButtonKind::Outline,
                                    true,
                                    "Read it again",
                                    cx,
                                )
                                .child("Retry")
                                .clickable(
                                    cx.listener(|this, _, window, cx| {
                                        this.reload(true, window, cx)
                                    }),
                                ),
                            ),
                    )
                    .into_any_element();
            }
            Load::Ready => {}
        }
        let locked = self.locked();
        let body = match self.section {
            Section::Properties => self.render_properties(&p, cx),
            Section::Source => self.render_source(&p),
            Section::Errors => self.render_errors(&p, cx),
            Section::Spec | Section::Body => {
                let editor = if self.section == Section::Body {
                    self.editor2.as_ref()
                } else {
                    self.editor.as_ref()
                };
                let title = match (self.section, self.object.kind) {
                    (Section::Body, ObjectKind::Type) => "Type body",
                    (Section::Body, _) => "Package body",
                    (_, ObjectKind::Package) => "Package spec",
                    (_, ObjectKind::Type) => "Type spec",
                    _ => "Source",
                };
                let (get, _) = if self.section == Section::Body {
                    form::source_body()
                } else {
                    form::body(&self.draft).map_or(form::source_body(), |(_, g, s)| (g, s))
                };
                let dot = !self.new && get(&self.base) != get(&self.draft);
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(section_head(
                        title,
                        dot,
                        Some(
                            hint(
                                "PL/SQL editor. Ctrl+Space completes tables, columns and keywords.",
                                &p,
                            )
                            .into_any_element(),
                        ),
                        &p,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .mx(px(16.))
                            .mb(px(12.))
                            .border_1()
                            .border_color(p.line2)
                            .rounded(RADIUS)
                            .overflow_hidden()
                            .children(editor.map(|e| editor_box(e, locked, &p))),
                    )
                    .into_any_element()
            }
        };
        root.child(self.render_header(&p, cx))
            .children(self.render_banners(&p, cx))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(self.render_footer(&p, cx))
            .into_any_element()
    }
}
