//! Main window: menu bar, toolbar, navigator, tabs, information pane, status bar.

use std::sync::Arc;

use egui::{RichText, Ui};
use navdb_core::config;
use navdb_core::{DbKind, ObjectKind};

use crate::commands as cmd;
use crate::dialogs::Dialog;
use crate::state::{Action, AppState, OpenConn, Toast};
use crate::tabs::{objects::ObjectsTab, Tab};
use crate::ui::{self, info_pane::InfoPane, style::icon, tree};

pub struct NavApp {
    st: AppState,
    tabs: Vec<Box<dyn Tab>>,
    active: usize,
    dialogs: Vec<Box<dyn Dialog>>,
    tree: tree::TreeState,
    info: InfoPane,
    show_nav: bool,
    applied_dark: Option<bool>,
    pending_close: Option<usize>,
}

impl NavApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let notify: navdb_core::session::Notify = Arc::new(move || ctx.request_repaint());
        let st = AppState::new(notify);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        Self {
            st,
            tabs: vec![Box::new(ObjectsTab::new())],
            active: 0,
            dialogs: Vec::new(),
            tree: tree::TreeState::default(),
            info: InfoPane::default(),
            show_nav: true,
            applied_dark: None,
            pending_close: None,
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.st.actions);
        for a in actions {
            match a {
                Action::OpenTab(t) => {
                    let key = t.key();
                    if let Some(i) = self.tabs.iter().position(|x| x.key() == key) {
                        self.active = i;
                    } else {
                        self.tabs.push(t);
                        self.active = self.tabs.len() - 1;
                    }
                }
                Action::OpenDialog(d) => self.dialogs.push(d),
                Action::OpenConnection(id) => {
                    if !self.st.conns.contains_key(&id) {
                        if let Some(cfg) = self.st.config(&id).cloned() {
                            self.st.conns.insert(id.clone(), OpenConn::new(cfg, self.st.notify.clone()));
                        }
                    }
                }
                Action::CloseConnection(id) => {
                    self.st.conns.remove(&id);
                    if self.st.selection.conn.as_deref() == Some(&id) {
                        self.st.selection.scope = None;
                        self.st.selection.kind = None;
                        self.st.selection.object = None;
                    }
                }
                Action::RefreshConnection(id) => {
                    if let Some(c) = self.st.conns.get_mut(&id) {
                        c.refresh_all();
                    }
                }
                Action::RefreshScope { conn, scope } => {
                    if let Some(c) = self.st.conns.get_mut(&conn) {
                        c.refresh_scope(&scope);
                        let kinds = c.object_kinds();
                        for k in kinds {
                            c.load_objects(&scope, k);
                        }
                    }
                }
                Action::Select(s) => self.st.selection = s,
                Action::SaveWorkspace => {
                    if let Err(e) = self.st.ws.save() {
                        self.st.toasts.push(Toast { text: format!("Saving settings failed: {:#}", e), error: true, until: self.st.time + 6.0 });
                    }
                }
                Action::Toast { text, error } => {
                    self.st.toasts.push(Toast { text, error, until: self.st.time + if error { 8.0 } else { 3.5 } });
                }
                Action::History(e) => {
                    config::append_history(&e);
                    self.st.history.push(e);
                    let max = self.st.ws.preferences.max_history.max(10);
                    if self.st.history.len() > max {
                        let n = self.st.history.len() - max;
                        self.st.history.drain(0..n);
                    }
                }
                Action::CloseTabByKey(k) => {
                    if let Some(i) = self.tabs.iter().position(|t| t.key() == k) {
                        if i != 0 {
                            self.tabs.remove(i);
                            if self.active >= self.tabs.len() {
                                self.active = self.tabs.len() - 1;
                            }
                        }
                    }
                }
            }
        }
        if !self.st.actions.is_empty() {
            ctx.request_repaint();
        }
    }

    fn poll_tasks(&mut self) {
        let mut done = Vec::new();
        for (i, t) in self.st.tasks.iter_mut().enumerate() {
            if let Some(r) = t.pending.poll() {
                done.push((i, r));
            }
        }
        for (i, r) in done.into_iter().rev() {
            let t = self.st.tasks.remove(i);
            let elapsed = t.started.elapsed().as_millis();
            match r {
                Ok(_) => {
                    self.st.record(&t.conn, t.scope.as_ref(), &t.sql, true, &t.success, elapsed);
                    self.st.toast(t.success.clone());
                    if t.refresh {
                        if let Some(s) = &t.scope {
                            self.st.refresh_scope(&t.conn, s);
                        }
                    }
                    if t.refresh_databases {
                        self.st.push(Action::RefreshConnection(t.conn.clone()));
                    }
                }
                Err(e) => {
                    let msg = format!("{:#}", e);
                    self.st.record(&t.conn, t.scope.as_ref(), &t.sql, false, &msg, elapsed);
                    self.st.error(msg);
                }
            }
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                ui.menu_button("New Connection", |ui| {
                    for k in DbKind::ALL {
                        if ui.button(format!("{} {}…", ui::style::kind_icon(k), k.label())).clicked() {
                            cmd::new_connection(&mut self.st, k);
                            ui.close_menu();
                        }
                    }
                });
                if ui.button("New Query\t⌘T").clicked() {
                    let sel = self.st.selection.clone();
                    cmd::new_query(&mut self.st, sel.conn, sel.scope);
                    ui.close_menu();
                }
                if ui.button("Open SQL File…").clicked() {
                    if let Some(p) = ui::open_file_dialog(&["sql", "txt"]) {
                        match std::fs::read_to_string(&p) {
                            Ok(sql) => {
                                let sel = self.st.selection.clone();
                                cmd::new_query_with(&mut self.st, sel.conn, sel.scope, sql);
                            }
                            Err(e) => self.st.error(format!("{}", e)),
                        }
                    }
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Import Connections…").clicked() {
                    cmd::import_connections(&mut self.st);
                    ui.close_menu();
                }
                if ui.button("Export Connections…").clicked() {
                    cmd::export_connections(&mut self.st);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Preferences…\t⌘,").clicked() {
                    cmd::preferences(&mut self.st);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Close Tab\t⌘W").clicked() {
                    self.pending_close = Some(self.active);
                    ui.close_menu();
                }
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.show_nav, "Navigation Pane");
                if ui.checkbox(&mut self.st.ws.preferences.show_object_info_pane, "Information Pane").changed() {
                    self.st.save_workspace();
                }
                if ui.checkbox(&mut self.st.ws.preferences.dark_mode, "Dark Mode").changed() {
                    self.st.save_workspace();
                }
                ui.separator();
                if ui.button(format!("{} History Log", icon::HISTORY)).clicked() {
                    cmd::history(&mut self.st);
                    ui.close_menu();
                }
                if ui.button("Zoom In").clicked() {
                    ui.ctx().set_zoom_factor(ui.ctx().zoom_factor() + 0.1);
                }
                if ui.button("Zoom Out").clicked() {
                    ui.ctx().set_zoom_factor((ui.ctx().zoom_factor() - 0.1).max(0.5));
                }
                if ui.button("Actual Size").clicked() {
                    ui.ctx().set_zoom_factor(1.0);
                }
            });
            ui.menu_button("Tools", |ui| {
                let sel = self.st.selection.clone();
                let src = sel.conn.clone().zip(sel.scope.clone());
                if ui.button("Data Transfer…").clicked() {
                    cmd::data_transfer(&mut self.st, src.clone());
                    ui.close_menu();
                }
                if ui.button("Data Synchronization…").clicked() {
                    cmd::data_sync(&mut self.st, src.clone());
                    ui.close_menu();
                }
                if ui.button("Structure Synchronization…").clicked() {
                    cmd::structure_sync(&mut self.st, src.clone());
                    ui.close_menu();
                }
                let need = |st: &mut AppState| st.toast("Select a database in the navigator first");
                if ui.button("Data Generation…").clicked() {
                    match &src {
                        Some((c, s)) => cmd::data_generation(&mut self.st, c, s, vec![]),
                        None => need(&mut self.st),
                    }
                    ui.close_menu();
                }
                if ui.button("Find in Database…").clicked() {
                    match &src {
                        Some((c, s)) => cmd::find_in_database(&mut self.st, c, s),
                        None => need(&mut self.st),
                    }
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(format!("{} Server Monitor", icon::MONITOR)).clicked() {
                    match &sel.conn {
                        Some(c) => cmd::server_monitor(&mut self.st, c),
                        None => self.st.toast("Select a connection first"),
                    }
                    ui.close_menu();
                }
                if ui.button(format!("{} Console", icon::CONSOLE)).clicked() {
                    match &sel.conn {
                        Some(c) => cmd::console(&mut self.st, c),
                        None => self.st.toast("Select a connection first"),
                    }
                    ui.close_menu();
                }
                if ui.button(format!("{} Automation", icon::AUTOMATION)).clicked() {
                    cmd::automation(&mut self.st);
                    ui.close_menu();
                }
                if ui.button(format!("{} History Log", icon::HISTORY)).clicked() {
                    cmd::history(&mut self.st);
                    ui.close_menu();
                }
            });
            ui.menu_button("Window", |ui| {
                let titles: Vec<String> = self.tabs.iter().map(|t| t.title()).collect();
                for (i, t) in titles.iter().enumerate() {
                    if ui.selectable_label(i == self.active, t).clicked() {
                        self.active = i;
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.button("Close All Tabs").clicked() {
                    self.tabs.truncate(1);
                    self.active = 0;
                    ui.close_menu();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About NavDB Studio").clicked() {
                    cmd::about(&mut self.st);
                    ui.close_menu();
                }
                if ui.button("Open Data Folder").clicked() {
                    let _ = std::process::Command::new("open").arg(config::data_dir()).spawn();
                    ui.close_menu();
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let sel = self.st.selection.clone();
            let conn_resp = ui::toolbar_button(ui, icon::CONNECTION, "Connection", false);
            let popup_id = ui.make_persistent_id("conn-kind-popup");
            if conn_resp.clicked() {
                ui.memory_mut(|m| m.toggle_popup(popup_id));
            }
            egui::popup_below_widget(ui, popup_id, &conn_resp, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                ui.set_min_width(160.0);
                for k in DbKind::ALL {
                    if ui.button(format!("{} {}…", ui::style::kind_icon(k), k.label())).clicked() {
                        cmd::new_connection(&mut self.st, k);
                    }
                }
            });
            if ui::toolbar_button(ui, icon::NEW_QUERY, "New Query", false).clicked() {
                cmd::new_query(&mut self.st, sel.conn.clone(), sel.scope.clone());
            }
            ui.separator();
            let kinds: [(ObjectKind, &str, &str); 3] = [
                (ObjectKind::Table, icon::TABLE, "Table"),
                (ObjectKind::View, icon::VIEW, "View"),
                (ObjectKind::Function, icon::FUNCTION, "Function"),
            ];
            for (k, ic, label) in kinds {
                let on = self.active == 0 && sel.kind == Some(k);
                if ui::toolbar_button(ui, ic, label, on).clicked() {
                    self.show_kind(k);
                }
            }
            if ui::toolbar_button(ui, icon::EVENT, "Event", self.active == 0 && sel.kind == Some(ObjectKind::Event)).clicked() {
                self.show_kind(ObjectKind::Event);
            }
            if ui::toolbar_button(ui, icon::USER, "User", false).clicked() {
                match &sel.conn {
                    Some(c) => cmd::users(&mut self.st, c),
                    None => self.st.toast("Select a connection first"),
                }
            }
            let others = ui::toolbar_button(ui, icon::OTHERS, "Others", false);
            let others_id = ui.make_persistent_id("others-popup");
            if others.clicked() {
                ui.memory_mut(|m| m.toggle_popup(others_id));
            }
            egui::popup_below_widget(ui, others_id, &others, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                ui.set_min_width(170.0);
                for k in [ObjectKind::Procedure, ObjectKind::Trigger, ObjectKind::Sequence, ObjectKind::MaterializedView, ObjectKind::Index, ObjectKind::Key] {
                    if ui.button(format!("{} {}", k.icon(), k.label())).clicked() {
                        self.show_kind(k);
                    }
                }
            });
            if ui::toolbar_button(ui, icon::QUERY, "Query", false).clicked() {
                cmd::new_query(&mut self.st, sel.conn.clone(), sel.scope.clone());
            }
            if ui::toolbar_button(ui, icon::BACKUP, "Backup", false).clicked() {
                match (&sel.conn, &sel.scope) {
                    (Some(c), Some(s)) => cmd::backup(&mut self.st, c, s),
                    _ => self.st.toast("Select a database first"),
                }
            }
            if ui::toolbar_button(ui, icon::AUTOMATION, "Automation", false).clicked() {
                cmd::automation(&mut self.st);
            }
            if ui::toolbar_button(ui, icon::MODEL, "Model", false).clicked() {
                match (&sel.conn, &sel.scope) {
                    (Some(c), Some(s)) => cmd::er_model(&mut self.st, c, s),
                    _ => self.st.toast("Select a database first"),
                }
            }
            if ui::toolbar_button(ui, icon::CHARTS, "Charts", false).clicked() {
                let src = sel.conn.clone().zip(sel.scope.clone()).map(|(c, s)| (c, s, String::new()));
                cmd::charts(&mut self.st, src);
            }
            if ui::toolbar_button(ui, icon::MONITOR, "Monitor", false).clicked() {
                match &sel.conn {
                    Some(c) => cmd::server_monitor(&mut self.st, c),
                    None => self.st.toast("Select a connection first"),
                }
            }
        });
    }

    fn show_kind(&mut self, k: ObjectKind) {
        self.active = 0;
        self.st.selection.kind = Some(k);
        self.st.selection.object = None;
        if self.st.selection.scope.is_none() {
            // pick the first open connection's default database
            if let Some(cid) = self.st.selection.conn.clone() {
                if let Some(oc) = self.st.conn(&cid) {
                    let def = oc.cfg.default_database();
                    let dbs = oc.visible_databases();
                    let db = if dbs.contains(&def) { Some(def) } else { dbs.first().cloned() };
                    if let Some(db) = db {
                        let schema = if oc.cfg.kind.has_schemas() { Some("public".to_string()) } else { None };
                        self.st.selection.scope = Some(navdb_core::Scope::new(db, schema));
                    }
                }
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        egui::ScrollArea::horizontal().id_salt("tabbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut close = None;
                for (i, t) in self.tabs.iter().enumerate() {
                    let title = if t.dirty() { format!("● {}", t.title()) } else { t.title() };
                    let selected = i == self.active;
                    let frame = egui::Frame::new()
                        .fill(if selected { ui.visuals().selection.bg_fill } else { ui.visuals().faint_bg_color })
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::symmetric(8, 3));
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let r = ui.add(egui::Label::new(RichText::new(title).size(12.5)).sense(egui::Sense::click()).selectable(false));
                            if r.clicked() {
                                self.active = i;
                            }
                            if r.middle_clicked() && i != 0 {
                                close = Some(i);
                            }
                            r.context_menu(|ui| {
                                if i != 0 && ui.button("Close").clicked() {
                                    close = Some(i);
                                    ui.close_menu();
                                }
                                if ui.button("Close Others").clicked() {
                                    self.pending_close = None;
                                    close = Some(usize::MAX - i);
                                    ui.close_menu();
                                }
                            });
                            if i != 0 && ui.add(egui::Label::new(RichText::new("✕").size(11.0).weak()).sense(egui::Sense::click())).clicked() {
                                close = Some(i);
                            }
                        });
                    });
                }
                if let Some(c) = close {
                    if c > self.tabs.len() {
                        // close others
                        let keep = usize::MAX - c;
                        let mut idx = 0usize;
                        self.tabs.retain(|_| {
                            let k = idx == 0 || idx == keep;
                            idx += 1;
                            k
                        });
                        self.active = if keep == 0 { 0 } else { 1 };
                    } else {
                        self.pending_close = Some(c);
                    }
                }
            });
        });
    }

    fn try_close(&mut self, i: usize) {
        if i == 0 || i >= self.tabs.len() {
            return;
        }
        if self.tabs[i].can_close(&mut self.st) {
            self.tabs.remove(i);
            if self.active >= i && self.active > 0 {
                self.active -= 1;
            }
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let sel = &self.st.selection;
            let mut parts = Vec::new();
            if let Some(c) = &sel.conn {
                parts.push(self.st.conn_name(c));
            }
            if let Some(s) = &sel.scope {
                parts.push(s.display());
            }
            if let Some(o) = &sel.object {
                parts.push(o.clone());
            }
            ui.label(RichText::new(parts.join("  ›  ")).small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.st.tasks.len() + self.st.conns.values().filter(|c| c.session.is_busy()).count();
                if busy > 0 {
                    ui.spinner();
                    ui.label(RichText::new(format!("{} running", busy)).small());
                }
                ui.label(RichText::new(format!("{} connection(s) open", self.st.conns.len())).small().weak());
            });
        });
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        let now = self.st.time;
        self.st.toasts.retain(|t| t.until > now);
        if self.st.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -36.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let mut dismiss = None;
                for (i, t) in self.st.toasts.iter().enumerate() {
                    let fill = if t.error { egui::Color32::from_rgb(170, 40, 40) } else { egui::Color32::from_rgb(40, 110, 60) };
                    let r = egui::Frame::popup(ui.style()).fill(fill).show(ui, |ui| {
                        ui.set_max_width(420.0);
                        ui.add(egui::Label::new(RichText::new(&t.text).color(egui::Color32::WHITE)).wrap());
                    });
                    if r.response.interact(egui::Sense::click()).clicked() {
                        dismiss = Some(i);
                    }
                }
                if let Some(i) = dismiss {
                    self.st.toasts.remove(i);
                }
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        let (t, w, comma, n) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::T),
                i.consume_key(Modifiers::COMMAND, Key::W),
                i.consume_key(Modifiers::COMMAND, Key::Comma),
                i.consume_key(Modifiers::COMMAND, Key::N),
            )
        });
        if t {
            let sel = self.st.selection.clone();
            cmd::new_query(&mut self.st, sel.conn, sel.scope);
        }
        if w {
            self.pending_close = Some(self.active);
        }
        if comma {
            cmd::preferences(&mut self.st);
        }
        if n {
            cmd::new_connection(&mut self.st, DbKind::MySql);
        }
    }
}

impl eframe::App for NavApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.st.time = ctx.input(|i| i.time);
        let dark = self.st.ws.preferences.dark_mode;
        if self.applied_dark != Some(dark) {
            ui::style::apply(ctx, dark);
            self.applied_dark = Some(dark);
        }
        for c in self.st.conns.values_mut() {
            c.poll();
        }
        self.poll_tasks();
        self.shortcuts(ctx);

        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_bar(ui));
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        if self.show_nav {
            egui::SidePanel::left("navigator").resizable(true).default_width(260.0).width_range(160.0..=600.0).show(ctx, |ui| {
                tree::show(ui, &mut self.st, &mut self.tree);
            });
        }
        if self.st.ws.preferences.show_object_info_pane {
            egui::SidePanel::right("info").resizable(true).default_width(260.0).width_range(180.0..=600.0).show(ctx, |ui| {
                self.info.ui(ui, &mut self.st);
            });
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            self.tab_bar(ui);
            ui.separator();
            if self.active >= self.tabs.len() {
                self.active = 0;
            }
            for (i, t) in self.tabs.iter_mut().enumerate() {
                if i != self.active {
                    t.tick(&mut self.st);
                }
            }
            let tab = &mut self.tabs[self.active];
            tab.ui(ui, &mut self.st);
        });

        let mut dialogs = std::mem::take(&mut self.dialogs);
        dialogs.retain_mut(|d| d.show(ctx, &mut self.st));
        dialogs.append(&mut self.dialogs);
        self.dialogs = dialogs;

        if let Some(i) = self.pending_close.take() {
            self.try_close(i);
        }
        self.apply_actions(ctx);
        self.toasts(ctx);
        // keep polling background work smoothly
        if !self.st.tasks.is_empty() || self.st.conns.values().any(|c| c.session.is_busy()) {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.st.ws.save();
    }
}
