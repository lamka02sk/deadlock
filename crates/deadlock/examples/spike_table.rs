//! 1,000,000 rows x 8 columns. Cells are formatted from (row, col) when drawn; no row is stored.
//! Scrolls itself for 5 s and prints frame intervals, then hands control to the mouse.

#[path = "support/frames.rs"]
mod frames;

use std::time::Duration;

use deadlock::theme::{self, Palette};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::*;

const ROWS: usize = 1_000_000;
const COLS: [&str; 8] = [
    "id",
    "customer_id",
    "status",
    "total",
    "currency",
    "created_at",
    "shipped_at",
    "note",
];

struct Rows;

impl TableDelegate for Rows {
    fn columns_count(&self, _: &App) -> usize {
        COLS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        ROWS
    }

    fn column(&self, col: usize, _: &App) -> Column {
        Column::new(COLS[col], COLS[col]).width(px(140.))
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = match col {
            0 => row.to_string(),
            1 => (row * 7919 % 50_000).to_string(),
            2 => ["paid", "shipped", "pending", "refunded"][row % 4].to_string(),
            3 => format!("{}.{:02}", row % 997, row % 100),
            4 => "EUR".to_string(),
            5 => format!(
                "2026-{:02}-{:02} 10:{:02}",
                row % 12 + 1,
                row % 28 + 1,
                row % 60
            ),
            6 => return div().text_color(cx.global::<Palette>().tx3).child("null"),
            _ => format!("row {row}"),
        };
        div().child(text)
    }
}

struct Spike {
    table: Entity<TableState<Rows>>,
    row: usize,
    frames: frames::FrameStats,
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.frames.tick() {
            self.row += 3;
            let row = self.row;
            self.table.update(cx, |t, cx| t.scroll_to_row(row, cx));
            window.request_animation_frame();
        }
        let p = cx.global::<Palette>();
        div()
            .relative()
            .size_full()
            .bg(p.bg)
            .text_color(p.tx)
            .font_family(theme::MONO_FONT)
            .text_size(theme::data_size())
            .child(DataTable::new(&self.table).bordered(false))
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
                window.set_window_title("spike_table: 1M rows");
                let table = cx.new(|cx| TableState::new(Rows, window, cx));
                cx.new(|_| Spike {
                    table,
                    row: 0,
                    frames: frames::FrameStats::new("spike_table", Duration::from_secs(5)),
                })
            })
            .expect("open window");
        });
}
