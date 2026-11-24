//! The New connection tab: database type list, the form, the address preview and
//! Test connection / Save and connect. Also edits a saved connection (Save).

use std::sync::Arc;
use std::time::Instant;

use deadlock_core::connection::{ConnectionConfig, Encryption, Engine, Environment};
use deadlock_core::driver::{Driver, DriverError, connect};
use deadlock_core::ssh::{self, HostKey, KnownHosts, SshAuth, SshConfig, TunnelError};
use gpui_kit::component::IndexPath;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::color_picker::{ColorPickerEvent, ColorPickerState, ColorSelect};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, div, px, relative, white,
};

use crate::app::Db;
use crate::assets::{Icon, icon};
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind, button};

/// Sent to the shell, which owns the project's connections.
pub enum FormEvent {
    Cancel,
    /// Connected; the shell saves `config` and keeps `driver` open.
    Connected {
        config: Box<ConnectionConfig>,
        password: String,
        /// The SSH password or key passphrase, for the keychain.
        ssh_secret: Option<String>,
        driver: Arc<dyn Driver>,
    },
    /// An edited connection, same id, not connected: the shell decides on reconnecting.
    Saved {
        config: Box<ConnectionConfig>,
        /// `None`: the password field was left empty, so the stored one stays.
        password: Option<String>,
        ssh_secret: Option<String>,
    },
}

/// A secret field's value: in an edit an empty field means "keep the stored one".
fn typed_secret(value: &str, editing: bool) -> Option<String> {
    (!(editing && value.is_empty())).then(|| value.to_owned())
}

/// A failed connect: an SSH host key to decide on, or the error text.
pub(crate) enum Failure {
    UnknownHost(Box<HostKey>),
    /// Oracle Thin cannot sign in: the account has only an old password verifier.
    OldVerifier(String),
    Other(String),
}

/// Connects, keeping an unknown SSH host key apart from other errors.
pub(crate) async fn try_connect(
    config: &ConnectionConfig,
    password: &str,
) -> Result<Arc<dyn Driver>, Failure> {
    connect(config, password).await.map_err(|e| match e {
        DriverError::Tunnel(TunnelError::UnknownHost(k)) => Failure::UnknownHost(k),
        e @ DriverError::OldVerifier(_) => Failure::OldVerifier(e.to_string()),
        e => Failure::Other(e.to_string()),
    })
}

/// Asks whether to trust an SSH server's unknown host key, and records it in the app's
/// known hosts when the user agrees. Resolves to the answer, or an error text.
pub(crate) fn ask_trust(
    key: &HostKey,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<bool, String>> {
    let answer = crate::dialog::ask(
        window,
        cx,
        crate::dialog::Ask::new(
            format!("Trust SSH server {}:{}?", key.host, key.port),
            &["Trust and connect", "Cancel"],
        )
        .text(
            "Deadlock has not connected to this SSH server before. Compare the fingerprint \
             with the server's own (ssh-keygen -lf on its host key). Trusting adds it to \
             Deadlock's known hosts, not ~/.ssh/known_hosts.",
        )
        .statement(format!(
            "{} key fingerprint:\n{}",
            key.algorithm, key.fingerprint
        )),
    );
    let key = key.clone();
    cx.spawn(async move |_| match answer.await {
        Some(0) => ssh::trust(&key, &KnownHosts::default()).map(|()| true),
        _ => Ok(false),
    })
}

enum Status {
    Idle,
    Busy(&'static str),
    Ok(String),
    Failed(String),
    /// Failed on Oracle's old password verifier: the Thick driver can sign in.
    OldVerifier(String),
}

type Choice = Entity<SelectState<Vec<&'static str>>>;

pub struct ConnectionForm {
    /// The saved connection being edited; `None` for a new one.
    editing: Option<ConnectionConfig>,
    /// The stored password, for Test connection while the field is left empty.
    pub known_password: Option<String>,
    engine: Engine,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    environment: Choice,
    /// The tint as `#rrggbb`; `None` for no colour.
    color: Option<String>,
    /// The custom colour field; kept in step with `color`.
    color_picker: Entity<ColorPickerState>,
    encryption: Choice,
    /// Oracle: whether the database field is a service name or a SID.
    connect_by: Choice,
    /// Oracle: Thin (built in) or Thick (Oracle Instant Client).
    driver: Choice,
    keychain: bool,
    read_only: bool,
    tunnel: bool,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_auth: Choice,
    ssh_key: Entity<InputState>,
    /// The SSH password, or the key's passphrase.
    ssh_secret: Entity<InputState>,
    status: Status,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FormEvent> for ConnectionForm {}

impl ConnectionForm {
    /// A form prefilled from `source` (a duplicate when `editing` is off), or empty.
    pub fn new(
        source: Option<&ConnectionConfig>,
        editing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let defaults = source
            .cloned()
            .unwrap_or_else(|| ConnectionConfig::new(Engine::Mysql));
        let ssh = &defaults.ssh;
        let unchanged = if editing {
            "Unchanged (type to replace)"
        } else {
            ""
        };
        let mut input = |value: &str, placeholder: &str, masked: bool| {
            let (value, placeholder) = (value.to_owned(), placeholder.to_owned());
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value)
                    .placeholder(placeholder)
                    .masked(masked)
            })
        };
        let name = match (source, editing) {
            (Some(c), false) => format!("{} copy", c.name),
            _ => defaults.name.clone(),
        };
        let name = input(&name, "e.g. shop_prod", false);
        let host = input(&defaults.host, "", false);
        let port = input(&defaults.port.to_string(), "", false);
        let database = input(&defaults.database, "", false);
        let user = input(&defaults.user, "", false);
        let password = input("", if editing { unchanged } else { "Enter password" }, true);
        let ssh_host = input(&ssh.host, "e.g. bastion.example.com", false);
        let ssh_port = input(&ssh.port.to_string(), "", false);
        let ssh_user = input(&ssh.user, "", false);
        let key_file = if ssh.key_file.is_empty() {
            "~/.ssh/id_ed25519"
        } else {
            &ssh.key_file
        };
        let ssh_key = input(key_file, "", false);
        let ssh_secret = input("", unchanged, true);
        let mut choice = |labels: Vec<&'static str>, selected: usize| {
            cx.new(|cx| {
                SelectState::new(labels, Some(IndexPath::default().row(selected)), window, cx)
            })
        };
        let environment = choice(
            Environment::ALL.map(Environment::label).to_vec(),
            Environment::ALL
                .iter()
                .position(|e| *e == defaults.environment)
                .unwrap_or(0),
        );
        let encryption = choice(
            Encryption::ALL.map(Encryption::label).to_vec(),
            Encryption::ALL
                .iter()
                .position(|e| *e == defaults.encryption)
                .unwrap_or(1),
        );
        let connect_by = choice(vec!["Service name", "SID"], usize::from(defaults.sid));
        let driver = choice(
            vec!["Thin (built in)", "Thick (Oracle Instant Client)"],
            usize::from(defaults.thick),
        );
        let ssh_auth = choice(
            SshAuth::ALL.map(SshAuth::label).to_vec(),
            SshAuth::ALL
                .iter()
                .position(|a| *a == ssh.auth)
                .unwrap_or(0),
        );
        // The address preview and the status line follow every keystroke.
        let mut _subscriptions: Vec<Subscription> = [
            &name,
            &host,
            &port,
            &database,
            &user,
            &password,
            &ssh_host,
            &ssh_port,
            &ssh_user,
            &ssh_key,
            &ssh_secret,
        ]
        .into_iter()
        .map(|s| {
            cx.subscribe(s, |this: &mut Self, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Change) {
                    this.status = Status::Idle;
                    cx.notify();
                }
            })
        })
        .collect();
        let color = defaults
            .color
            .clone()
            .filter(|c| crate::color::parse_hex(c).is_some());
        let color_picker = cx.new(|cx| {
            let state = ColorPickerState::new(window, cx);
            match color.as_deref().and_then(crate::color::parse_hex) {
                Some(c) => state.default_value(c),
                None => state,
            }
        });
        _subscriptions.push(cx.subscribe(
            &color_picker,
            |this: &mut Self, _, e: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(c) = e;
                this.color = c.map(crate::color::to_hex);
                cx.notify();
            },
        ));
        for s in [&environment, &encryption, &connect_by, &ssh_auth] {
            _subscriptions.push(cx.observe(s, |_, _, cx| cx.notify()));
        }
        _subscriptions.push(cx.observe(&driver, |this: &mut Self, _, cx| {
            this.status = Status::Idle;
            cx.notify();
        }));
        Self {
            editing: source.filter(|_| editing).cloned(),
            known_password: None,
            engine: defaults.engine,
            name,
            host,
            port,
            database,
            user,
            password,
            environment,
            color,
            color_picker,
            encryption,
            connect_by,
            driver,
            keychain: source.is_none_or(|c| c.keychain),
            read_only: defaults.read_only,
            tunnel: defaults.ssh_tunnel,
            ssh_host,
            ssh_port,
            ssh_user,
            ssh_auth,
            ssh_key,
            ssh_secret,
            status: Status::Idle,
            _subscriptions,
        }
    }

    /// The None swatch and the palette; the chosen one carries a ring and a check.
    fn swatches(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let all = std::iter::once(("None", None)).chain(
            crate::color::PALETTE
                .iter()
                .map(|(name, hex)| (*name, Some(*hex))),
        );
        all.map(|(name, hex)| {
            let on = self.color.as_deref().map(str::to_lowercase).as_deref() == hex;
            let fill = hex.and_then(crate::color::parse_hex);
            div()
                .id(SharedString::from(format!("swatch-{name}")))
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
                    gpui_kit::component::tooltip::Tooltip::new(name).build(window, cx)
                })
                .clickable(cx.listener(move |this, _, window, cx| {
                    this.color = hex.map(str::to_owned);
                    this.color_picker.update(cx, |s, cx| {
                        match hex.and_then(crate::color::parse_hex) {
                            Some(c) => s.set_value(c, window, cx),
                            None => s.clear_value(window, cx),
                        }
                    });
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect()
    }

    fn set_engine(&mut self, engine: Engine, window: &mut Window, cx: &mut Context<Self>) {
        let old = self.engine;
        self.engine = engine;
        // A port the user typed is kept; the old default follows the type.
        let port = self.port.read(cx).value();
        if port.is_empty() || port == old.default_port().to_string() {
            self.port.update(cx, |s, cx| {
                s.set_value(engine.default_port().to_string(), window, cx);
            });
        }
        self.status = Status::Idle;
        cx.notify();
    }

    fn text(state: &Entity<InputState>, cx: &App) -> String {
        state.read(cx).value().trim().to_owned()
    }

    fn selected<T: Copy>(state: &Choice, all: &[T], cx: &App) -> Option<T> {
        state
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| all.get(ix.row).copied())
    }

    /// The connection being edited, if any.
    pub fn editing(&self) -> Option<&ConnectionConfig> {
        self.editing.as_ref()
    }

    /// The form as a connection. A new one's id is assigned by the shell when saving;
    /// an edit keeps the id.
    fn config(&self, cx: &App) -> ConnectionConfig {
        let host = Self::text(&self.host, cx);
        let name = Self::text(&self.name, cx);
        ConnectionConfig {
            name: if name.is_empty() { host.clone() } else { name },
            port: Self::text(&self.port, cx).parse().unwrap_or(0),
            host,
            database: Self::text(&self.database, cx),
            user: Self::text(&self.user, cx),
            environment: Self::selected(&self.environment, &Environment::ALL, cx)
                .unwrap_or_default(),
            color: self.color.clone(),
            encryption: Self::selected(&self.encryption, &Encryption::ALL, cx).unwrap_or_default(),
            read_only: self.read_only,
            keychain: self.keychain,
            sid: self.sid(cx),
            thick: self.thick(cx),
            ssh_tunnel: self.tunnel,
            ssh: if self.tunnel {
                SshConfig {
                    host: Self::text(&self.ssh_host, cx),
                    port: Self::text(&self.ssh_port, cx).parse().unwrap_or(0),
                    user: Self::text(&self.ssh_user, cx),
                    auth: self.auth(cx),
                    key_file: if self.auth(cx) == SshAuth::Key {
                        Self::text(&self.ssh_key, cx)
                    } else {
                        String::new()
                    },
                }
            } else {
                SshConfig::default()
            },
            id: self
                .editing
                .as_ref()
                .map(|c| c.id.clone())
                .unwrap_or_default(),
            hidden_schemas: self
                .editing
                .as_ref()
                .map(|c| c.hidden_schemas.clone())
                .unwrap_or_default(),
            ..ConnectionConfig::new(self.engine)
        }
    }

    fn auth(&self, cx: &App) -> SshAuth {
        Self::selected(&self.ssh_auth, &SshAuth::ALL, cx).unwrap_or_default()
    }

    /// The SSH secret typed, remembered for every session of the connection.
    fn ssh_secret(&self, config: &ConnectionConfig, cx: &App) -> Option<String> {
        if !config.ssh_tunnel || config.ssh.auth == SshAuth::Agent {
            return None;
        }
        let secret = typed_secret(&self.ssh_secret.read(cx).value(), self.editing.is_some())?;
        ssh::remember(&config.ssh, secret.clone());
        Some(secret)
    }

    fn thick(&self, cx: &App) -> bool {
        self.engine == Engine::Oracle
            && self
                .driver
                .read(cx)
                .selected_index(cx)
                .is_some_and(|ix| ix.row == 1)
    }

    /// The fix offered when Thin cannot sign in: pick Thick and test again.
    fn switch_to_thick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.driver.update(cx, |s, cx| {
            s.set_selected_index(Some(IndexPath::default().row(1)), window, cx);
        });
        self.attempt(false, window, cx);
    }

    fn sid(&self, cx: &App) -> bool {
        self.engine == Engine::Oracle
            && self
                .connect_by
                .read(cx)
                .selected_index(cx)
                .is_some_and(|ix| ix.row == 1)
    }

    /// Checks what the server cannot: an empty host, a port that is not a number.
    fn validate(&self, cx: &App) -> Result<ConnectionConfig, String> {
        let config = self.config(cx);
        if config.host.is_empty() {
            return Err("Enter a host.".into());
        }
        if config.port == 0 {
            return Err("Port must be a number from 1 to 65535.".into());
        }
        if config.ssh_tunnel {
            if config.ssh.host.is_empty() || config.ssh.user.is_empty() {
                return Err("Enter the SSH host and user.".into());
            }
            if config.ssh.port == 0 {
                return Err("SSH port must be a number from 1 to 65535.".into());
            }
            if config.ssh.auth == SshAuth::Key && config.ssh.key_file.is_empty() {
                return Err("Enter the SSH key file.".into());
            }
        }
        Ok(config)
    }

    /// Test connection (`save` off) or Save and connect. An unknown SSH host key is
    /// asked about, and the attempt repeats once it is trusted.
    fn attempt(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let config = match self.validate(cx) {
            Ok(c) => c,
            Err(e) => return self.fail(e, cx),
        };
        let typed = typed_secret(&self.password.read(cx).value(), self.editing.is_some());
        let ssh_secret = self.ssh_secret(&config, cx);
        if save && self.editing.is_some() {
            cx.emit(FormEvent::Saved {
                config: Box::new(config),
                password: typed,
                ssh_secret,
            });
            return;
        }
        let password = typed
            .or_else(|| self.known_password.clone())
            .unwrap_or_default();
        let (c, p) = (config.clone(), password.clone());
        let rx = cx.global::<Db>().0.spawn(async move {
            let started = Instant::now();
            try_connect(&c, &p).await.map(|d| (started.elapsed(), d))
        });
        self.status = Status::Busy(if save { "Connecting…" } else { "Testing…" });
        cx.notify();
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(Failure::Other("connection task failed".into())));
            let key = match result {
                Ok((took, driver)) => {
                    let _ = this.update(cx, |this, cx| {
                        if save {
                            cx.emit(FormEvent::Connected {
                                config: Box::new(config),
                                password,
                                ssh_secret,
                                driver,
                            });
                        } else {
                            // The test connection closes on the runtime.
                            let version = driver.server_version().to_owned();
                            drop(cx.global::<Db>().0.spawn(async move { drop(driver) }));
                            this.status = Status::Ok(format!(
                                "Connected in {} ms to {version}",
                                took.as_millis()
                            ));
                            cx.notify();
                        }
                    });
                    return;
                }
                Err(Failure::Other(e)) => {
                    let _ = this.update(cx, |this, cx| this.fail(e, cx));
                    return;
                }
                Err(Failure::OldVerifier(e)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = Status::OldVerifier(e);
                        cx.notify();
                    });
                    return;
                }
                Err(Failure::UnknownHost(key)) => key,
            };
            let asked = cx.update_window(handle, |_, window, cx| ask_trust(&key, window, cx));
            let Ok(answer) = asked else { return };
            let retry = match answer.await {
                Ok(true) => None,
                Ok(false) => Some(TunnelError::UnknownHost(key).to_string()),
                Err(e) => Some(e),
            };
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| match retry {
                    None => this.attempt(save, window, cx),
                    Some(e) => this.fail(e, cx),
                })
            });
        })
        .detach();
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.status = Status::Failed(message);
        cx.notify();
    }
}

/// A labelled field (`.fl`): label above, 6 px gap, medium weight.
fn field(label: &'static str, input: impl IntoElement) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .min_w_0()
        .font_weight(FontWeight::MEDIUM)
        .child(label)
        .child(div().font_weight(FontWeight::NORMAL).child(input))
}

fn text_input(state: &Entity<InputState>, mono: bool) -> Input {
    // `Styled::h`, not `Input::h` (multi-line only): the design's 32 px field.
    let input = Styled::h(Input::new(state), px(32.));
    if mono {
        input.font_family(MONO_FONT).text_size(data_size())
    } else {
        input
    }
}

/// Two fields side by side that wrap on narrow widths.
fn pair(a: impl IntoElement, b: impl IntoElement) -> gpui_kit::Div {
    div()
        .flex()
        .flex_wrap()
        .gap_x(px(12.))
        .gap_y(px(16.))
        .child(a)
        .child(b)
}

/// A fieldset: 13 px semibold legend, 12 px below it, 16 px between fields.
fn section(legend: &'static str, children: Vec<AnyElement>) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .mb(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(legend),
        )
        .child(div().flex().flex_col().gap(px(16.)).children(children))
}

impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let swatches = self.swatches(&p, cx);
        let engine = self.engine;

        let types = div()
            .flex_none()
            .w(px(200.))
            .py(px(20.))
            .px(px(12.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .border_r_1()
            .border_color(p.line)
            .child(
                div()
                    .mb(px(8.))
                    .px(px(10.))
                    .text_size(small_size())
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.tx2)
                    .child("Database type"),
            )
            .children(Engine::ALL.map(|e| {
                let on = e == engine;
                div()
                    .id(e.label())
                    .flex()
                    .items_center()
                    .h(px(32.))
                    .px(px(10.))
                    .rounded(RADIUS)
                    .whitespace_nowrap()
                    .when(on, |d| d.bg(p.sel).font_weight(FontWeight::MEDIUM))
                    .when(!on, |d| d.hover(|s| s.bg(p.hover)))
                    .clickable(
                        cx.listener(move |this, _, window, cx| this.set_engine(e, window, cx)),
                    )
                    .child(e.label())
            }));

        let database_label = match engine {
            Engine::Mysql | Engine::Mariadb => "Database",
            Engine::Oracle if self.sid(cx) => "SID",
            Engine::Oracle => "Service name",
            Engine::Redis => "Database index",
        };
        let database = field(database_label, text_input(&self.database, true));
        let database =
            if engine == Engine::Oracle {
                div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(pair(
                    field("Connect by", Select::new(&self.connect_by).h(px(32.)))
                        .flex_grow(1.)
                        .flex_basis(px(140.)),
                    database.flex_grow(3.).flex_basis(px(240.)),
                ))
                .child(div().text_size(small_size()).text_color(p.tx3).child(
                    "Easy Connect (host:port/service). TNS aliases and wallets are not supported.",
                ))
                .child(
                    field("Driver", Select::new(&self.driver).h(px(32.)))
                        .max_w(px(320.))
                        .pt(px(10.)),
                )
                .child(div().text_size(small_size()).text_color(p.tx3).child(
                    if self.thick(cx) {
                        "Thick loads Oracle Instant Client, which must be installed on this \
                         computer. It signs in to accounts with old password types that Thin \
                         cannot."
                    } else {
                        "Thin is built in and needs nothing installed."
                    },
                ))
            } else {
                database
            };
        let server = section(
            "Server",
            vec![
                field("Name", text_input(&self.name, false)).into_any_element(),
                pair(
                    field("Host", text_input(&self.host, true))
                        .flex_grow(3.)
                        .flex_basis(px(240.)),
                    field("Port", text_input(&self.port, true))
                        .flex_grow(1.)
                        .flex_basis(px(96.)),
                )
                .into_any_element(),
                database.into_any_element(),
            ],
        );

        let keychain = self.keychain;
        let sign_in = section(
            "Sign-in",
            vec![
                pair(
                    field(
                        if engine == Engine::Redis {
                            "User (optional)"
                        } else {
                            "User"
                        },
                        text_input(&self.user, true),
                    )
                    .flex_1()
                    .flex_basis(px(200.)),
                    field("Password", text_input(&self.password, false))
                        .flex_1()
                        .flex_basis(px(200.)),
                )
                .into_any_element(),
                Checkbox::new("keychain")
                    .checked(keychain)
                    .label("Keep the password in the system keychain")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.keychain = *checked;
                        cx.notify();
                    }))
                    .into_any_element(),
            ],
        );

        let read_only = self.read_only;
        let safety = section(
            "Safety",
            vec![
                pair(
                    field("Environment", Select::new(&self.environment).h(px(32.)))
                        .flex_1()
                        .flex_basis(px(200.)),
                    field("Encryption", Select::new(&self.encryption).h(px(32.)))
                        .flex_1()
                        .flex_basis(px(200.)),
                )
                .into_any_element(),
                field(
                    "Color",
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .children(swatches)
                        .child(
                            div()
                                .w(px(150.))
                                .child(ColorSelect::new(&self.color_picker).placeholder("Custom")),
                        ),
                )
                .into_any_element(),
                Checkbox::new("read-only")
                    .checked(read_only)
                    .label("Open read-only, so statements that change data are refused")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.read_only = *checked;
                        cx.notify();
                    }))
                    .into_any_element(),
                Checkbox::new("ssh")
                    .checked(self.tunnel)
                    .label("Connect through an SSH tunnel")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.tunnel = *checked;
                        this.status = Status::Idle;
                        cx.notify();
                    }))
                    .into_any_element(),
            ],
        );
        let tunnel = self.tunnel.then(|| {
            let auth = self.auth(cx);
            let mut fields = vec![
                pair(
                    field("SSH host", text_input(&self.ssh_host, true))
                        .flex_grow(3.)
                        .flex_basis(px(240.)),
                    field("SSH port", text_input(&self.ssh_port, true))
                        .flex_grow(1.)
                        .flex_basis(px(96.)),
                )
                .into_any_element(),
                pair(
                    field("SSH user", text_input(&self.ssh_user, true))
                        .flex_1()
                        .flex_basis(px(200.)),
                    field("Authentication", Select::new(&self.ssh_auth).h(px(32.)))
                        .flex_1()
                        .flex_basis(px(200.)),
                )
                .into_any_element(),
            ];
            match auth {
                SshAuth::Key => fields.push(
                    pair(
                        field("Key file", text_input(&self.ssh_key, true))
                            .flex_1()
                            .flex_basis(px(200.)),
                        field("Passphrase (optional)", text_input(&self.ssh_secret, false))
                            .flex_1()
                            .flex_basis(px(200.)),
                    )
                    .into_any_element(),
                ),
                SshAuth::Password => fields.push(
                    field("SSH password", text_input(&self.ssh_secret, false)).into_any_element(),
                ),
                SshAuth::Agent => fields.push(
                    div()
                        .text_size(small_size())
                        .text_color(p.tx3)
                        .child("Signs in with the keys of the running SSH agent (SSH_AUTH_SOCK).")
                        .into_any_element(),
                ),
            }
            fields.push(
                div()
                    .text_size(small_size())
                    .text_color(p.tx3)
                    .child(
                        "Host and port above are the database as the SSH server sees it. The \
                         SSH server's host key is checked against ~/.ssh/known_hosts and \
                         Deadlock's own list. Passwords and passphrases go to the keychain \
                         with the database password.",
                    )
                    .into_any_element(),
            );
            section("SSH tunnel", fields)
        });

        let address = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Connection address"),
            )
            .child(
                div()
                    .py(px(8.))
                    .px(px(12.))
                    .border_1()
                    .border_color(p.line)
                    .rounded(RADIUS)
                    .bg(p.panel)
                    .font_family(MONO_FONT)
                    .text_size(data_size())
                    .child(SharedString::from(self.config(cx).address())),
            );

        let busy = matches!(self.status, Status::Busy(_));
        let status = match &self.status {
            Status::Idle => None,
            Status::Busy(label) => Some(div().text_color(p.tx2).child(*label)),
            Status::Ok(line) => Some(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_color(p.ok)
                    .font_weight(FontWeight::MEDIUM)
                    // The check mark.
                    .child(icon(Icon::Commit, p.ok))
                    .child(SharedString::from(line.clone())),
            ),
            Status::Failed(e) => Some(
                div()
                    .min_w_0()
                    .text_color(p.danger)
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(e.clone())),
            ),
            Status::OldVerifier(e) => Some(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_color(p.danger)
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(e.clone())),
                    )
                    .child(
                        button(
                            "switch-thick",
                            Kind::Outline,
                            true,
                            "Use Oracle Instant Client for this connection, and test again",
                            cx,
                        )
                        .child("Switch this connection to the Thick driver")
                        .clickable(cx.listener(|this, _, window, cx| {
                            this.switch_to_thick(window, cx);
                        })),
                    ),
            ),
        };
        let big = |b: gpui_kit::Stateful<gpui_kit::Div>| b.h(px(32.)).px(px(12.));
        let save_label = if self.editing.is_some() {
            "Save"
        } else {
            "Save and connect"
        };
        let actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(12.))
            .gap_y(px(8.))
            .pt(px(20.))
            .border_t_1()
            .border_color(p.line)
            .child(
                big(button("test", Kind::Outline, !busy, "Test connection", cx))
                    .child("Test connection")
                    .when(!busy, |b| {
                        b.clickable(
                            cx.listener(|this, _, window, cx| this.attempt(false, window, cx)),
                        )
                    }),
            )
            .children(status)
            .child(div().flex_grow(1.))
            .child(
                big(button(
                    "cancel",
                    Kind::Ghost,
                    true,
                    "Close without saving",
                    cx,
                ))
                .child("Cancel")
                .clickable(cx.listener(|_, _, _, cx| cx.emit(FormEvent::Cancel))),
            )
            .child(
                big(button("save", Kind::Primary, !busy, save_label, cx))
                    .child(save_label)
                    .when(!busy, |b| {
                        b.clickable(
                            cx.listener(|this, _, window, cx| this.attempt(true, window, cx)),
                        )
                    }),
            );

        let form = div()
            .flex_1()
            .min_w_0()
            .pt(px(24.))
            .px(px(32.))
            .pb(px(32.))
            .flex()
            .flex_col()
            .gap(px(28.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(relative(1.2))
                            .child(SharedString::from(match &self.editing {
                                Some(c) => format!("Edit {}", c.name),
                                None => format!("New {engine} connection"),
                            })),
                    )
                    .child(div().text_color(p.tx2).child(SharedString::from(format!(
                        "Nothing is saved until you choose {save_label}."
                    )))),
            )
            .child(
                div()
                    .max_w(px(640.))
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .child(server)
                    .child(sign_in)
                    .child(safety)
                    .children(tunnel)
                    .child(address)
                    .child(actions),
            );

        // The type list stays put; only the form scrolls.
        div().size_full().flex().child(types).child(
            div()
                .id("connection-form")
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_y_scroll()
                .child(form),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::typed_secret;

    #[test]
    fn empty_secret_keeps_the_stored_one_only_when_editing() {
        assert_eq!(typed_secret("", true), None);
        assert_eq!(typed_secret("s3cret", true), Some("s3cret".into()));
        // A new connection may have an empty password.
        assert_eq!(typed_secret("", false), Some(String::new()));
    }
}
