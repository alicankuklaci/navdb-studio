//! Yes/No confirmation window.

use super::Dialog;
use crate::state::AppState;

pub type OnConfirm = Box<dyn FnOnce(&mut AppState)>;

pub struct ConfirmDialog {
    id: String,
    title: String,
    message: String,
    on_yes: Option<OnConfirm>,
}

impl ConfirmDialog {
    pub fn new(title: impl Into<String>, message: impl Into<String>, on_yes: OnConfirm) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), title: title.into(), message: message.into(), on_yes: Some(on_yes) }
    }
}

impl Dialog for ConfirmDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut done = false;
        super::window(self.title.clone(), &self.id).open(&mut open).resizable(false).default_width(380.0).show(ctx, |ui| {
            ui.add(egui::Label::new(&self.message).wrap());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let yes = ui.button(egui::RichText::new("  Yes  ").strong());
                    if yes.clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Some(f) = self.on_yes.take() {
                            f(st);
                        }
                        done = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        done = true;
                    }
                });
            });
        });
        open && !done
    }
}
