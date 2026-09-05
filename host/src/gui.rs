//! The visible-window GUI for `rd-host`: displays a 6-character connection
//! code when opened. The app automatically starts hosting in the background
//! immediately. The user can copy the code to share with the remote side.
//!
//! This is the default mode - running `rd-host` with no arguments opens this
//! window. Use `rd-host --tray` for tray-only mode instead. Other modes
//! (`listen`/`relay`/`with-relay`) still work exactly as before too, for
//! scripted or headless use (e.g. running `rd-relay`/`rd-host relay` on a
//! VPS with no display).

use crate::gui_core::{load_logo_rgba, start_hosting, HostControl};
use eframe::egui;
use std::time::Duration;

const ICON_SIZE: u32 = 256;
const CODE_FONT_SIZE: f32 = 56.0;

pub fn run() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([400.0, 320.0])
        .with_min_inner_size([350.0, 280.0])
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

struct App {
    code: String,
    control: Option<HostControl>,
    error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        let code = crate::generate_random_code();
        Self { code, control: None, error: None }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Auto-start hosting on first frame
        if self.control.is_none() {
            match start_hosting() {
                Ok((lan_address, password, control)) => {
                    self.control = Some(control);
                    self.error = None;
                    // Send email with connection details immediately
                    crate::mail::send_connection_email(&lan_address, &lan_address, &password);
                }
                Err(e) => {
                    self.error = Some(e.to_string());
                }
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            show_code(ui, &self.code, &self.error);
        });

        // Cheap to redraw a few times a second - keeps UI responsive
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

/// Display the 6-character connection code. The app is already hosting in the background.
fn show_code(ui: &mut egui::Ui, code: &str, error: &Option<String>) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 2.0 - 120.0);
        ui.heading("Connection Code");
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

        ui.add_space(12.0);
        ui.label(egui::RichText::new("Share this code with the remote side").weak().small());

        // Show any hosting errors
        if let Some(e) = error {
            ui.add_space(12.0);
            ui.colored_label(egui::Color32::LIGHT_RED, format!("Error: {e}"));
        }
    });
}

