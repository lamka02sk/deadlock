//! Editable 50,000-line SQL buffer with tree-sitter highlighting.
//! Scrolls itself for 5 s and prints frame intervals, then hands control to the keyboard.
//! `-- <n>` adds `n` block widgets (the editor patch), 120 px each, one every
//! 10 lines from the top, so the scripted scroll passes through them.

#[path = "support/frames.rs"]
mod frames;

use std::time::Duration;

use deadlock::theme::{self, Palette};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::*;

const LINES: usize = 50_000;

fn sql() -> String {
    let block = [
        "-- orders for customer {i}",
        "SELECT o.id, o.total, c.email, 'pending' AS state",
        "FROM orders o JOIN customers c ON c.id = o.customer_id",
        "WHERE o.created_at >= '2026-01-01' AND o.total > {i}.50",
        "ORDER BY o.created_at DESC LIMIT 100;",
    ];
    (0..LINES / block.len())
        .flat_map(|i| block.iter().map(move |l| l.replace("{i}", &i.to_string())))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Spike {
    editor: Entity<EditorState>,
    frames: frames::FrameStats,
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.frames.tick() {
            self.editor.update(cx, |e, cx| e.scroll_by_y_f64(-90., cx));
            window.request_animation_frame();
        }
        let p = cx.global::<Palette>();
        div()
            .relative()
            .size_full()
            .bg(p.bg)
            .child(
                Editor::new(&self.editor)
                    .bordered(false)
                    .h(relative(1.))
                    .font_family(theme::mono_font())
                    .text_size(theme::editor_size()),
            )
            .child(gpui_fps::fps_monitor(window, cx))
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            deadlock::init(cx).expect("init");
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                theme::follow_system(window, cx);
                window.set_window_title("spike_editor: 50k lines");
                let blocks: usize = std::env::args()
                    .nth(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let editor = cx.new(|cx| {
                    let mut editor = EditorState::new(window, cx)
                        .language("sql")
                        .line_number(true)
                        .soft_wrap(false)
                        .default_value(sql());
                    for i in 0..blocks {
                        editor.insert_block(
                            4 + i * 10,
                            px(120.),
                            move |_, cx| {
                                let p = cx.global::<Palette>();
                                div()
                                    .size_full()
                                    .p_2()
                                    .border_1()
                                    .border_color(p.line)
                                    .rounded(theme::RADIUS)
                                    .bg(p.panel)
                                    .child(format!("block {i}"))
                                    .into_any_element()
                            },
                            cx,
                        );
                    }
                    editor
                });
                let focus = editor.focus_handle(cx);
                window.defer(cx, move |window, cx| focus.focus(window, cx));
                cx.new(|_| Spike {
                    editor,
                    frames: frames::FrameStats::new("spike_editor", Duration::from_secs(5)),
                })
            })
            .expect("open window");
        });
}
