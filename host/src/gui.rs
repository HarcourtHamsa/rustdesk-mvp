//! The visible-window GUI for `rd-host`: displays a 6-character connection
//! code when opened. The app automatically starts hosting in the background
//! immediately. The user can copy the code to share with the remote side.
//!
//! This is the default mode - running `rd-host` with no arguments opens this
//! window. Use `rd-host --tray` for tray-only mode instead. Other modes
//! (`listen`/`relay`/`with-relay`) still work exactly as before too, for
//! scripted or headless use (e.g. running `rd-relay`/`rd-host relay` on a
//! VPS with no display).

use crate::gui_core::{load_logo_rgba, start_hosting, HostControl, NgrokState};
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
                Ok((_, _, control)) => {
                    self.control = Some(control);
                    self.error = None;
                    // Email will be sent by gui_core.rs after ngrok tunnel is ready
                }
                Err(e) => {
                    self.error = Some(e.to_string());
                }
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            show_code(ui, &self.code, &self.error, self.control.as_ref());
        });

        // Cheap to redraw a few times a second - keeps UI responsive
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

/// Display the 6-character connection code and tunnel status.
fn show_code(ui: &mut egui::Ui, code: &str, error: &Option<String>, control: Option<&HostControl>) {
    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        ui.heading("Connection Code");
        ui.add_space(20.0);

        // Format code as "xxx-xxx" for display (but copy without dash)
        let formatted_code = if code.len() == 6 {
            format!("{}-{}", &code[0..3], &code[3..6])
        } else {
            code.to_string()
        };

        // Display the formatted code in a large, centered monospace font
        ui.label(
            egui::RichText::new(&formatted_code)
                .size(CODE_FONT_SIZE)
                .monospace()
                .color(egui::Color32::from_rgb(100, 200, 255)),
        );

        ui.add_space(16.0);

        // Copy button for the code (copies without dash)
        if ui.button("📋 Copy Code").on_hover_text("Copy the code to clipboard").clicked() {
            ui.ctx().copy_text(code.to_string());
        }

        ui.add_space(16.0);

        // Display tunnel status
        if let Some(ctrl) = control {
            match &*ctrl.ngrok.lock().unwrap() {
                NgrokState::Starting => {
                    ui.label(egui::RichText::new("🔄 Starting ngrok tunnel...").weak().small());
                }
                NgrokState::Ready(addr) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(90, 220, 120),
                        format!("✓ Tunnel ready: {}", addr),
                    );
                }
                NgrokState::Unavailable(reason) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("✗ Tunnel failed: {}", reason));
                }
            }
        }

        ui.add_space(8.0);
        ui.label(egui::RichText::new("Email will be sent once tunnel is ready").weak().small());

        // Show any hosting errors
        if let Some(e) = error {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::LIGHT_RED, format!("Error: {e}"));
        }
    });
}

