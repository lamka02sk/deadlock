//! Two windows from one process, keyed like projects. Opening a key that already has a
//! window focuses it instead of creating another. Scripted: open alpha, open beta, then
//! open alpha again after 2 s; prints the window count after each step.

use std::collections::HashMap;
use std::time::Duration;

use deadlock::theme::{self, Palette};
use gpui_kit::component::button::Button;
use gpui_kit::*;

#[derive(Default)]
struct Registry(HashMap<SharedString, AnyWindowHandle>);

impl Global for Registry {}

fn open(key: SharedString, cx: &mut App) -> anyhow::Result<()> {
    if let Some(handle) = cx.default_global::<Registry>().0.get(&key).copied() {
        // A window closed by the user fails to update; fall through and reopen it.
        if handle.update(cx, |_, w, _| w.activate_window()).is_ok() {
            println!(
                "open {key}: focused existing window, {} windows",
                cx.windows().len()
            );
            return Ok(());
        }
    }
    let title = key.clone();
    // Wayland only sends an activation request for windows with an app id.
    let options = WindowOptions {
        app_id: Some("deadlock".into()),
        ..Default::default()
    };
    let (handle, _) = gpui_kit::open_window(options, cx, |window, cx| {
        theme::follow_system(window, cx);
        window.set_window_title(&format!("spike_windows: {title}"));
        cx.new(|_| Project { key: title })
    })?;
    cx.default_global::<Registry>()
        .0
        .insert(key.clone(), handle);
    println!("open {key}: new window, {} windows", cx.windows().len());
    Ok(())
}

struct Project {
    key: SharedString,
}

impl Render for Project {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.global::<Palette>();
        let button = |key: &'static str| {
            Button::new(key)
                .label(format!("Open {key}"))
                .on_click(move |_, _, cx| {
                    if let Err(e) = open(key.into(), cx) {
                        eprintln!("{e:#}");
                    }
                })
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .bg(p.bg)
            .text_color(p.tx)
            .child(format!("Project {}", self.key))
            .child(button("alpha"))
            .child(button("beta"))
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            deadlock::init(cx).expect("init");
            open("alpha".into(), cx).expect("open alpha");
            open("beta".into(), cx).expect("open beta");
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                cx.update(|cx| {
                    open("alpha".into(), cx).expect("reopen alpha");
                    let n = cx.windows().len();
                    println!(
                        "{} after reopening alpha: {n} windows",
                        if n == 2 { "PASS" } else { "FAIL" }
                    );
                });
                // Whether the compositor honored the activation request (Wayland may refuse).
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                cx.update(|cx| {
                    let alpha = cx.default_global::<Registry>().0.get("alpha").copied();
                    let active =
                        alpha.and_then(|h| h.update(cx, |_, w, _| w.is_window_active()).ok());
                    println!("alpha is the active window: {active:?}");
                });
            })
            .detach();
        });
}
