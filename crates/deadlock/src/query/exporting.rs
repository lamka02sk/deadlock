//! Export from a tab: the grid in front (a table's rows, a result in the panel or the
//! last inline result) to a file, either the loaded rows or all rows, fetched again on
//! the tab's session. A read, so read-only connections allow it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use deadlock_core::connection::Engine;
use deadlock_core::export::{self, Dialect, Format, Spec};
use gpui_kit::component::table::TableState;
use gpui_kit::{App, Context, Entity};
use tokio::sync::oneshot;

use super::{Panel, QueryTab, Session, TabEvent};
use crate::app::Db;
use crate::export::{Job, Offer, PathField, PathHost, save_path};
use crate::results::Grid;

/// What an export of all rows reads again.
enum Rerun {
    Sql(String, super::binds::Values),
    /// The Redis key tab's whole collection.
    Key,
}

impl PathHost for QueryTab {
    fn path_field(&mut self) -> &mut Option<Entity<PathField>> {
        &mut self.path_field
    }
}

impl QueryTab {
    pub fn exporting(&self) -> bool {
        self.export.as_ref().is_some_and(Job::is_running)
    }

    /// The grid an export takes: the table's rows, the result shown in the panel, or
    /// the last inline result.
    pub fn export_grid(&self, cx: &App) -> Option<Entity<TableState<Grid>>> {
        if self.table.is_some() {
            return self.data_grid();
        }
        let panel = match self.panel {
            Panel::Result(i) if !self.inline || self.panel_open => self.results.get(i).cloned(),
            _ => None,
        };
        panel
            .or_else(|| {
                self.last_block
                    .as_ref()
                    .and_then(|b| b.read(cx).grid().cloned())
            })
            .or_else(|| {
                self.blocks
                    .iter()
                    .rev()
                    .find_map(|b| b.view.read(cx).grid().cloned())
            })
            .or_else(|| self.results.first().cloned())
    }

    /// What the export menu offers for `grid`.
    pub fn export_offer(&self, grid: &Entity<TableState<Grid>>, cx: &App) -> Offer {
        let g = grid.read(cx).delegate();
        Offer {
            loaded: g.row_count(),
            complete: g.is_complete(),
            rerun: self.table.is_some() || g.source.is_some(),
            sql: self.engine() != Some(Engine::Redis),
        }
    }

    pub(super) fn engine(&self) -> Option<Engine> {
        self.connection.as_ref().map(|c| c.engine)
    }

    /// Asks where to save, then exports `grid` in `format`: all rows when `all` and
    /// they can be fetched again, otherwise the loaded ones.
    pub fn export(
        &mut self,
        grid: Entity<TableState<Grid>>,
        format: Format,
        all: bool,
        cx: &mut Context<Self>,
    ) {
        if self.is_running() || self.path_field.is_some() {
            return;
        }
        let offer = self.export_offer(&grid, cx);
        let all = all && offer.rerun && !offer.complete;
        let (rerun, spec, name) = if self.is_key() {
            // A Redis key: its whole collection is read again, page by page.
            let name = self
                .table
                .as_ref()
                .map(|t| t.table.table.replace(['/', '\\', ':'], "_"))
                .unwrap_or_default();
            let spec = Spec {
                format,
                dialect: Dialect::Mysql {
                    backslash_escapes: true,
                },
                table: name.clone(),
                keep: None,
            };
            (all.then_some(Rerun::Key), spec, name)
        } else if self.table.is_some() {
            match self.table_export(format, cx) {
                Ok((sql, spec)) => {
                    let name = spec.table.clone();
                    (all.then(|| Rerun::Sql(sql, Vec::new())), spec, name)
                }
                Err(e) => {
                    self.export_failed(e, cx);
                    return;
                }
            }
        } else {
            let dialect = if self.engine() == Some(Engine::Oracle) {
                Dialect::Oracle
            } else {
                // The server's default; NO_BACKSLASH_ESCAPES sessions are rare.
                Dialect::Mysql {
                    backslash_escapes: true,
                }
            };
            let source = grid.read(cx).delegate().source.clone();
            let spec = Spec {
                format,
                dialect,
                // Named after the file once it is chosen.
                table: String::new(),
                keep: None,
            };
            (
                source.filter(|_| all).map(|(s, b)| Rerun::Sql(s, b)),
                spec,
                "result".to_owned(),
            )
        };
        let file = format!("{name}.{}", format.extension());
        let picked = save_path(self.root.clone(), file, self.window, cx);
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else { return };
            let _ = this.update(cx, |t, cx| t.start_export(&grid, path, rerun, spec, cx));
        })
        .detach();
    }

    fn start_export(
        &mut self,
        grid: &Entity<TableState<Grid>>,
        path: PathBuf,
        rerun: Option<Rerun>,
        mut spec: Spec,
        cx: &mut Context<Self>,
    ) {
        if self.is_running() {
            return;
        }
        if spec.table.is_empty() {
            spec.table = path
                .file_stem()
                .map_or_else(|| "result".into(), |s| s.to_string_lossy().into_owned());
        }
        let file = deadlock_core::project::folder_name(&path);
        let rows = Arc::new(AtomicU64::new(0));
        let rt = Arc::clone(&cx.global::<Db>().0);
        let fetch = rerun.is_some();
        let (done, cancel) = match rerun {
            Some(rerun) => {
                let Session::Ready(d) = &self.session else {
                    self.export_failed(
                        "the tab's session is closed: run the statement again first".into(),
                        cx,
                    );
                    return;
                };
                let d = Arc::clone(d);
                // A result grid that still reads holds the session: close it, and on
                // MySQL stop the server sending the rest it would otherwise read.
                let draining = self.table.is_none() && self.grids_open(cx);
                if self.table.is_none() {
                    self.close_grids(cx);
                }
                let kill =
                    draining && matches!(self.engine(), Some(Engine::Mysql | Engine::Mariadb));
                let (tx, stop) = oneshot::channel();
                let progress = Arc::clone(&rows);
                let key = self.key_source(cx);
                let done = rt.spawn(async move {
                    if kill {
                        let _ = d.cancel().await;
                    }
                    match (rerun, key) {
                        (Rerun::Key, Some((key, kind, pattern))) => {
                            let r = Arc::clone(&d);
                            let rows: deadlock_core::driver::BoxFuture<'static, _> =
                                Box::pin(async move {
                                    r.as_redis()
                                        .ok_or(deadlock_core::driver::DriverError::Unsupported(
                                            "not a Redis connection",
                                        ))?
                                        .rows(&key, &kind, pattern)
                                });
                            export::run_from(d, rows, spec, path, progress, stop).await
                        }
                        (Rerun::Sql(sql, binds), _) => {
                            export::run(d, sql, binds, spec, path, progress, stop).await
                        }
                        (Rerun::Key, None) => Err("the key has not loaded".into()),
                    }
                });
                (done, Some(tx))
            }
            None => {
                let g = grid.read(cx).delegate();
                let (columns, data) = (g.columns().to_vec(), g.rows().to_vec());
                let progress = Arc::clone(&rows);
                let done = rt.spawn_blocking(move || {
                    let n = export::write_rows(&columns, &data, &spec, &path)?;
                    progress.store(n, Ordering::Relaxed);
                    Ok(n)
                });
                (done, None)
            }
        };
        self.export = Some(Job::start(file, rows, cancel, cx));
        cx.emit(TabEvent::Changed);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = done
                .await
                .unwrap_or_else(|_| Err("the export task stopped".into()));
            let _ = this.update(cx, |t, cx| {
                let failed = matches!(&outcome, Err(e) if e != "cancelled");
                if let Some(j) = &mut t.export {
                    j.finish(outcome);
                }
                // A failure may be a lost connection: say so, as after a statement.
                if failed && fetch {
                    t.check_tx(false, true, cx).detach();
                }
                cx.emit(TabEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether a result grid still fetches rows.
    fn grids_open(&self, cx: &App) -> bool {
        let inline = self
            .blocks
            .iter()
            .filter_map(|b| b.view.read(cx).grid().cloned());
        self.results
            .iter()
            .cloned()
            .chain(inline)
            .any(|t| t.read(cx).delegate().is_open())
    }

    fn export_failed(&mut self, error: String, cx: &mut Context<Self>) {
        let mut job = Job::start(String::new(), Arc::default(), None, cx);
        job.finish(Err(error));
        self.export = Some(job);
        cx.notify();
    }

    pub(super) fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if let Some(j) = &mut self.export {
            j.cancel();
        }
        cx.notify();
    }

    pub(super) fn close_export(&mut self, cx: &mut Context<Self>) {
        if !self.exporting() {
            self.export = None;
        }
        cx.notify();
    }
}
