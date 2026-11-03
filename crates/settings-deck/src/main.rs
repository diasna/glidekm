//! glidekm Settings: native settings window for the Linux / SteamOS client.
//!
//! Talks to the background client (glidekm-client) only through files:
//! it edits the config file (the client applies changes live) and reads the
//! client's status file and log from the runtime directory.

mod sys;

use eframe::egui::{self, Color32, RichText};
use glidekm_core::ipc::{self, ClientConfig, Status};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const UINPUT_FIX: &str = "echo 'KERNEL==\"uinput\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"' | sudo tee /etc/udev/rules.d/60-glidekm-uinput.rules && sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=uinput";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("glidekm-settings {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("glidekm Settings")
            .with_app_id("glidekm-settings")
            .with_inner_size([760.0, 720.0])
            .with_min_inner_size([520.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "glidekm Settings",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[derive(Clone, PartialEq)]
struct Draft {
    pass: String,
    port: u16,
    auto_size: bool,
    width: u16,
    height: u16,
    auto_update: bool,
}

impl Draft {
    fn from(c: &ClientConfig) -> Draft {
        Draft {
            pass: String::new(),
            port: c.port,
            auto_size: c.fixed_size().is_none(),
            width: c.width,
            height: c.height,
            auto_update: c.auto_update,
        }
    }
}

enum Msg {
    None,
    Info(String),
    Ok(String),
    Err(String),
}

struct App {
    path: PathBuf,
    saved_cfg: ClientConfig,
    saved: Draft,
    draft: Draft,
    show_pass: bool,
    status: Option<Status>,
    last_poll: Instant,
    started_at: Option<Instant>,
    ips: Vec<String>,
    hostname: String,
    autostart: Option<bool>,
    log: Vec<String>,
    msg: Msg,
    copied: Option<(&'static str, Instant)>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> App {
        cc.egui_ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.button_padding = egui::vec2(12.0, 6.0);
            style.spacing.interact_size.y = 28.0;
        });

        let path = ipc::client_config_path();
        let cfg = ClientConfig::load(&path).unwrap_or_default();
        let draft = Draft::from(&cfg);
        let mut app = App {
            path,
            saved_cfg: cfg,
            saved: draft.clone(),
            draft,
            show_pass: false,
            status: Status::read_live(),
            last_poll: Instant::now(),
            started_at: None,
            ips: sys::local_ipv4s(),
            hostname: sys::hostname(),
            autostart: sys::autostart(),
            log: Vec::new(),
            msg: Msg::None,
            copied: None,
        };
        if app.status.is_none() {
            app.start_client();
        }
        if app.saved_cfg.passphrase.is_empty() {
            app.msg = Msg::Info("Set a passphrase (the same as on the PC), then Save.".into());
        }
        app
    }

    fn start_client(&mut self) {
        match sys::start_client() {
            Ok(how) => {
                self.started_at = Some(Instant::now());
                self.msg = Msg::Info(format!("Starting glidekm ({how})…"));
            }
            Err(e) => self.msg = Msg::Err(e),
        }
    }

    fn poll(&mut self) {
        if self.last_poll.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_poll = Instant::now();
        self.status = Status::read_live();
        if self.status.is_some() && self.started_at.take().is_some() {
            if let Msg::Info(_) = self.msg {
                self.msg = Msg::None;
            }
        }
        self.log = sys::tail(&ipc::log_path(), 200);
        self.ips = sys::local_ipv4s();
    }

    fn dirty(&self) -> bool {
        self.draft != self.saved
    }

    fn save(&mut self) {
        let d = &self.draft;
        let cfg = ClientConfig {
            passphrase: if d.pass.is_empty() {
                self.saved_cfg.passphrase.clone()
            } else {
                d.pass.clone()
            },
            port: d.port,
            width: if d.auto_size { 0 } else { d.width },
            height: if d.auto_size { 0 } else { d.height },
            auto_update: d.auto_update,
        };
        match cfg.save(&self.path) {
            Ok(()) => {
                self.saved_cfg = cfg;
                self.draft.pass.clear();
                self.show_pass = false;
                self.saved = self.draft.clone();
                self.msg = Msg::Ok("Saved — applied immediately.".into());
            }
            Err(e) => self.msg = Msg::Err(e),
        }
    }

    fn copy(&mut self, ctx: &egui::Context, what: &'static str, text: String) {
        ctx.copy_text(text);
        self.copied = Some((what, Instant::now()));
    }

    fn copy_label(&self, what: &'static str) -> &'static str {
        match self.copied {
            Some((w, t)) if w == what && t.elapsed() < Duration::from_millis(1500) => "Copied",
            _ => "Copy",
        }
    }
}

fn green(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(74, 222, 128)
    } else {
        Color32::from_rgb(22, 163, 74)
    }
}
fn red(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(248, 113, 113)
    } else {
        Color32::from_rgb(220, 38, 38)
    }
}
fn amber(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(251, 191, 36)
    } else {
        Color32::from_rgb(217, 119, 6)
    }
}
fn blue(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(96, 165, 250)
    } else {
        Color32::from_rgb(37, 99, 235)
    }
}

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(14))
        .corner_radius(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(15.0));
            ui.add_space(4.0);
            add(ui);
        });
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.poll();
        ctx.request_repaint_after(Duration::from_millis(500));
        let dark = root.visuals().dark_mode;
        let st = self.status.clone();
        let passphrase_set = !self.saved_cfg.passphrase.is_empty();

        // ---- header
        egui::Panel::top("header").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("glidekm").strong().size(18.0));
                ui.label(RichText::new("Steam Deck").weak());
                ui.add_space(12.0);
                let (color, text) = match &st {
                    None => (red(dark), "Not running".to_string()),
                    Some(_) if !passphrase_set => {
                        (Color32::GRAY, "Set a passphrase to start".into())
                    }
                    Some(s) if !s.input_error.is_empty() => (red(dark), "No input access".into()),
                    Some(s) if !s.net_error.is_empty() => (red(dark), "Network problem".into()),
                    Some(s) if s.active => (
                        blue(dark),
                        if s.capture {
                            "In use · game mouse".into()
                        } else {
                            "In use".into()
                        },
                    ),
                    Some(s) if !s.server.is_empty() => {
                        (green(dark), format!("Connected to {}", s.server))
                    }
                    Some(_) => (amber(dark), "Waiting for the PC…".into()),
                };
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 5.0, color);
                ui.label(text);
            });
            ui.add_space(6.0);
        });

        // ---- footer: save bar
        egui::Panel::bottom("bar").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                match &self.msg {
                    Msg::None if self.dirty() => {
                        ui.label(RichText::new("Unsaved changes").weak());
                    }
                    Msg::None => {}
                    Msg::Info(m) => {
                        ui.label(RichText::new(m).weak());
                    }
                    Msg::Ok(m) => {
                        ui.label(RichText::new(m).color(green(dark)));
                    }
                    Msg::Err(m) => {
                        ui.label(RichText::new(m).color(red(dark)));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save = ui.add_enabled(
                        self.dirty(),
                        egui::Button::new(RichText::new("Save").strong()),
                    );
                    if save.clicked() {
                        self.save();
                    }
                    if ui
                        .add_enabled(self.dirty(), egui::Button::new("Revert"))
                        .clicked()
                    {
                        self.draft = self.saved.clone();
                        self.msg = Msg::None;
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                if st.is_none() {
                    egui::Frame::new()
                        .fill(red(dark).gamma_multiply(0.12))
                        .stroke(egui::Stroke::new(1.0, red(dark)))
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("glidekm is not running.").strong());
                                if self.started_at.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
                                    ui.spinner();
                                } else if ui.button("Start").clicked() {
                                    self.start_client();
                                }
                            });
                        });
                    ui.add_space(8.0);
                }

                // ---- This Deck
                section(ui, "This Deck", |ui| {
                    ui.label(RichText::new("Enter this address on your PC (or press Scan in the PC's glidekm settings).").weak());
                    let port = st.as_ref().map(|s| s.port).unwrap_or(self.saved_cfg.port);
                    let ip = self.ips.first().cloned().unwrap_or_else(|| "no network".into());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&ip).monospace().size(26.0).strong());
                        ui.label(RichText::new(format!(":{port}")).monospace().weak());
                        let label = self.copy_label("ip");
                        if ui.button(label).clicked() {
                            let text = if port == glidekm_core::DEFAULT_PORT { ip.clone() } else { format!("{ip}:{port}") };
                            self.copy(ctx, "ip", text);
                        }
                    });
                    let mut extra: Vec<String> = Vec::new();
                    if !self.hostname.is_empty() {
                        extra.push(format!("{}.local", self.hostname));
                    }
                    extra.extend(self.ips.iter().skip(1).cloned());
                    if !extra.is_empty() {
                        ui.label(RichText::new(extra.join("   ·   ")).monospace().weak());
                    }
                });
                ui.add_space(8.0);

                // ---- Status
                section(ui, "Status", |ui| {
                    egui::Grid::new("status").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                        let weak = |t: &str| RichText::new(t.to_string()).weak();
                        ui.label(weak("PC"));
                        match &st {
                            Some(s) if !s.server.is_empty() => ui.label(
                                RichText::new(format!("{}{}", if s.active { "In use from " } else { "Connected: " }, s.server))
                                    .color(green(dark)),
                            ),
                            Some(s) if !s.last_rejected.is_empty() => ui.label(RichText::new(&s.last_rejected).color(red(dark))),
                            Some(_) if !passphrase_set => ui.label(RichText::new("Set a passphrase first").color(amber(dark))),
                            Some(_) => ui.label(RichText::new("Waiting for the PC").color(amber(dark))),
                            None => ui.label("–"),
                        };
                        ui.end_row();
                        ui.label(weak("Mouse & keyboard"));
                        match &st {
                            Some(s) if !s.input_error.is_empty() => ui.label(RichText::new("No access to /dev/uinput").color(red(dark))),
                            Some(_) => ui.label(RichText::new("Ready").color(green(dark))),
                            None => ui.label("–"),
                        };
                        ui.end_row();
                        ui.label(weak("Screen"));
                        match &st {
                            Some(s) => ui.label(format!(
                                "{}×{} · {}",
                                s.width,
                                s.height,
                                if s.screen_source.is_empty() {
                                    if s.screen_auto { "auto" } else { "manual" }
                                } else {
                                    s.screen_source.as_str()
                                }
                            )),
                            None => ui.label("–"),
                        };
                        ui.end_row();
                        ui.label(weak("Network"));
                        match &st {
                            Some(s) if !s.net_error.is_empty() => ui.label(RichText::new(&s.net_error).color(red(dark))),
                            Some(s) => ui.label(format!("Listening on UDP {}", s.port)),
                            None => ui.label("–"),
                        };
                        ui.end_row();
                    });
                    if st.as_ref().is_some_and(|s| !s.input_error.is_empty()) {
                        ui.add_space(6.0);
                        egui::Frame::new()
                            .fill(red(dark).gamma_multiply(0.10))
                            .stroke(egui::Stroke::new(1.0, red(dark)))
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::same(10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(RichText::new("glidekm can't create its virtual mouse and keyboard.").strong());
                                ui.label("Allow it once. This needs your password (set one with `passwd` in Konsole if you haven't).");
                                ui.horizontal(|ui| {
                                    if sys::has_pkexec() && ui.button("Fix now…").clicked() {
                                        match sys::fix_uinput() {
                                            Ok(()) => self.msg = Msg::Ok("Permission added — glidekm picks it up within a few seconds.".into()),
                                            Err(e) => self.msg = Msg::Err(e),
                                        }
                                    }
                                    let label = self.copy_label("fix");
                                    if ui.button(format!("{label} command")).clicked() {
                                        self.copy(ctx, "fix", UINPUT_FIX.to_string());
                                    }
                                    ui.label(RichText::new("…or paste it into Konsole.").weak());
                                });
                            });
                    }
                });
                ui.add_space(8.0);

                // ---- Settings
                section(ui, "Settings", |ui| {
                    ui.label(
                        RichText::new(format!("Saved to {} and applied immediately.", self.path.display()))
                            .weak(),
                    );
                    ui.add_space(2.0);
                    ui.label(RichText::new("Passphrase").strong());
                    ui.horizontal(|ui| {
                        let hint = if passphrase_set { "•••••••• (unchanged)" } else { "required — same as on the PC" };
                        ui.add(
                            egui::TextEdit::singleline(&mut self.draft.pass)
                                .password(!self.show_pass)
                                .hint_text(hint)
                                .desired_width(280.0),
                        );
                        if ui.button(if self.show_pass { "Hide" } else { "Show" }).clicked() {
                            self.show_pass = !self.show_pass;
                        }
                        if ui.button("Generate").clicked() {
                            self.draft.pass = sys::generate_passphrase();
                            self.show_pass = true;
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("UDP port").strong());
                        ui.add(egui::DragValue::new(&mut self.draft.port).range(1..=65535));
                    });
                    ui.add_space(4.0);
                    let cur = st.as_ref().map(|s| (s.width, s.height)).unwrap_or((1280, 800));
                    if ui
                        .checkbox(&mut self.draft.auto_size, "Detect screen size automatically")
                        .on_hover_text("Handles the Deck's rotated panel and docking")
                        .changed()
                        && !self.draft.auto_size
                        && self.draft.width == 0
                    {
                        self.draft.width = cur.0;
                        self.draft.height = cur.1;
                    }
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        if self.draft.auto_size {
                            // Display only: the detected size; never touches the draft.
                            let (mut w, mut h) = cur;
                            ui.add_enabled(false, egui::DragValue::new(&mut w).suffix(" px"));
                            ui.label("×");
                            ui.add_enabled(false, egui::DragValue::new(&mut h).suffix(" px"));
                        } else {
                            ui.add(egui::DragValue::new(&mut self.draft.width).range(320..=16384).suffix(" px"));
                            ui.label("×");
                            ui.add(egui::DragValue::new(&mut self.draft.height).range(200..=16384).suffix(" px"));
                        }
                    });
                    if let Some(mut on) = self.autostart {
                        ui.add_space(4.0);
                        if ui
                            .checkbox(&mut on, "Start automatically (Desktop and Game Mode)")
                            .changed()
                        {
                            match sys::set_autostart(on) {
                                Ok(()) => {
                                    self.autostart = Some(on);
                                    self.msg = Msg::Ok(if on { "Starts automatically.".into() } else { "Won't start automatically.".into() });
                                }
                                Err(e) => self.msg = Msg::Err(e),
                            }
                        }
                    }
                });
                ui.add_space(8.0);

                // ---- Updates
                section(ui, "Updates", |ui| {
                    let mine = env!("CARGO_PKG_VERSION");
                    let running = st.as_ref().map(|s| s.version.clone()).unwrap_or_default();
                    if !running.is_empty() && glidekm_core::update::newer(&running, mine) {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("glidekm was updated to {running}.")).color(green(dark)));
                            if ui.button("Restart this window").clicked() {
                                sys::restart_self();
                            }
                        });
                    }
                    ui.label(format!("glidekm {mine}"));
                    match &st {
                        Some(s) if s.update_unsupported => {
                            ui.label(RichText::new(&s.update_status).weak());
                        }
                        Some(s) => {
                            ui.checkbox(&mut self.draft.auto_update, "Install updates automatically (signed releases from GitHub)");
                            ui.horizontal(|ui| {
                                let label = if s.update_available.is_empty() {
                                    "Check for updates".to_string()
                                } else {
                                    format!("Install {}", s.update_available)
                                };
                                if ui.button(label).clicked() {
                                    match sys::request_update_check() {
                                        Ok(()) => self.msg = Msg::Info("Checking for updates…".into()),
                                        Err(e) => self.msg = Msg::Err(e),
                                    }
                                }
                                if !s.update_status.is_empty() {
                                    ui.label(RichText::new(&s.update_status).weak());
                                }
                            });
                        }
                        None => {
                            ui.label(RichText::new("Start glidekm to check for updates.").weak());
                        }
                    }
                });
                ui.add_space(8.0);

                egui::CollapsingHeader::new(RichText::new("Log").strong()).show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for l in &self.log {
                                let t = RichText::new(l).monospace().size(11.5);
                                ui.label(if l.contains(" WARN ") { t.color(amber(dark)) } else { t });
                            }
                        });
                });
            });
        });
    }
}
