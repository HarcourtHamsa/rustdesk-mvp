//! The visible-window GUI for `rd-host`: displays a 6-character code when
//! opened, which the user can copy. This identifies the remote connection.
//! Click START to begin listening for a connection (same as `rd-host listen`
//! under the hood, on the default port) - the window then shows the address
//! and password to give whoever's connecting, each with a copy button. Click
//! the power button again to stop.
//!
//! This is *not* the default anymore - running `rd-host` with no arguments
//! now goes straight to the tray-only mode in `tray.rs` (auto-starts,
//! no window). This window is reached via `rd-host --window`, for anyone
//! who wants to see/drive it interactively instead. `rd-host
//! listen`/`relay`/`with-relay` still work exactly as before too, for
//! scripted or headless use (e.g. running `rd-relay`/`rd-host relay` on a
//! VPS with no display).

use crate::gui_core::{load_logo_rgba, start_hosting, stop_hosting, HostControl, NgrokState};
use eframe::egui;
use std::time::Duration;

const ICON_SIZE: u32 = 256;
const CODE_FONT_SIZE: f32 = 56.0;

pub fn run() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([400.0, 500.0])
        .with_min_inner_size([350.0, 450.0])
        .with_resizable(false);
    if let Some(rgba) = load_logo_rgba(ICON_SIZE) {
        viewport = viewport.with_icon(egui::IconData { width: ICON_SIZE, height: ICON_SIZE, rgba });
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "rd-host",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(App::default()))
        }),
    )
}

enum PowerState {
    /// Initial state - show the 6-character code
    ShowCode { code: String },
    /// User clicked START - waiting to start hosting or hosting is active
    Off,
    On {
        address: String,
        password: String,
        control: HostControl,
    },
}

struct App {
    state: PowerState,
    error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        let code = crate::generate_random_code();
        Self { state: PowerState::ShowCode { code }, error: None }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default().show(ui, |ui| match &mut self.state {
            PowerState::ShowCode { code } => {
                if show_code(ui, code) {
                    // User clicked START, transition to Off state to start hosting
                    self.state = PowerState::Off;
                }
            }
            PowerState::Off => {
                if show_off(ui, &self.error) {
                    match start_hosting() {
                        Ok((address, password, control)) => {
                            self.error = None;
                            self.state = PowerState::On { address, password, control };
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            }
            PowerState::On { address, password, control } => {
                if show_on(ui, address, password, control) {
                    stop_hosting(control);
                    self.state = PowerState::Off;
                }
            }
        });

        // Cheap to redraw a few times a second - keeps "waiting..." vs
        // "connected: ..." current without needing input to refresh.
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

/// Display the 6-character code on startup. Returns true if START was clicked.
fn show_code(ui: &mut egui::Ui, code: &str) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 2.0 - 140.0);
        ui.heading("Your Connection Code");
        ui.add_space(24.0);

        // Display the 6-character code in a large, centered monospace font
        ui.label(
            egui::RichText::new(code)
                .size(CODE_FONT_SIZE)
                .monospace()
                .color(egui::Color32::from_rgb(100, 200, 255)),
        );

        ui.add_space(24.0);

        // Copy button for the code
        if ui.button("📋 Copy Code").on_hover_text("Copy the code to clipboard").clicked() {
            ui.ctx().copy_text(code.to_string());
        }

        ui.add_space(32.0);

        // START button to begin hosting
        let button = egui::Button::new(egui::RichText::new("START").size(18.0).strong())
            .min_size(egui::vec2(120.0, 50.0))
            .corner_radius(25.0);
        if ui.add(button).on_hover_text("Begin hosting").clicked() {
            clicked = true;
        }

        ui.add_space(12.0);
        ui.label(egui::RichText::new("Click START to begin hosting").weak().small());
    });
    clicked
}

fn show_off(ui: &mut egui::Ui, error: &Option<String>) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 2.0 - 100.0);
        ui.heading("rd-host");
        ui.add_space(28.0);

        // A literal power-symbol glyph isn't in egui's bundled font (renders
        // as an empty box), so this spells out what clicking it does instead.
        let button = egui::Button::new(egui::RichText::new("START").size(20.0).strong())
            .min_size(egui::vec2(120.0, 120.0))
            .corner_radius(60.0);
        if ui.add(button).on_hover_text("Start hosting").clicked() {
            clicked = true;
        }

        ui.add_space(16.0);
        ui.label(egui::RichText::new("Click to start hosting").weak());

        if let Some(e) = error {
            ui.add_space(12.0);
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    });
    clicked
}

/// Returns true if the power button was clicked (caller should stop hosting).
fn show_on(ui: &mut egui::Ui, address: &str, password: &str, control: &HostControl) -> bool {
    let mut turn_off = false;
    let peer = control.status.lock().unwrap().connected_peer.clone();

    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        ui.heading("rd-host");
        ui.add_space(20.0);

        let button = egui::Button::new(
            egui::RichText::new("STOP").size(16.0).strong().color(egui::Color32::from_rgb(90, 220, 120)),
        )
        .min_size(egui::vec2(84.0, 84.0))
        .corner_radius(42.0);
        if ui.add(button).on_hover_text("Stop hosting").clicked() {
            turn_off = true;
        }

        ui.add_space(14.0);
        match &peer {
            Some(p) => {
                ui.colored_label(egui::Color32::from_rgb(90, 220, 120), format!("Connected: {p}"));
            }
            None => {
                ui.label(egui::RichText::new("Waiting for a connection...").weak());
            }
        }

        ui.add_space(24.0);
        copy_row(ui, "LAN address", address);

        ui.add_space(10.0);
        match &*control.ngrok.lock().unwrap() {
            NgrokState::Starting => {
                ui.horizontal(|ui| {
                    ui.add_sized([72.0, 20.0], egui::Label::new("Tunnel"));
                    ui.label(egui::RichText::new("starting ngrok tunnel...").weak());
                });
            }
            NgrokState::Ready(tunnel_addr) => copy_row(ui, "Tunnel", tunnel_addr),
            NgrokState::Unavailable(reason) => {
                ui.horizontal(|ui| {
                    ui.add_sized([72.0, 20.0], egui::Label::new("Tunnel"));
                    ui.label(egui::RichText::new(reason).weak().small());
                });
            }
        }

        ui.add_space(10.0);
        copy_row(ui, "Password", password);

        ui.add_space(20.0);
        ui.label(
            egui::RichText::new(
                "LAN address only works on this network. The tunnel address\nworks from anywhere, once ngrok finishes starting.",
            )
            .weak()
            .small(),
        );
    });

    turn_off
}

/// One labeled, monospaced, copyable value - used for both Address and Password.
fn copy_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized([72.0, 20.0], egui::Label::new(label));
        ui.add(
            egui::Label::new(egui::RichText::new(value).monospace())
                .selectable(true),
        );
        if ui.button("Copy").on_hover_text(format!("Copy {label}")).clicked() {
            ui.ctx().copy_text(value.to_string());
        }
    });
}
