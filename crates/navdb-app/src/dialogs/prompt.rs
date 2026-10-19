//! Single-line text input window (rename, new group, save query...).

use super::Dialog;
use crate::state::AppState;

pub type OnOk = Box<dyn FnOnce(&mut AppState, String)>;

pub struct PromptDialog {
    id: String,
    title: String,
    label: String,
    value: String,
    on_ok: Option<OnOk>,
    focused: bool,
}

impl PromptDialog {
    pub fn new(title: impl Into<String>, label: impl Into<String>, initial: String, on_ok: OnOk) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), title: title.into(), label: label.into(), value: initial, on_ok: Some(on_ok), focused: false }
    }
}

impl Dialog for PromptDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut done = false;
        super::window(self.title.clone(), &self.id).open(&mut open).resizable(false).default_width(360.0).show(ctx, |ui| {
            ui.label(&self.label);
            let r = ui.add(egui::TextEdit::singleline(&mut self.value).desired_width(320.0).min_size(egui::vec2(320.0, 0.0)));
            if !self.focused {
                r.request_focus();
                self.focused = true;
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(egui::RichText::new("  OK  ").strong()).clicked() || enter {
                        if let Some(f) = self.on_ok.take() {
                            f(st, self.value.trim().to_string());
                        }
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            });
        });
        open && !done
    }
}
