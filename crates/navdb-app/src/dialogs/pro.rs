//! NavDB Studio Pro subscription window (Mac App Store build).

use egui::RichText;

use super::Dialog;
use crate::state::AppState;
use crate::store;
use crate::ui::style::icon;

const FEATURES: &[&str] = &[
    "Unlimited saved connections",
    "Data Transfer between connections and engines",
    "Structure Sync and Data Sync",
    "ER model diagrams",
    "Charts and dashboards",
    "Automation: scheduled queries, backups and exports",
    "Import connections from other tools",
];

pub struct ProDialog {
    /// The feature that opened the window, shown as the reason.
    reason: Option<String>,
}

impl ProDialog {
    pub fn new(reason: Option<&str>) -> Self {
        Self { reason: reason.map(str::to_string) }
    }
}

impl Dialog for ProDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let s = store::status();
        // StoreKit answers asynchronously
        ctx.request_repaint_after(std::time::Duration::from_millis(400));
        let mut open = true;
        super::window("NavDB Studio Pro", "pro").open(&mut open).resizable(false).default_width(420.0).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                ui.heading(format!("{} NavDB Studio Pro", icon::STAR));
                if s.pro {
                    ui.label(RichText::new("Your subscription is active. Thank you!").strong());
                } else if let Some(r) = &self.reason {
                    ui.label(format!("{} is part of NavDB Studio Pro.", r));
                }
            });
            ui.add_space(8.0);
            for f in FEATURES {
                ui.label(format!("{}  {}", icon::CHECK, f));
            }
            ui.add_space(4.0);
            ui.label(RichText::new(format!("The free version includes up to {} saved connections with the full editor, data grid and designers.", store::FREE_CONNECTIONS)).small().weak());
            ui.add_space(10.0);

            if !s.pro {
                if s.products.is_empty() && s.message.is_empty() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading subscriptions…");
                    });
                }
                for p in &s.products {
                    ui.group(|ui| {
                        // Text and button are stacked, never side by side, so the
                        // wrapped plan text can't run under the button (App Review 4.0).
                        ui.set_width(ui.available_width());
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&p.name).strong());
                            ui.label(RichText::new(format!("{} per {}", p.price, p.period)).heading());
                            if !p.trial.is_empty() {
                                ui.label(format!("Free for {}, then {} per {}. Renews automatically until cancelled.", p.trial, p.price, p.period));
                            }
                            if !p.description.is_empty() {
                                ui.label(RichText::new(&p.description).small().weak());
                            }
                            ui.add_space(6.0);
                            let button = egui::Button::new(RichText::new(format!("Subscribe to {}", p.name)).strong())
                                .min_size(egui::vec2(ui.available_width(), 28.0));
                            if ui.add_enabled(!s.busy, button).clicked() {
                                store::purchase(&p.id);
                            }
                        });
                    });
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!s.busy, egui::Button::new("Restore Purchases")).clicked() {
                        store::restore();
                    }
                    if s.products.is_empty() && !s.message.is_empty() && ui.button("Try Again").clicked() {
                        store::reload();
                    }
                    if s.busy {
                        ui.spinner();
                    }
                });
            }
            ui.hyperlink_to("Manage subscription", store::MANAGE_URL);
            if !s.message.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(&s.message).color(ui.visuals().warn_fg_color));
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new("Payment is charged to your Apple Account. Subscriptions renew automatically unless cancelled at least 24 hours before the end of the current period; manage or cancel them in your App Store account settings.")
                    .small()
                    .weak(),
            );
            ui.horizontal(|ui| {
                ui.hyperlink_to(RichText::new("Terms of Use").small(), store::TERMS_URL);
                ui.label(RichText::new("·").small().weak());
                ui.hyperlink_to(RichText::new("Privacy Policy").small(), store::PRIVACY_URL);
            });
        });
        open
    }
}
