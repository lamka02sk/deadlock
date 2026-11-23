//! 300 tables and 400 foreign keys on the schema diagram's canvas. Pans, zooms and
//! drags a card by itself, phase by phase, and prints the paint time per frame of each.

#[path = "support/frames.rs"]
mod frames;

use std::time::{Duration, Instant};

use deadlock::diagram::{Board, paint};
use deadlock::theme::{self, Palette};
use deadlock_core::diagram::{Column, Diagram, Link, Table};
use gpui_kit::*;

const TABLES: usize = 300;
const LINKS: usize = 400;
const PHASE: Duration = Duration::from_secs(3);

fn diagram() -> Diagram {
    let tables = (0..TABLES)
        .map(|i| Table {
            name: format!("table_{i:03}"),
            columns: (0..6 + i % 7)
                .map(|c| Column {
                    name: if c == 0 {
                        "id".into()
                    } else {
                        format!("column_{c}_id")
                    },
                    data_type: ["bigint", "varchar(255)", "decimal(12,2)", "datetime"][c % 4]
                        .into(),
                    primary_key: c == 0,
                    foreign_key: c > 0 && c % 2 == 1,
                })
                .collect(),
        })
        .collect();
    let links = (0..LINKS)
        .map(|i| {
            let from = (i * 7 + 1) % TABLES;
            let to = if i % 50 == 0 { from } else { (i * 13) % TABLES };
            Link {
                name: format!("fk_{i}"),
                from: format!("table_{from:03}"),
                from_columns: vec!["column_1_id".into()],
                to: format!("table_{to:03}"),
                to_columns: vec!["id".into()],
            }
        })
        .collect();
    Diagram { tables, links }
}

/// Phases: label, zoom at the start, and what each frame does.
const PHASES: [(&str, f32); 5] = [
    ("pan at 100%", 1.),
    ("pan at 50%", 0.5),
    ("pan at 25%, everything visible", 0.25),
    ("zoom 25% ↔ 200% around the pointer", 0.25),
    ("drag a card at 50%", 0.5),
];

struct Perf {
    board: Board,
    phase: usize,
    started: Option<Instant>,
    frame: u32,
    paints: std::rc::Rc<std::cell::RefCell<Vec<Duration>>>,
    frames: frames::FrameStats,
}

impl Perf {
    fn step(&mut self) -> bool {
        if self.phase == PHASES.len() {
            return false;
        }
        let now = Instant::now();
        let start = *self.started.get_or_insert(now);
        if now - start >= PHASE {
            let mut p = self.paints.borrow_mut();
            p.sort();
            let ms = |d: Duration| d.as_secs_f64() * 1000.;
            if !p.is_empty() {
                println!(
                    "[{}] paint: p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms over {} frames",
                    PHASES[self.phase].0,
                    ms(p[p.len() / 2]),
                    ms(p[p.len() * 95 / 100]),
                    ms(p[p.len() - 1]),
                    p.len()
                );
            }
            p.clear();
            self.phase += 1;
            self.started = None;
            self.frame = 0;
            if self.phase == PHASES.len() {
                return false;
            }
        }
        if self.frame == 0 {
            self.board.zoom = PHASES[self.phase].1;
            self.board.pan = (0., 0.);
        }
        let f = self.frame as f32;
        match self.phase {
            0..=2 => self.board.pan = (-f * 7., -f * 3.),
            3 => {
                let z = 0.25 * 8f32.powf(((f / 60.).sin() + 1.) / 2.);
                self.board.zoom_at(960., 540., z);
            }
            _ => {
                let i = 0;
                if let Some(r) = self.board.card(i) {
                    self.board.pan = (-r.x * 0.5 + 200., -r.y * 0.5 + 200.);
                    self.board.show(i, (r.x + 4., r.y + 2.));
                }
            }
        }
        self.frame += 1;
        true
    }
}

impl Render for Perf {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames.tick();
        if self.step() {
            window.request_animation_frame();
        } else {
            cx.quit();
        }
        let p = *cx.global::<Palette>();
        let entity = cx.entity();
        let paints = self.paints.clone();
        div().size_full().bg(p.panel).child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    entity.update(cx, |t, cx| {
                        let started = Instant::now();
                        paint(&t.board, bounds, &p, window, cx);
                        paints.borrow_mut().push(started.elapsed());
                    });
                },
            )
            .size_full(),
        )
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            deadlock::init(cx).expect("init");
            let bounds = Bounds::centered(None, size(px(1920.), px(1080.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..WindowOptions::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                theme::follow_system(window, cx);
                window.set_window_title("diagram_perf: 300 tables, 400 links");
                let mut board = Board::new(diagram());
                board.show_all();
                let t = Instant::now();
                board.arrange();
                println!("arrange: {:.1} ms", t.elapsed().as_secs_f64() * 1000.);
                cx.new(|_| Perf {
                    board,
                    phase: 0,
                    started: None,
                    frame: 0,
                    paints: Default::default(),
                    frames: frames::FrameStats::new("diagram_perf", PHASE * 5),
                })
            })
            .expect("open window");
        });
}
