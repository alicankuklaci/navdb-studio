//! New / Edit Connection form (General, Advanced, Databases, SSL, SSH).

use egui::RichText;
use navdb_core::{driver, ConnectionConfig, DbKind, Pending};

use super::Dialog;
use crate::state::{Action, AppState};
use crate::ui;

#[derive(PartialEq, Clone, Copy)]
enum Page {
    General,
    Advanced,
    Databases,
    Ssl,
    Ssh,
}

pub struct ConnectionDialog {
    cfg: ConnectionConfig,
    is_new: bool,
    page: Page,
    test: Option<Pending<String>>,
    test_result: Option<Result<String, String>>,
    db_list: Option<Pending<Vec<String>>>,
    available_dbs: Vec<String>,
    port_text: String,
    error: Option<String>,
}

impl ConnectionDialog {
    pub fn new(cfg: ConnectionConfig, is_new: bool) -> Self {
        let port_text = cfg.port.to_string();
        Self {
            cfg,
            is_new,
            page: Page::General,
            test: None,
            test_result: None,
            db_list: None,
            available_dbs: vec![],
            port_text,
            error: None,
        }
    }

    fn run_blocking<T: Send + 'static>(cfg: ConnectionConfig, f: impl FnOnce(ConnectionConfig) -> anyhow::Result<T> + Send + 'static, ctx: egui::Context) -> Pending<T> {
        navdb_core::session::spawn_task(std::sync::Arc::new(move || ctx.request_repaint()), move || f(cfg))
    }

    fn general(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("conn-general").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Connection Name:");
            let hint = self.cfg.display_name();
            ui.add(egui::TextEdit::singleline(&mut self.cfg.name).hint_text(hint).desired_width(320.0).min_size(egui::vec2(320.0, 0.0)));
            ui.end_row();
            ui.label("Type:");
            egui::ComboBox::from_id_salt("conn-kind").selected_text(self.cfg.kind.label()).show_ui(ui, |ui| {
                for k in DbKind::ALL {
                    if ui.selectable_label(self.cfg.kind == k, k.label()).clicked() && self.cfg.kind != k {
                        self.cfg.kind = k;
                        self.cfg.port = k.default_port();
                        self.port_text = self.cfg.port.to_string();
                        if self.cfg.user.is_empty() || self.cfg.user == "root" || self.cfg.user == "postgres" {
                            self.cfg.user = k.default_user().into();
                        }
                        if !k.is_file_based() && self.cfg.host.is_empty() {
                            self.cfg.host = "localhost".into();
                        }
                    }
                }
            });
            ui.end_row();
            if self.cfg.kind.is_file_based() {
                ui.label("Database File:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.database).desired_width(260.0).min_size(egui::vec2(260.0, 0.0)).hint_text("/path/to/file.sqlite"));
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("SQLite", &["sqlite", "sqlite3", "db", "db3", "s3db"]).pick_file() {
                            self.cfg.database = p.display().to_string();
                        }
                    }
                    if ui.button("New…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_file_name("database.sqlite").save_file() {
                            self.cfg.database = p.display().to_string();
                        }
                    }
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.cfg.read_only, "Open read-only");
                ui.end_row();
            } else {
                ui.label("Host:");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.host).desired_width(320.0).min_size(egui::vec2(320.0, 0.0)));
                ui.end_row();
                ui.label("Port:");
                if ui.add(egui::TextEdit::singleline(&mut self.port_text).desired_width(80.0).min_size(egui::vec2(80.0, 0.0))).changed() {
                    if let Ok(p) = self.port_text.trim().parse() {
                        self.cfg.port = p;
                    }
                }
                ui.end_row();
                ui.label(if self.cfg.kind == DbKind::Redis { "Username (ACL):" } else { "User Name:" });
                ui.add(egui::TextEdit::singleline(&mut self.cfg.user).desired_width(320.0).min_size(egui::vec2(320.0, 0.0)));
                ui.end_row();
                ui.label("Password:");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.password).password(true).desired_width(320.0).min_size(egui::vec2(320.0, 0.0)));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.cfg.save_password, "Save password");
                ui.end_row();
                ui.label(match self.cfg.kind {
                    DbKind::PostgreSql => "Initial Database:",
                    DbKind::Redis => "Database (db0..):",
                    _ => "Default Database:",
                });
                ui.add(egui::TextEdit::singleline(&mut self.cfg.database).desired_width(320.0).min_size(egui::vec2(320.0, 0.0)).hint_text(match self.cfg.kind {
                    DbKind::PostgreSql => "postgres",
                    DbKind::Redis => "db0",
                    _ => "(optional)",
                }));
                ui.end_row();
            }
        });
    }

    fn advanced(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("conn-adv").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Connect timeout (s):");
            ui.add(egui::DragValue::new(&mut self.cfg.connect_timeout_secs).range(1..=300));
            ui.end_row();
            ui.label("Keepalive interval (s):");
            ui.add(egui::DragValue::new(&mut self.cfg.keepalive_secs).range(0..=3600));
            ui.end_row();
            if matches!(self.cfg.kind, DbKind::MySql | DbKind::MariaDb) {
                ui.label("Encoding:");
                egui::ComboBox::from_id_salt("enc").selected_text(&self.cfg.encoding).show_ui(ui, |ui| {
                    for e in ["utf8mb4", "utf8", "latin1", "ascii", "binary", "cp1251", "gbk"] {
                        ui.selectable_value(&mut self.cfg.encoding, e.to_string(), e);
                    }
                });
                ui.end_row();
            }
            ui.label("Group:");
            ui.add(egui::TextEdit::singleline(&mut self.cfg.group).desired_width(200.0).min_size(egui::vec2(200.0, 0.0)));
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut self.cfg.read_only, "Read-only (table editing disabled)");
            ui.end_row();
            ui.label("Color:");
            ui.horizontal(|ui| {
                let mut has = self.cfg.color.is_some();
                if ui.checkbox(&mut has, "").changed() {
                    self.cfg.color = if has { Some([50, 120, 220]) } else { None };
                }
                if let Some(c) = &mut self.cfg.color {
                    ui.color_edit_button_srgb(c);
                }
            });
            ui.end_row();
        });
        if self.cfg.kind == DbKind::Sqlite {
            ui.separator();
            ui.label(RichText::new("Attached databases").strong());
            let mut remove = None;
            for (i, (alias, path)) in self.cfg.attached.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(alias).desired_width(100.0).min_size(egui::vec2(100.0, 0.0)).hint_text("alias"));
                    ui.add(egui::TextEdit::singleline(path).desired_width(300.0).min_size(egui::vec2(300.0, 0.0)).hint_text("file"));
                    if ui.small_button("✖").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.cfg.attached.remove(i);
            }
            if ui.button("➕ Attach Database…").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    let alias = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "db2".into());
                    self.cfg.attached.push((alias, p.display().to_string()));
                }
            }
        }
    }

    fn databases(&mut self, ui: &mut egui::Ui) {
        ui.label("Show only selected databases in the navigator (none checked = show all).");
        ui.horizontal(|ui| {
            if ui.button("Load database list").clicked() {
                let cfg = self.cfg.clone();
                self.db_list = Some(Self::run_blocking(cfg, |c| driver::connect(&c)?.list_databases(), ui.ctx().clone()));
            }
            if self.db_list.is_some() {
                ui.spinner();
            }
            if ui.button("Clear").clicked() {
                self.cfg.custom_databases.clear();
            }
        });
        if let Some(p) = &mut self.db_list {
            if let Some(r) = p.poll() {
                self.db_list = None;
                match r {
                    Ok(v) => self.available_dbs = v,
                    Err(e) => self.error = Some(format!("{:#}", e)),
                }
            }
        }
        let mut all: Vec<String> = self.available_dbs.clone();
        for d in &self.cfg.custom_databases {
            if !all.contains(d) {
                all.push(d.clone());
            }
        }
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for d in all {
                let mut on = self.cfg.custom_databases.contains(&d);
                if ui.checkbox(&mut on, &d).changed() {
                    if on {
                        self.cfg.custom_databases.push(d.clone());
                    } else {
                        self.cfg.custom_databases.retain(|x| x != &d);
                    }
                }
            }
        });
    }

    fn ssl(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.cfg.ssl.enabled, "Use SSL / TLS");
        ui.add_enabled_ui(self.cfg.ssl.enabled, |ui| {
            egui::Grid::new("ssl").num_columns(3).spacing([10.0, 8.0]).show(ui, |ui| {
                for (label, field) in [
                    ("CA Certificate:", &mut self.cfg.ssl.ca_cert),
                    ("Client Certificate:", &mut self.cfg.ssl.client_cert),
                    ("Client Key:", &mut self.cfg.ssl.client_key),
                ] {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(field).desired_width(280.0).min_size(egui::vec2(280.0, 0.0)));
                    if ui.button("…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            *field = p.display().to_string();
                        }
                    }
                    ui.end_row();
                }
            });
            ui.checkbox(&mut self.cfg.ssl.verify_server, "Verify server certificate");
            ui.label(RichText::new("PostgreSQL uses PEM client cert + PKCS#8 key; MySQL uses the CA certificate.").small().weak());
        });
    }

    fn ssh(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.cfg.ssh.enabled, "Use SSH Tunnel");
        ui.add_enabled_ui(self.cfg.ssh.enabled, |ui| {
            egui::Grid::new("ssh").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("Host:");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.ssh.host).desired_width(280.0).min_size(egui::vec2(280.0, 0.0)));
                ui.end_row();
                ui.label("Port:");
                ui.add(egui::DragValue::new(&mut self.cfg.ssh.port).range(1..=65535));
                ui.end_row();
                ui.label("User Name:");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.ssh.user).desired_width(280.0).min_size(egui::vec2(280.0, 0.0)));
                ui.end_row();
                ui.label("Private Key:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.ssh.private_key).desired_width(220.0).min_size(egui::vec2(220.0, 0.0)).hint_text("~/.ssh/id_ed25519"));
                    if ui.button("…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            self.cfg.ssh.private_key = p.display().to_string();
                        }
                    }
                });
                ui.end_row();
            });
            ui.label(RichText::new("The tunnel uses your system OpenSSH client (key or ssh-agent authentication).").small().weak());
        });
    }
}

impl Dialog for ConnectionDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut close = false;
        let title = if self.is_new { format!("New Connection ({})", self.cfg.kind.label()) } else { format!("Edit Connection – {}", self.cfg.display_name()) };
        super::window(title, ("conn-dialog", &self.cfg.id)).open(&mut open).default_width(560.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.page, Page::General, "General");
                ui.selectable_value(&mut self.page, Page::Advanced, "Advanced");
                if self.cfg.kind.is_sql() && !self.cfg.kind.is_file_based() {
                    ui.selectable_value(&mut self.page, Page::Databases, "Databases");
                }
                if !self.cfg.kind.is_file_based() {
                    ui.selectable_value(&mut self.page, Page::Ssl, "SSL");
                    ui.selectable_value(&mut self.page, Page::Ssh, "SSH");
                }
            });
            ui.separator();
            ui.set_min_height(300.0);
            match self.page {
                Page::General => self.general(ui),
                Page::Advanced => self.advanced(ui),
                Page::Databases => self.databases(ui),
                Page::Ssl => self.ssl(ui),
                Page::Ssh => self.ssh(ui),
            }
            if let Some(p) = &mut self.test {
                if let Some(r) = p.poll() {
                    self.test = None;
                    self.test_result = Some(r.map_err(|e| format!("{:#}", e)));
                }
            }
            ui.separator();
            match &self.test_result {
                Some(Ok(v)) => {
                    ui.colored_label(egui::Color32::from_rgb(60, 160, 80), format!("✔ Connection successful – {}", v));
                }
                Some(Err(e)) => ui::error_text(ui, &format!("✖ {}", e)),
                None => {}
            }
            if let Some(e) = &self.error {
                ui::error_text(ui, e);
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(self.test.is_none(), egui::Button::new("Test Connection")).clicked() {
                    self.test_result = None;
                    let cfg = self.cfg.clone();
                    self.test = Some(Self::run_blocking(cfg, |c| driver::test_connection(&c), ctx.clone()));
                }
                if self.test.is_some() {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("  OK  ").strong()).clicked() {
                        if self.cfg.kind.is_file_based() && self.cfg.database.trim().is_empty() {
                            self.error = Some("Choose a database file".into());
                            return;
                        }
                        let cfg = self.cfg.clone();
                        if let Some(existing) = st.ws.connections.iter_mut().find(|c| c.id == cfg.id) {
                            let changed = *existing != cfg;
                            *existing = cfg.clone();
                            if changed {
                                // reconnect with new settings
                                st.push(Action::CloseConnection(cfg.id.clone()));
                            }
                        } else {
                            st.ws.connections.push(cfg.clone());
                        }
                        st.save_workspace();
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        });
        open && !close
    }
}
