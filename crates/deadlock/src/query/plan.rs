//! The result panel's Plan tab: summary tiles, the step tree with its share-of-time
//! bars, the step detail pane, and the server's raw plan as text.

use std::collections::HashSet;
use std::ops::Range;

use deadlock_core::edit::TableRef;
use deadlock_core::plan::{Plan, PlanNode, Reference, millis, rows, table_reference};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClipboardItem, Context, Div, FontWeight, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px, relative,
};

use super::{QueryTab, TabEvent};
use crate::assets::{Icon, icon};
use crate::theme::{Palette, data_size, mono_font, small_size, tab_height};
use crate::ui::Clickable as _;
use crate::ui::{Kind as ButtonKind, button};

/// A plan in the Plan tab, with the statement it explains.
pub(super) struct PlanView {
    pub plan: Plan,
    /// The statement's bytes in the editor when it was explained, and its text.
    pub range: Range<usize>,
    pub text: String,
    /// Tree (`false`) or the raw text.
    pub text_view: bool,
    /// The selected step, by its position in [`Plan::walk`].
    pub selected: Option<usize>,
    pub collapsed: HashSet<usize>,
    /// Per step: where the statement names its table.
    refs: Vec<Option<Reference>>,
}

impl PlanView {
    pub fn new(plan: Plan, range: Range<usize>, text: String) -> Self {
        let refs = plan
            .walk()
            .iter()
            .map(|(_, n)| n.table.as_deref().and_then(|t| table_reference(&text, t)))
            .collect();
        let selected = plan.slowest().or((!plan.steps.is_empty()).then_some(0));
        Self {
            plan,
            range,
            text,
            text_view: false,
            selected,
            collapsed: HashSet::new(),
            refs,
        }
    }

    /// The table a step reads, when the statement names it.
    fn table_of(&self, step: usize) -> Option<TableRef> {
        let r = self.refs.get(step)?.as_ref()?;
        Some(TableRef {
            database: r.database.clone().or_else(|| self.plan.database.clone())?,
            table: r.table.clone(),
        })
    }
}

/// The Tree / Text switch and Copy plan, on the right of the panel's tab row.
pub(super) fn header_actions(view: &PlanView, cx: &mut Context<QueryTab>) -> Div {
    let p = *cx.global::<Palette>();
    let segment = |id: &'static str, label: &'static str, on: bool, text: bool, tip| {
        segment_button(id, on, tip, &p)
            .child(label)
            .clickable(cx.listener(move |this, _, _, cx| {
                if let Some(v) = &mut this.explained {
                    v.text_view = text;
                }
                cx.notify();
            }))
    };
    let measured = view.plan.measured;
    let (estimated_keys, measured_keys) = (
        crate::keymap::label("explain", cx),
        crate::keymap::label("explain-measured", cx),
    );
    let mode = |id: &'static str, label: &'static str, analyze: bool, tip: String| {
        segment_button(id, measured == analyze, tip, &p)
            .child(label)
            .clickable(cx.listener(move |this, _, _, cx| this.reexplain(analyze, cx)))
    };
    let raw = view.plan.raw.clone();
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .flex()
                .gap(px(4.))
                .child(mode(
                    "plan-estimated",
                    "Estimated",
                    false,
                    format!("Plan without running the statement ({estimated_keys})"),
                ))
                .child(mode(
                    "plan-measured",
                    "Measured",
                    true,
                    format!("Run the statement and measure each step ({measured_keys})"),
                )),
        )
        .child(
            div()
                .flex()
                .gap(px(4.))
                .child(segment(
                    "plan-tree",
                    "Tree",
                    !view.text_view,
                    false,
                    "Steps as a tree",
                ))
                .child(segment(
                    "plan-text",
                    "Text",
                    view.text_view,
                    true,
                    "The server's plan",
                )),
        )
        .child(
            button(
                "copy-plan",
                ButtonKind::Outline,
                true,
                "Copy the server's plan",
                cx,
            )
            .child("Copy plan")
            .clickable(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(raw.clone()));
            }),
        )
}

/// `.b.bo`, and `.b.on` when `on`: filled with `--tx`, with no hover change.
fn segment_button(
    id: &'static str,
    on: bool,
    tip: impl Into<SharedString>,
    p: &Palette,
) -> gpui_kit::Stateful<Div> {
    let tip: SharedString = tip.into();
    let hover = p.hover;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(crate::theme::row_height())
        .px(px(8.))
        .border_1()
        .rounded(crate::theme::RADIUS)
        .cursor_pointer()
        .when(on, |d| d.bg(p.tx).border_color(p.tx).text_color(p.bg))
        .when(!on, |d| {
            d.border_color(p.line2)
                .text_color(p.tx)
                .hover(move |s| s.bg(hover))
        })
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
}

/// The Plan tab's body.
pub(super) fn render(this: &QueryTab, cx: &mut Context<QueryTab>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let Some(view) = &this.explained else {
        return div().into_any_element();
    };
    let body = if view.text_view {
        raw_text(&view.plan.raw, &p).into_any_element()
    } else {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(tree(view, &p, cx))
            .child(detail(view, &p, cx))
            .into_any_element()
    };
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(tiles(&view.plan, &p))
        .child(body)
        .into_any_element()
}

/// `.sum`: a label over a value.
fn tile(label: &'static str, value: String, warn: bool, p: &Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(div().text_size(small_size()).text_color(p.tx3).child(label))
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .when(warn, |d| d.text_color(p.warn))
                .child(value),
        )
}

/// Execution, Planning, Rows returned and Slowest step, each only when reported. An
/// estimated plan shows its cost instead.
fn tiles(plan: &Plan, p: &Palette) -> Div {
    let mut out = Vec::new();
    if plan.measured {
        if let Some(t) = plan.execution_ms {
            out.push(tile("Execution", millis(t), false, p));
        }
        if let Some(t) = plan.planning_ms {
            out.push(tile("Planning", millis(t), false, p));
        }
        if let Some(n) = plan.rows_returned {
            out.push(tile("Rows returned", rows(n), false, p));
        }
        let walk = plan.walk();
        let total = plan.own_total();
        if let Some((_, n)) = plan.slowest().and_then(|i| walk.get(i))
            && total > 0.
        {
            let share = n.own_ms.unwrap_or(0.) / total * 100.;
            let text = format!("{}, {share:.0}% of the time", n.label());
            out.push(tile("Slowest step", text, true, p));
        }
    } else {
        out.push(tile(
            "Plan",
            "Estimated: the statement did not run".into(),
            false,
            p,
        ));
        if let Some(c) = plan.cost {
            out.push(tile("Estimated cost", rows(c), false, p));
        }
    }
    div()
        .flex()
        .flex_none()
        .flex_wrap()
        .gap_x(px(40.))
        .gap_y(px(12.))
        .px(px(16.))
        .py(px(14.))
        .border_b_1()
        .border_color(p.line)
        .children(out)
}

/// `.pl`'s columns: Step, Estimated rows, Actual rows, Own time, Share of time.
const NUM_W: f32 = 96.;
const TIME_W: f32 = 88.;

fn step_cell() -> Div {
    div()
        .flex_grow(1.8)
        .flex_basis(px(0.))
        .min_w(px(320.))
        .px(px(12.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
}

fn num_cell(w: f32) -> Div {
    div()
        .flex_none()
        .w(px(w))
        .px(px(12.))
        .flex()
        .justify_end()
        .whitespace_nowrap()
}

fn share_cell() -> Div {
    div()
        .flex_grow(1.)
        .flex_basis(px(0.))
        .min_w(px(170.))
        .px(px(12.))
        .flex()
        .items_center()
        .gap(px(8.))
}

fn tree(view: &PlanView, p: &Palette, cx: &mut Context<QueryTab>) -> impl IntoElement {
    let measured = view.plan.measured;
    let walk = view.plan.walk();
    let slowest = view.plan.slowest();
    let total = view.plan.own_total();
    let row = || {
        div()
            .flex()
            .items_center()
            .w_full()
            .min_h(tab_height())
            .border_b_1()
            .border_color(p.line)
    };
    let header = row()
        .bg(p.panel)
        .text_color(p.tx2)
        .text_size(small_size())
        .font_weight(FontWeight::MEDIUM)
        .child(step_cell().child("Step"))
        .child(num_cell(NUM_W).child("Estimated rows"))
        .when(measured, |d| {
            d.child(num_cell(NUM_W).child("Actual rows"))
                .child(num_cell(TIME_W).child("Own time"))
                .child(share_cell().child("Share of time"))
        })
        .when(!measured, |d| d.child(num_cell(NUM_W).child("Cost")));

    let mut rows_out: Vec<AnyElement> = Vec::with_capacity(walk.len());
    let mut hidden_below: Option<usize> = None;
    for (i, (depth, n)) in walk.iter().enumerate() {
        if hidden_below.is_some_and(|d| *depth > d) {
            continue;
        }
        hidden_below = None;
        let collapsed = view.collapsed.contains(&i);
        if collapsed {
            hidden_below = Some(*depth);
        }
        let slow = slowest == Some(i);
        let selected = view.selected == Some(i);
        let share = n.own_ms.filter(|_| total > 0.).map(|t| t / total);
        let chevron = if n.children.is_empty() {
            div().size(px(16.)).flex_none().into_any_element()
        } else {
            div()
                .id(("plan-toggle", i))
                .flex_none()
                .child(icon(
                    if collapsed {
                        Icon::ChevronRight
                    } else {
                        Icon::ChevronDown
                    },
                    p.tx3,
                ))
                .clickable(cx.listener(move |this, _, _, cx| {
                    if let Some(v) = &mut this.explained
                        && !v.collapsed.remove(&i)
                    {
                        v.collapsed.insert(i);
                    }
                    cx.stop_propagation();
                    cx.notify();
                }))
                .into_any_element()
        };
        #[allow(clippy::cast_precision_loss)] // a plan's depth is small
        let indent = 12. + 20. * *depth as f32;
        // The indent is content, not padding, so every row's cell keeps one width.
        let step = step_cell()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(div().flex_none().w(px(indent - 12.)))
            .child(chevron)
            .child(
                div().min_w_0().overflow_hidden().text_ellipsis().child(
                    gpui_kit::StyledText::new(SharedString::from(n.label()))
                        .with_highlights(title_style(n, p)),
                ),
            );
        let mono = |d: Div| d.font_family(mono_font()).text_size(data_size());
        let estimated = mono(num_cell(NUM_W)).children(n.estimated_rows.map(rows));
        let r = row()
            .id(("plan-step", i))
            .when(selected, |d| d.bg(p.sel))
            .when(!selected, |d| {
                let bg = p.panel;
                d.hover(move |s| s.bg(bg))
            })
            .clickable(cx.listener(move |this, _, _, cx| {
                if let Some(v) = &mut this.explained {
                    v.selected = Some(i);
                }
                cx.notify();
            }))
            .child(step)
            .child(estimated);
        let r = if measured {
            let off = n.misestimated();
            let actual = mono(num_cell(NUM_W))
                .id(("plan-actual", i))
                .when(off, |d| {
                    let higher = n.actual_rows > n.estimated_rows;
                    let tip: SharedString = format!(
                        "{} than the estimate by more than 10×",
                        if higher { "More rows" } else { "Fewer rows" }
                    )
                    .into();
                    d.text_color(p.warn)
                        .gap(px(4.))
                        .child(if higher { "↑" } else { "↓" })
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                })
                .children(n.actual_rows.map(rows));
            let own = mono(num_cell(TIME_W))
                .when(slow, |d| {
                    d.text_color(p.warn).font_weight(FontWeight::MEDIUM)
                })
                .children(n.own_ms.map(millis));
            let bar = share_cell().children(
                share
                    .map(|s| {
                        let fill = if slow { p.warn } else { p.tx3 };
                        [
                            div()
                                .flex_1()
                                .h(px(6.))
                                .rounded(px(3.))
                                .bg(p.line)
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(s.clamp(0., 1.) as f32))
                                        .min_w(px(1.))
                                        .rounded(px(3.))
                                        .bg(fill),
                                )
                                .into_any_element(),
                            mono(div())
                                .flex_none()
                                .w(px(36.))
                                .flex()
                                .justify_end()
                                .text_color(if slow { p.warn } else { p.tx3 })
                                .when(slow, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(format!("{:.0}%", s * 100.))
                                .into_any_element(),
                        ]
                    })
                    .into_iter()
                    .flatten(),
            );
            r.child(actual).child(own).child(bar)
        } else {
            r.child(mono(num_cell(NUM_W)).children(n.cost.map(rows)))
        };
        rows_out.push(r.into_any_element());
    }
    div()
        .id("plan-tree")
        .flex_grow(1.)
        .flex_basis(px(480.))
        .min_w_0()
        .h_full()
        .overflow_scroll()
        .child(
            div()
                .min_w(px(if measured { 780. } else { 512. }))
                .child(header)
                .children(rows_out),
        )
}

/// The title in medium weight, the detail dimmed.
fn title_style(n: &PlanNode, p: &Palette) -> Vec<(Range<usize>, gpui_kit::HighlightStyle)> {
    let title = n.title.len();
    let mut out = vec![(
        0..title,
        gpui_kit::HighlightStyle {
            font_weight: Some(FontWeight::MEDIUM),
            ..Default::default()
        },
    )];
    if !n.detail.is_empty() {
        out.push((
            title..title + 1 + n.detail.len(),
            gpui_kit::HighlightStyle {
                color: Some(p.tx3),
                ..Default::default()
            },
        ));
    }
    out
}

/// The facts of a step, in the order the detail pane lists them.
fn facts(view: &PlanView, n: &PlanNode) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |label: &str, value: Option<String>| {
        if let Some(v) = value {
            out.push((label.to_owned(), v));
        }
    };
    push("Estimated rows", n.estimated_rows.map(rows));
    push("Actual rows", n.actual_rows.map(rows));
    push("Runs", n.loops.map(rows));
    push("Own time", n.own_ms.map(millis));
    if view.plan.measured && !n.children.is_empty() {
        push("Total time", n.inclusive_ms().map(millis));
    }
    push("Cost", n.cost.map(rows));
    out.extend(n.facts.iter().cloned());
    out
}

fn detail(view: &PlanView, p: &Palette, cx: &mut Context<QueryTab>) -> impl IntoElement {
    let walk = view.plan.walk();
    let aside = div()
        .id("plan-detail")
        .flex_none()
        .w(px(304.))
        .h_full()
        .overflow_y_scroll()
        .p(px(20.))
        .flex()
        .flex_col()
        .gap(px(20.))
        .border_l_1()
        .border_color(p.line);
    let Some((i, (_, n))) = view.selected.and_then(|i| Some((i, *walk.get(i)?))) else {
        return aside.child(
            div()
                .text_color(p.tx3)
                .child("Select a step to see its details."),
        );
    };
    let table = view.table_of(i);
    let mut sentence = n.describe(table.as_ref().map(|t| t.table.as_str()));
    if view.plan.slowest() == Some(i) {
        sentence.push_str(" It is the slowest step of this query.");
    }
    let grid = facts(view, n).into_iter().map(|(label, value)| {
        div()
            .flex()
            .gap(px(12.))
            .child(div().flex_none().w(px(104.)).text_color(p.tx3).child(label))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(mono_font())
                    .text_size(data_size())
                    .child(value),
            )
    });
    let show = button(
        "plan-show",
        ButtonKind::Outline,
        true,
        "Select the step's table in the editor, or the whole statement",
        cx,
    )
    .child("Show in query")
    .clickable(cx.listener(move |this, _, window, cx| this.show_step(i, window, cx)));
    let open = table.map(|t| {
        button(
            "plan-open",
            ButtonKind::Outline,
            true,
            "Open the table's structure",
            cx,
        )
        .child(format!("Open {}", t.table))
        .clickable(cx.listener(move |_, _, _, cx| cx.emit(TabEvent::OpenTable(t.clone()))))
    });
    aside
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(n.label()),
                )
                .child(
                    div()
                        .text_color(p.tx2)
                        .line_height(relative(1.5))
                        .child(sentence),
                ),
        )
        .child(div().flex().flex_col().gap(px(8.)).children(grid))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .child(show)
                .children(open),
        )
}

fn raw_text(raw: &str, p: &Palette) -> impl IntoElement {
    div()
        .id("plan-text")
        .flex_1()
        .min_h_0()
        .overflow_scroll()
        .py(px(10.))
        .px(px(16.))
        .font_family(mono_font())
        .text_size(data_size())
        .text_color(p.tx)
        .children(
            raw.lines()
                .map(|l| div().h(px(22.)).whitespace_nowrap().child(l.to_owned())),
        )
}

impl QueryTab {
    /// Selects the step's table reference in the editor, or the whole statement when
    /// the step names no table the statement shows.
    fn show_step(&mut self, step: usize, window: &mut gpui_kit::Window, cx: &mut Context<Self>) {
        let Some(view) = &self.explained else { return };
        let buffer = self.editor.read(cx).value().to_string();
        // The statement may have moved since it was explained.
        let start = if buffer.get(view.range.clone()) == Some(view.text.as_str()) {
            Some(view.range.start)
        } else {
            buffer.find(&view.text)
        };
        let Some(start) = start else {
            self.note(
                "Show in query",
                "The statement has changed since it was explained.".into(),
                true,
            );
            cx.notify();
            return;
        };
        let range = match view.refs.get(step).and_then(Option::as_ref) {
            Some(r) => start + r.range.start..start + r.range.end,
            None => start..start + view.text.len(),
        };
        self.editor.update(cx, |e, cx| {
            e.set_selected_range(range, cx);
            e.focus(window, cx);
        });
    }
}
