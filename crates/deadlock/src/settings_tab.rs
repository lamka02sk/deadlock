//! The Settings tab from the design's `Settings` artboard: Project (this project's name,
//! color, pattern, connections and Delete project), General (appearance, editor, results, safety and the first
//! shortcuts) and Shortcuts (every binding). Each change is saved and applied at once.

use std::path::PathBuf;

use deadlock_core::connection::{ConnectionConfig, Engine, Environment};
use deadlock_core::project::Pattern;
use deadlock_core::settings::{
    EDITOR_SIZES, Indent, KeywordCase, PAGE_SIZES, ResultsPlace, Settings, Theme, TxDefault,
    UI_SIZES,
};
use gpui_kit::component::IndexPath;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    WeakEntity, Window, div, px, relative, white,
};

use crate::assets::{Icon, icon};
use crate::keymap::{self, ENTRIES, Entry};
use crate::settings::{self, get};
use crate::shell::{Shell, pattern_layer};
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, row_height, small_size};
use crate::ui::Clickable as _;

type Choice = Entity<SelectState<Vec<SharedString>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    /// This project's own settings, kept in its `project.toml`.
    Project,
    General,
    Shortcuts,
}

/// The General section shows this many shortcuts; the rest are in Shortcuts.
const GENERAL_SHORTCUTS: [&str; 4] = ["run", "run-all", "console", "search"];

/// A shortcut row waiting for its new keys.
struct Capture {
    id: &'static str,
    /// Keys already used by another action: shown with Swap.
    conflict: Option<(String, &'static Entry)>,
    _intercept: Subscription,
}

pub struct SettingsTab {
    section: Section,
    ui_size: Choice,
    editor_size: Choice,
    indent: Choice,
    keywords: Choice,
    page_size: Choice,
    null_text: Entity<InputState>,
    capture: Option<Capture>,
    /// Held while a shortcut is captured.
    focus: gpui_kit::FocusHandle,
    /// The project whose history Clear history deletes.
    root: PathBuf,
    /// The window's shell, which holds the project's `project.toml`.
    shell: WeakEntity<Shell>,
    /// The project's display name, saved as it is typed.
    name: Entity<InputState>,
    /// The last Project change could not be written.
    look_error: Option<String>,
    _subs: Vec<Subscription>,
}

/// The select's row for `value` among `options`.
fn row_of<T: PartialEq>(options: &[T], value: &T) -> usize {
    options.iter().position(|o| o == value).unwrap_or(0)
}

fn indent_row(i: Indent) -> usize {
    match i {
        Indent::Two => 0,
        Indent::Four => 1,
        Indent::Tab => 2,
    }
}

fn case_row(k: KeywordCase) -> usize {
    match k {
        KeywordCase::Lower => 0,
        KeywordCase::Upper => 1,
        KeywordCase::Keep => 2,
    }
}

impl SettingsTab {
    /// `name` is the project's display name as saved; the tab is made inside an update
    /// of `shell`, so it cannot read it there.
    pub fn new(
        root: PathBuf,
        shell: WeakEntity<Shell>,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let s = get(cx).clone();
        let mut subs = Vec::new();
        let mut choice = |labels: Vec<SharedString>,
                          at: usize,
                          set: fn(&mut Settings, usize),
                          window: &mut Window,
                          cx: &mut Context<Self>| {
            let state = cx
                .new(|cx| SelectState::new(labels, Some(IndexPath::default().row(at)), window, cx));
            subs.push(cx.subscribe(
                &state,
                move |_, s, _: &SelectEvent<Vec<SharedString>>, cx| {
                    let Some(row) = s.read(cx).selected_index(cx).map(|i| i.row) else {
                        return;
                    };
                    settings::update(cx, |st| set(st, row));
                },
            ));
            state
        };
        let px_labels = |sizes: &[u8]| {
            sizes
                .iter()
                .map(|n| format!("{n} px").into())
                .collect::<Vec<_>>()
        };
        let ui_size = choice(
            px_labels(&UI_SIZES),
            row_of(&UI_SIZES, &s.ui_size),
            |s, r| s.ui_size = UI_SIZES[r.min(UI_SIZES.len() - 1)],
            window,
            cx,
        );
        let editor_size = choice(
            px_labels(&EDITOR_SIZES),
            row_of(&EDITOR_SIZES, &s.editor_size),
            |s, r| s.editor_size = EDITOR_SIZES[r.min(EDITOR_SIZES.len() - 1)],
            window,
            cx,
        );
        let indent = choice(
            vec!["2 spaces".into(), "4 spaces".into(), "Tab".into()],
            indent_row(s.indent),
            |s, r| {
                s.indent = match r {
                    0 => Indent::Two,
                    1 => Indent::Four,
                    _ => Indent::Tab,
                }
            },
            window,
            cx,
        );
        let keywords = choice(
            vec![
                "lower case".into(),
                "UPPER CASE".into(),
                "Leave as typed".into(),
            ],
            case_row(s.keyword_case),
            |s, r| {
                s.keyword_case = match r {
                    0 => KeywordCase::Lower,
                    1 => KeywordCase::Upper,
                    _ => KeywordCase::Keep,
                }
            },
            window,
            cx,
        );
        let page_size = choice(
            PAGE_SIZES
                .iter()
                .map(|n| crate::export::grouped(*n as usize).into())
                .collect(),
            row_of(&PAGE_SIZES, &s.page_size),
            |s, r| s.page_size = PAGE_SIZES[r.min(PAGE_SIZES.len() - 1)],
            window,
            cx,
        );
        let null_text = cx.new(|cx| InputState::new(window, cx).default_value(s.null_text.clone()));
        subs.push(cx.subscribe(&null_text, |_, input, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                let text = input.read(cx).value().to_string();
                settings::update(cx, |s| s.null_text = text);
            }
        }));
        let folder = deadlock_core::project::folder_name(&root);
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(folder)
                .default_value(name.unwrap_or_default())
        });
        subs.push(
            cx.subscribe_in(&name, window, |this, _, e: &InputEvent, window, cx| {
                // Saved on every change: a window closed right after typing keeps it.
                if matches!(e, InputEvent::Change) {
                    this.set_look(|_, _| {}, window, cx);
                }
            }),
        );
        Self {
            section: Section::General,
            ui_size,
            editor_size,
            indent,
            keywords,
            page_size,
            null_text,
            capture: None,
            root,
            shell,
            name,
            look_error: None,
            focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    pub(crate) fn show(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    /// The project's name, color and pattern as saved.
    fn look(&self, cx: &App) -> Option<(Option<String>, Option<String>, Pattern)> {
        let config = &self.shell.upgrade()?.read(cx).project().config;
        Some((config.name.clone(), config.color.clone(), config.pattern))
    }

    /// Saves the typed name with the saved color and pattern, `change` applied, through
    /// the shell; a failed write is shown in the section. Nothing changed: no write.
    fn set_look(
        &mut self,
        change: impl FnOnce(&mut Option<String>, &mut Pattern),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(shell), Some(saved)) = (self.shell.upgrade(), self.look(cx)) else {
            return;
        };
        let (mut color, mut pattern) = (saved.1.clone(), saved.2);
        change(&mut color, &mut pattern);
        let typed = self.name.read(cx).value().trim().to_owned();
        let typed = (!typed.is_empty()).then_some(typed);
        if (&typed, &color, pattern) == (&saved.0, &saved.1, saved.2) {
            return;
        }
        let saved = shell.update(cx, |s, cx| s.set_look(typed, color, pattern, window, cx));
        self.look_error = saved
            .err()
            .map(|e| format!("The project settings cannot be saved: {e}"));
        cx.notify();
    }

    /// Keeps the selects in step with settings changed elsewhere (another window).
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = get(cx).clone();
        let want = [
            (&self.ui_size, row_of(&UI_SIZES, &s.ui_size)),
            (&self.editor_size, row_of(&EDITOR_SIZES, &s.editor_size)),
            (&self.indent, indent_row(s.indent)),
            (&self.keywords, case_row(s.keyword_case)),
            (&self.page_size, row_of(&PAGE_SIZES, &s.page_size)),
        ];
        for (state, row) in want {
            if state.read(cx).selected_index(cx).map(|i| i.row) != Some(row) {
                state.update(cx, |st, cx| {
                    st.set_selected_index(Some(IndexPath::default().row(row)), window, cx);
                });
            }
        }
    }

    /// Change: the next key combination becomes the binding of `id`. Focus leaving
    /// the tab (a click elsewhere, another window) ends the capture.
    fn start_capture(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let this = cx.entity().downgrade();
        let focus = self.focus.clone();
        let intercept = cx.intercept_keystrokes(move |e, window, cx| {
            if !focus.is_focused(window) {
                let _ = this.update(cx, |t, cx| {
                    t.capture = None;
                    cx.notify();
                });
                return;
            }
            cx.stop_propagation();
            let key = e.keystroke.clone();
            let _ = this.update(cx, |t, cx| t.captured(&key, cx));
        });
        self.capture = Some(Capture {
            id,
            conflict: None,
            _intercept: intercept,
        });
        cx.notify();
    }

    fn captured(&mut self, key: &gpui_kit::Keystroke, cx: &mut Context<Self>) {
        let Some(c) = &mut self.capture else { return };
        if key.key == "escape" && !key.modifiers.modified() {
            self.capture = None;
            cx.notify();
            return;
        }
        let Some(keys) = keymap::keystroke_text(key) else {
            return;
        };
        if let Some(other) = keymap::conflict(c.id, &keys, cx) {
            c.conflict = Some((keys, other));
            cx.notify();
            return;
        }
        let id = c.id;
        self.capture = None;
        set_keys(id, &keys, cx);
    }

    /// Swap: the conflicting action takes this one's keys.
    fn swap(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.capture.take() else { return };
        let Some((keys, other)) = c.conflict else {
            return;
        };
        let mine = keymap::keys(c.id, cx);
        let (id, other_id) = (c.id, other.id);
        settings::update(cx, |s| {
            override_keys(s, id, &keys);
            override_keys(s, other_id, &mine);
        });
        cx.notify();
    }

    fn reset_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                "Reset every shortcut to its default?",
                &["Reset all", "Cancel"],
            )
            .text("Your own key bindings are removed.")
            .danger(true),
        );
        cx.spawn(async move |_, cx| {
            if answer.await == Some(0) {
                cx.update(|cx| settings::update(cx, |s| s.shortcuts.clear()));
            }
        })
        .detach();
    }

    fn clear_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.shell.upgrade().map_or_else(
            || deadlock_core::project::folder_name(&self.root),
            |s| s.read(cx).project().name(),
        );
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Clear the query history of {name}?"),
                &["Clear history", "Cancel"],
            )
            .text("Every statement recorded for this project is deleted. This cannot be undone.")
            .danger(true),
        );
        let root = self.root.clone();
        cx.spawn(async move |_, cx| {
            if answer.await != Some(0) {
                return;
            }
            cx.update(|cx| {
                let Some(db) = cx.global::<crate::app::HistoryDb>().0.clone() else {
                    return;
                };
                let rx = cx.global::<crate::app::Db>().0.spawn_blocking(move || {
                    db.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clear(&root)
                });
                cx.spawn(async move |cx| {
                    if let Ok(Err(e)) = rx.await {
                        eprintln!("deadlock: history: {e}");
                    }
                    // Open History panels list the empty history.
                    cx.update(crate::shell::history_cleared);
                })
                .detach();
            });
        })
        .detach();
    }
}

/// Stores `keys` for `id`, or drops the override when they are the default.
fn override_keys(s: &mut Settings, id: &str, keys: &str) {
    if keymap::entry(id).is_some_and(|e| e.default == keys) {
        s.shortcuts.remove(id);
    } else {
        s.shortcuts.insert(id.to_owned(), keys.to_owned());
    }
}

fn set_keys(id: &str, keys: &str, cx: &mut App) {
    settings::update(cx, |s| override_keys(s, id, keys));
}

/// `.b.bo` buttons, filled with `--tx` while pressed.
fn seg_button(
    id: SharedString,
    label: &'static str,
    on: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (bg, fg, border, hover) = if on {
        (p.tx, p.bg, p.tx, p.tx)
    } else {
        (p.bg, p.tx, p.line2, p.hover)
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .h(row_height())
        .px(px(8.))
        .border_1()
        .border_color(border)
        .rounded(RADIUS)
        .bg(bg)
        .text_color(fg)
        .whitespace_nowrap()
        .hover(move |s| s.bg(hover))
        .child(label)
}

/// A label and its help text (`.lbl`).
fn label(
    title: &str,
    help: Option<String>,
    help_color: Option<gpui_kit::Hsla>,
    p: &Palette,
) -> gpui_kit::Div {
    div()
        .flex_grow(1.)
        .flex_basis(px(260.))
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(SharedString::from(title.to_owned()))
        .when_some(help, |d, h| {
            d.child(
                div()
                    .text_size(small_size())
                    .text_color(help_color.unwrap_or(p.tx3))
                    .child(h),
            )
        })
}

/// A setting row (`.row`).
fn row(p: &Palette) -> gpui_kit::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(24.))
        .gap_y(px(6.))
        .min_h(px(44.))
        .py(px(6.))
        .border_b_1()
        .border_color(p.line)
}

/// A fieldset with its legend.
fn fieldset(legend: &'static str, rows: Vec<AnyElement>) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .child(
            div()
                .mb(px(4.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(legend),
        )
        .children(rows)
}

/// A key chip (`.kb`).
fn kb(keys: &str, capturing: bool, p: &Palette) -> gpui_kit::Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(2.))
        .border_1()
        .border_color(if capturing { p.blue } else { p.line2 })
        .rounded(px(4.))
        .bg(p.panel)
        .font_family(MONO_FONT)
        .text_size(small_size())
        .when(capturing, |d| {
            d.shadow(vec![gpui_kit::BoxShadow {
                color: p.sel,
                offset: gpui_kit::point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(2.),
                inset: false,
            }])
        })
        .child(keymap::display(keys))
}

fn small_button(
    id: impl Into<gpui_kit::ElementId>,
    label: SharedString,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(row_height())
        .px(px(8.))
        .border_1()
        .border_color(p.line2)
        .rounded(RADIUS)
        .text_color(p.tx)
        .whitespace_nowrap()
        .hover(|s| s.bg(p.hover))
        .child(label)
}

fn checkbox(id: &'static str, on: bool, set: fn(&mut Settings, bool)) -> Checkbox {
    Checkbox::new(id)
        .checked(on)
        .cursor_pointer()
        .on_click(move |checked: &bool, _, cx| {
            let v = *checked;
            settings::update(cx, |s| set(s, v));
        })
}

/// A Pattern tile: the pattern in `color` over its tint, the label under it, ringed
/// while chosen.
fn pattern_tile(pattern: Pattern, color: gpui_kit::Hsla, on: bool, p: &Palette) -> gpui_kit::Div {
    let preview = div()
        .relative()
        .w(px(64.))
        .h(px(32.))
        .overflow_hidden()
        .rounded(RADIUS)
        .border_2()
        .border_color(if on { p.tx } else { p.line2 })
        .bg(crate::color::over(color, crate::shell::BAND_ALPHA, p.panel))
        .children(pattern_layer(pattern, color, p).map(|l| l.right_0()));
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.))
        .text_size(small_size())
        .text_color(if on { p.tx } else { p.tx2 })
        .child(preview)
        .child(pattern.label())
}

/// `user@host:port` as the Connections block shows it; Redis and a blank user leave
/// the user out.
fn short_address(c: &ConnectionConfig) -> String {
    if c.engine == Engine::Redis || c.user.is_empty() {
        format!("{}:{}", c.host, c.port)
    } else {
        format!("{}@{}:{}", c.user, c.host, c.port)
    }
}

/// A grey tag beside a connection's name (`Read-only`).
fn tag(text: &'static str, p: &Palette) -> gpui_kit::Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .border_1()
        .border_color(p.line2)
        .rounded(px(4.))
        .text_size(small_size())
        .text_color(p.tx2)
        .child(text)
}

impl SettingsTab {
    /// Runs `f` on the window's shell.
    fn on_shell(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Shell, &mut Window, &mut Context<Shell>),
    ) {
        if let Some(shell) = self.shell.upgrade() {
            shell.update(cx, |s, cx| f(s, window, cx));
        }
    }

    /// The Connections block: New connection, then each connection in sidebar order
    /// with Edit, Duplicate and Delete.
    fn connections(&self, cx: &mut Context<Self>) -> gpui_kit::Div {
        let p = *cx.global::<Palette>();
        let Some(shell) = self.shell.upgrade() else {
            return div();
        };
        let list = shell.read(cx).project().config.connections.clone();
        let head = row(&p)
            .child(label(
                "Servers this project connects to",
                Some("Saved in project.toml; passwords stay in the system keychain.".into()),
                None,
                &p,
            ))
            .child(
                small_button("project-new-connection", "New connection".into(), &p).clickable(
                    cx.listener(|this, _, window, cx| {
                        this.on_shell(window, cx, |s, window, cx| s.new_connection(window, cx));
                    }),
                ),
            );
        let empty = list
            .is_empty()
            .then(|| row(&p).text_color(p.tx3).child("No connections yet."));
        let rows = list.into_iter().enumerate().map(|(i, c)| {
            let color = c.color.as_deref().and_then(crate::color::parse_hex);
            let action =
                |id: &'static str,
                 text: &'static str,
                 run: fn(&mut Shell, &str, &mut Window, &mut Context<Shell>)| {
                    let conn = c.id.clone();
                    small_button((id, i), text.into(), &p).clickable(cx.listener(
                        move |this, _, window, cx| {
                            let conn = conn.clone();
                            this.on_shell(window, cx, move |s, window, cx| {
                                run(s, &conn, window, cx)
                            });
                        },
                    ))
                };
            row(&p)
                .child(
                    div()
                        .flex_grow(1.)
                        .flex_basis(px(260.))
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(crate::ui::dot(color.unwrap_or(p.tx3)))
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(c.name.clone()),
                                )
                                .when(c.environment == Environment::Production, |d| {
                                    d.child(crate::dialog::production_badge(&p))
                                })
                                .when(c.read_only, |d| d.child(tag("Read-only", &p))),
                        )
                        .child(
                            div()
                                .pl(px(14.))
                                .text_size(small_size())
                                .text_color(p.tx3)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(format!("{} · {}", c.engine.label(), short_address(&c))),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(action("project-conn-edit", "Edit", |s, id, w, cx| {
                            s.edit_connection(id, true, w, cx);
                        }))
                        .child(action(
                            "project-conn-duplicate",
                            "Duplicate",
                            |s, id, w, cx| {
                                s.edit_connection(id, false, w, cx);
                            },
                        ))
                        .child(
                            action("project-conn-delete", "Delete", |s, id, w, cx| {
                                s.delete_connection(id, w, cx);
                            })
                            .text_color(p.danger),
                        ),
                )
        });
        let mut all: Vec<gpui_kit::Div> = std::iter::once(head).chain(empty).chain(rows).collect();
        if let Some(last) = all.pop() {
            all.push(last.border_b_0());
        }
        fieldset(
            "Connections",
            all.into_iter().map(IntoElement::into_any_element).collect(),
        )
    }

    /// The Danger zone: Delete project….
    fn danger_zone(&self, cx: &mut Context<Self>) -> gpui_kit::Div {
        let p = *cx.global::<Palette>();
        fieldset(
            "Danger zone",
            vec![
                row(&p)
                    .border_b_0()
                    .child(label(
                        "Delete project",
                        Some(
                            "Removes Deadlock's data for this folder: connections, saved \
                             passwords, query history, schema cache, diagrams and consoles. \
                             The folder and your files stay."
                                .into(),
                        ),
                        None,
                        &p,
                    ))
                    .child(
                        small_button("project-delete", "Delete project…".into(), &p)
                            .text_color(p.danger)
                            .clickable(cx.listener(|this, _, window, cx| {
                                this.on_shell(window, cx, |s, window, cx| {
                                    s.delete_project(window, cx);
                                });
                            })),
                    )
                    .into_any_element(),
            ],
        )
    }

    /// The Project section: name, color and pattern, connections, Delete project.
    fn project(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = *cx.global::<Palette>();
        let Some((_, color, pattern)) = self.look(cx) else {
            return Vec::new();
        };
        let swatches = std::iter::once(("None", None)).chain(
            crate::color::PALETTE
                .iter()
                .map(|(name, hex)| (*name, Some(*hex))),
        );
        let swatches =
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .children(swatches.map(|(label, hex)| {
                    let on = color.as_deref().map(str::to_lowercase).as_deref() == hex;
                    let fill = hex.and_then(crate::color::parse_hex);
                    div()
                        .id(SharedString::from(format!("project-color-{label}")))
                        .size(px(24.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_2()
                        .border_color(if on { p.tx } else { p.line2 })
                        .when(hex.is_none(), |d| {
                            d.child(icon(Icon::Close, p.tx3).size(px(12.)))
                        })
                        .when_some(fill, |d, c| {
                            d.bg(c)
                                .when(on, |d| d.child(icon(Icon::Commit, white()).size(px(12.))))
                        })
                        .tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx)
                        })
                        .clickable(cx.listener(move |this, _, window, cx| {
                            this.set_look(|c, _| *c = hex.map(str::to_owned), window, cx);
                        }))
                }));
        // Without a color the tiles show the pattern in grey; it shows once a color is set.
        let tint = color
            .as_deref()
            .and_then(crate::color::parse_hex)
            .unwrap_or(p.tx3);
        let tiles = div().flex().gap(px(8.)).children(Pattern::ALL.map(|pat| {
            div()
                .id(SharedString::from(format!(
                    "project-pattern-{}",
                    pat.label()
                )))
                .child(pattern_tile(pat, tint, pat == pattern, &p))
                .clickable(cx.listener(move |this, _, window, cx| {
                    this.set_look(|_, p| *p = pat, window, cx);
                }))
        }));
        let look = fieldset(
            "Name and color",
            vec![
                row(&p)
                    .child(label(
                        "Name",
                        Some("Shown in the title bar, the window title and recent projects. The folder keeps its name.".into()),
                        None,
                        &p,
                    ))
                    .child(div().w(px(240.)).child(
                        gpui_kit::Styled::h(Input::new(&self.name), row_height()),
                    ))
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Color",
                        Some("Tints the title bar, so this project's windows stand out.".into()),
                        None,
                        &p,
                    ))
                    .child(swatches)
                    .into_any_element(),
                row(&p)
                    .border_b_0()
                    .child(label(
                        "Pattern",
                        Some(if color.is_some() {
                            "Laid over the color at the left of the title bar.".into()
                        } else {
                            "Shows once the project has a color.".into()
                        }),
                        None,
                        &p,
                    ))
                    .child(tiles)
                    .into_any_element(),
            ],
        );
        let error = self
            .look_error
            .clone()
            .map(|e| crate::ui::warning_band(div().child(e), &p).into_any_element());
        error
            .into_iter()
            .chain([
                look.into_any_element(),
                self.connections(cx).into_any_element(),
                self.danger_zone(cx).into_any_element(),
            ])
            .collect()
    }

    fn shortcut_row(&self, e: &'static Entry, cx: &mut Context<Self>) -> AnyElement {
        let p = *cx.global::<Palette>();
        let keys = keymap::keys(e.id, cx);
        let capture = self.capture.as_ref().filter(|c| c.id == e.id);
        let Some(c) = capture else {
            return row(&p)
                .child(label(e.name, None, None, &p))
                .child(kb(&keys, false, &p))
                .child(
                    small_button(
                        SharedString::from(format!("change-{}", e.id)),
                        "Change".into(),
                        &p,
                    )
                    .clickable(cx.listener(move |this, _, window, cx| {
                        this.start_capture(e.id, window, cx);
                    })),
                )
                .into_any_element();
        };
        let help = match &c.conflict {
            Some((k, other)) => format!(
                "{} is used by {}. Swap them, or press another shortcut.",
                keymap::display(k),
                other.name
            ),
            None => "Press the new shortcut. Esc cancels.".into(),
        };
        row(&p)
            .bg(p.warn_bg)
            .child(label(e.name, Some(help), Some(p.warn), &p))
            .child(kb(&keys, true, &p))
            .when(c.conflict.is_some(), |d| {
                d.child(
                    small_button("shortcut-swap", "Swap".into(), &p)
                        .clickable(cx.listener(|this, _, _, cx| this.swap(cx))),
                )
            })
            .child(
                small_button("shortcut-cancel", "Cancel".into(), &p).clickable(cx.listener(
                    |this, _, _, cx| {
                        this.capture = None;
                        cx.notify();
                    },
                )),
            )
            .into_any_element()
    }

    fn general(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = *cx.global::<Palette>();
        let s = get(cx).clone();
        let select =
            |state: &Choice, w: f32| div().w(px(w)).child(Select::new(state).h(row_height()));
        let theme_seg = div().flex().gap(px(4.)).children(
            [
                (Theme::System, "System"),
                (Theme::Light, "Light"),
                (Theme::Dark, "Dark"),
            ]
            .into_iter()
            .map(|(t, l)| {
                seg_button(format!("theme-{l}").into(), l, s.theme == t, &p)
                    .clickable(move |_, _, cx| settings::update(cx, |s| s.theme = t))
            }),
        );
        let appearance = fieldset(
            "Appearance",
            vec![
                row(&p)
                    .child(label(
                        "Theme",
                        Some("System follows your desktop's light or dark mode.".into()),
                        None,
                        &p,
                    ))
                    .child(theme_seg)
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Interface text size",
                        Some("Menus, tabs, the sidebar and grids.".into()),
                        None,
                        &p,
                    ))
                    .child(select(&self.ui_size, 100.))
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Editor text size",
                        Some("SQL editor and console.".into()),
                        None,
                        &p,
                    ))
                    .child(select(&self.editor_size, 100.))
                    .into_any_element(),
            ],
        );
        let editor = fieldset(
            "Editor",
            vec![
                row(&p)
                    .child(label(
                        "Indent with",
                        Some("Used by Tab and by Format.".into()),
                        None,
                        &p,
                    ))
                    .child(select(&self.indent, 120.))
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Keywords when formatting",
                        Some("How Format writes select, from, where.".into()),
                        None,
                        &p,
                    ))
                    .child(select(&self.keywords, 150.))
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Wrap long lines",
                        Some("Off scrolls sideways instead.".into()),
                        None,
                        &p,
                    ))
                    .child(checkbox("wrap", s.wrap, |s, v| s.wrap = v))
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Autocomplete",
                        Some("Suggest tables, columns and keywords while typing.".into()),
                        None,
                        &p,
                    ))
                    .child(checkbox("autocomplete", s.autocomplete, |s, v| {
                        s.autocomplete = v
                    }))
                    .into_any_element(),
            ],
        );
        let results_seg = div().flex().gap(px(4.)).children(
            [
                (ResultsPlace::Inline, "Inline"),
                (ResultsPlace::Panel, "Panel"),
            ]
            .into_iter()
            .map(|(r, l)| {
                seg_button(format!("results-{l}").into(), l, s.results == r, &p)
                    .clickable(move |_, _, cx| settings::update(cx, |s| s.results = r))
            }),
        );
        let results = fieldset(
            "Results",
            vec![
                row(&p)
                    .child(label(
                        "Show results",
                        Some(
                            "New projects start with this. Each project remembers its own choice."
                                .into(),
                        ),
                        None,
                        &p,
                    ))
                    .child(results_seg)
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Rows per page",
                        Some("Fetched at a time as you scroll.".into()),
                        None,
                        &p,
                    ))
                    .child(select(&self.page_size, 100.))
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Show NULL as",
                        Some("In grids. Exports are not affected.".into()),
                        None,
                        &p,
                    ))
                    .child(
                        div().w(px(120.)).child(
                            gpui_kit::Styled::h(Input::new(&self.null_text), row_height())
                                .font_family(MONO_FONT)
                                .text_size(data_size()),
                        ),
                    )
                    .into_any_element(),
            ],
        );
        let tx_seg = div().flex().gap(px(4.)).children(
            [
                (TxDefault::Auto, "Auto commit"),
                (TxDefault::Manual, "Manual"),
            ]
            .into_iter()
            .map(|(t, l)| {
                seg_button(format!("tx-{l}").into(), l, s.tx_mode == t, &p)
                    .clickable(move |_, _, cx| settings::update(cx, |s| s.tx_mode = t))
            }),
        );
        let safety = fieldset(
            "Safety",
            vec![
                row(&p)
                    .child(label(
                        "New connections start in",
                        Some("Production connections always start in Manual.".into()),
                        None,
                        &p,
                    ))
                    .child(tx_seg)
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Ask before UPDATE or DELETE without WHERE",
                        Some("Also TRUNCATE and DROP. Production connections always ask.".into()),
                        None,
                        &p,
                    ))
                    .child(checkbox("ask-dangerous", s.ask_dangerous, |s, v| {
                        s.ask_dangerous = v;
                    }))
                    .into_any_element(),
                row(&p)
                    .child(label(
                        "Keep query history",
                        Some(format!(
                            "Per project, newest {} statements. Passwords are masked.",
                            crate::export::grouped(deadlock_core::history::KEEP)
                        )),
                        None,
                        &p,
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(checkbox("keep-history", s.keep_history, |s, v| {
                                s.keep_history = v;
                            }))
                            .child(
                                small_button("clear-history", "Clear history".into(), &p)
                                    .clickable(cx.listener(|this, _, window, cx| {
                                        this.clear_history(window, cx);
                                    })),
                            ),
                    )
                    .into_any_element(),
            ],
        );
        let mut shortcut_rows: Vec<AnyElement> = GENERAL_SHORTCUTS
            .iter()
            .filter_map(|id| keymap::entry(id))
            .map(|e| self.shortcut_row(e, cx))
            .collect();
        let more = ENTRIES.len() - GENERAL_SHORTCUTS.len();
        shortcut_rows.push(
            row(&p)
                .border_b_0()
                .child(
                    div().flex_grow(1.).flex_basis(px(260.)).child(
                        div()
                            .id("more-shortcuts")
                            .text_color(p.tx3)
                            .hover(|s| s.text_color(p.tx))
                            .child(format!("{more} more in the Shortcuts section"))
                            .clickable(cx.listener(|this, _, _, cx| {
                                this.section = Section::Shortcuts;
                                cx.notify();
                            })),
                    ),
                )
                .child(
                    small_button("reset-shortcuts", "Reset all to defaults".into(), &p).clickable(
                        cx.listener(|this, _, window, cx| this.reset_shortcuts(window, cx)),
                    ),
                )
                .into_any_element(),
        );
        vec![
            appearance.into_any_element(),
            editor.into_any_element(),
            results.into_any_element(),
            safety.into_any_element(),
            fieldset("Shortcuts", shortcut_rows).into_any_element(),
        ]
    }

    fn shortcuts(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = *cx.global::<Palette>();
        let mut groups: Vec<&'static str> = Vec::new();
        for e in ENTRIES {
            if !groups.contains(&e.group) {
                groups.push(e.group);
            }
        }
        let mut out: Vec<AnyElement> = groups
            .into_iter()
            .map(|g| {
                let rows = ENTRIES
                    .iter()
                    .filter(|e| e.group == g)
                    .map(|e| self.shortcut_row(e, cx))
                    .collect();
                fieldset(g, rows).into_any_element()
            })
            .collect();
        out.push(
            row(&p)
                .border_b_0()
                .child(div().flex_grow(1.))
                .child(
                    small_button("reset-shortcuts", "Reset all to defaults".into(), &p).clickable(
                        cx.listener(|this, _, window, cx| this.reset_shortcuts(window, cx)),
                    ),
                )
                .into_any_element(),
        );
        out
    }
}

impl Render for SettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(window, cx);
        let p = *cx.global::<Palette>();
        let project_name = SharedString::from(
            self.shell
                .upgrade()
                .map_or_else(String::new, |s| s.read(cx).project().name()),
        );
        let nav_item =
            |id: &'static str, label: SharedString, s: Section, cx: &mut Context<Self>| {
                let on = self.section == s;
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .w_full()
                    .h(row_height() + px(4.))
                    .px(px(10.))
                    .rounded(RADIUS)
                    .whitespace_nowrap()
                    .when(on, |d| d.bg(p.sel).font_weight(FontWeight::MEDIUM))
                    .when(!on, |d| d.hover(|st| st.bg(p.hover)))
                    .child(label)
                    .clickable(cx.listener(move |this, _, _, cx| {
                        this.section = s;
                        cx.notify();
                    }))
            };
        // Fixed beside the body: the design's wrap would put the list above a body
        // that already scrolls.
        let nav = div()
            .flex_none()
            .w(px(200.))
            .py(px(20.))
            .px(px(12.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .border_r_1()
            .border_color(p.line)
            .child(nav_item(
                "settings-project",
                project_name.clone(),
                Section::Project,
                cx,
            ))
            .child(nav_item(
                "settings-general",
                "General".into(),
                Section::General,
                cx,
            ))
            .child(nav_item(
                "settings-shortcuts",
                "Shortcuts".into(),
                Section::Shortcuts,
                cx,
            ))
            .child(
                div()
                    .mt(px(16.))
                    .mx(px(10.))
                    .text_size(small_size())
                    .text_color(p.tx3)
                    .line_height(relative(1.5))
                    .child(if self.section == Section::Project {
                        "Project settings are saved in the folder's .deadlock/project.toml, \
                         for everyone who opens it."
                    } else {
                        "Settings apply to every project on this computer and take effect at once."
                    }),
            );
        let body = match self.section {
            Section::Project => self.project(cx),
            Section::General => self.general(cx),
            Section::Shortcuts => self.shortcuts(cx),
        };
        let error = cx
            .global::<settings::SettingsState>()
            .error
            .clone()
            .map(|e| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .py(px(12.))
                    .px(px(16.))
                    .border_1()
                    .border_color(p.warn_line)
                    .bg(p.warn_bg)
                    .rounded(RADIUS)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("The settings file cannot be read: defaults are in use"),
                    )
                    .child(div().text_color(p.tx2).child(format!(
                        "{e}. It is kept as it is until you change a setting; it is then saved \
                         next to the new file as settings.toml.unreadable."
                    )))
            });
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .bg(p.bg)
            .text_color(p.tx)
            .child(nav)
            .child(
                div()
                    .id("settings-body")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        div().pt(px(24.)).px(px(32.)).pb(px(32.)).child(
                            div()
                                .max_w(px(720.))
                                .flex()
                                .flex_col()
                                .gap(px(28.))
                                .line_height(relative(1.4))
                                .child(
                                    div()
                                        .text_size(px(20.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .line_height(relative(1.2))
                                        .child(match self.section {
                                            Section::Project => project_name,
                                            Section::General => "Settings".into(),
                                            Section::Shortcuts => "Shortcuts".into(),
                                        }),
                                )
                                .children(error)
                                .children(body),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_leave_out_a_redis_or_blank_user() {
        let mysql = ConnectionConfig {
            user: "app".into(),
            host: "db.local".into(),
            ..ConnectionConfig::new(Engine::Mysql)
        };
        assert_eq!(short_address(&mysql), "app@db.local:3306");
        let anonymous = ConnectionConfig {
            user: String::new(),
            ..mysql
        };
        assert_eq!(short_address(&anonymous), "db.local:3306");
        let redis = ConnectionConfig {
            user: "default".into(),
            ..ConnectionConfig::new(Engine::Redis)
        };
        assert_eq!(short_address(&redis), "localhost:6379");
    }
}
