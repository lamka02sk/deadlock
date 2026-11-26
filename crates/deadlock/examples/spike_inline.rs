//! A result table between two lines of the SQL editor, scrolling with the text.
//!
//! Uses the editor's block widgets (Deadlock's patch to gpui-component's editor,
//! `vendor/gpui-base/PATCHES.md`): one buffer, one gutter, blocks under the last line
//! of each statement that push the later rows down. Type above a block to see it
//! follow its line; delete the line to remove it.

use deadlock::theme::{self, Palette};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::*;

const TEXT: &str = "-- 1\nSELECT id, email, created_at\nFROM customers\nWHERE created_at >= '2026-01-01'\nORDER BY created_at DESC\nLIMIT 5;\n\n-- 2\nUPDATE orders\nSET status = 'shipped'\nWHERE id = 42;\n";

/// Rows of the demo table.
const ROWS: usize = 5;
const ROW_HEIGHT: f32 = 32.;

struct Demo;

impl TableDelegate for Demo {
    fn columns_count(&self, _: &App) -> usize {
        3
    }

    fn rows_count(&self, _: &App) -> usize {
        ROWS
    }

    fn column(&self, col: usize, _: &App) -> Column {
        let name = ["id", "email", "created_at"][col];
        Column::new(name, name).width(px(180.))
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match col {
            0 => (1000 - row).to_string(),
            1 => format!("user{row}@example.com"),
            _ => format!("2026-11-{:02} 09:30", 22 - row),
        }
    }
}

struct Spike {
    editor: Entity<EditorState>,
}

impl Render for Spike {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.global::<Palette>();
        div().size_full().bg(p.bg).text_color(p.tx).child(
            Editor::new(&self.editor)
                .bordered(false)
                .h_full()
                .font_family(theme::mono_font())
                .text_size(theme::editor_size()),
        )
    }
}

/// A card with `child`, as an inline result is drawn.
fn card(child: AnyElement, cx: &App) -> AnyElement {
    let p = cx.global::<Palette>();
    div()
        .size_full()
        .pt(px(4.))
        .pb(px(8.))
        .px(px(16.))
        .child(
            div()
                .size_full()
                .border_1()
                .border_color(p.line)
                .rounded(theme::RADIUS)
                .overflow_hidden()
                .font_family(theme::mono_font())
                .text_size(theme::data_size())
                .child(child),
        )
        .into_any_element()
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            deadlock::init(cx).expect("init");
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                theme::follow_system(window, cx);
                window.set_window_title("spike_inline: editor blocks");
                let table = cx.new(|cx| TableState::new(Demo, window, cx));
                let editor = cx.new(|cx| {
                    let mut e = EditorState::new(window, cx)
                        .language("sql")
                        .line_number(true)
                        .default_value(TEXT);
                    #[allow(clippy::cast_precision_loss)] // ROWS is small
                    let grid = px(ROW_HEIGHT * (ROWS + 1) as f32 + 2. + 12.);
                    // Under `LIMIT 5;` (row 5) and `WHERE id = 42;` (row 10).
                    e.insert_block(
                        5,
                        grid,
                        move |_, cx| {
                            card(
                                DataTable::new(&table).bordered(false).into_any_element(),
                                cx,
                            )
                        },
                        cx,
                    );
                    e.insert_block(
                        10,
                        px(46.),
                        |_, cx| {
                            let tx2 = cx.global::<Palette>().tx2;
                            card(
                                div()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .px(px(12.))
                                    .text_color(tx2)
                                    .child("1 row affected in 8 ms")
                                    .into_any_element(),
                                cx,
                            )
                        },
                        cx,
                    );
                    e
                });
                cx.new(|_| Spike { editor })
            })
            .expect("open window");
        });
}
