//! The visible-window GUI for `rd-host`: a small window with a power
//! button. Click it to start listening for a connection (same as `rd-host
//! listen` under the hood, on the default port) - the window then shows
//! the address and password to give whoever's connecting, each with a copy
//! button. Click the power button again to stop.
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

pub fn run() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([360.0, 440.0])
        .with_min_inner_size([320.0, 400.0])
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
        Self { state: PowerState::Off, error: None }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default().show(ui, |ui| match &mut self.state {
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
