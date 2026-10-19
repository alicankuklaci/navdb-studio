use egui::{Color32, Visuals};
use egui_phosphor::regular as ph;
use navdb_core::{DbKind, ObjectKind};

pub fn apply(ctx: &egui::Context, dark: bool) {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.selection.bg_fill = if dark { Color32::from_rgb(38, 79, 120) } else { Color32::from_rgb(200, 222, 248) };
    v.selection.stroke.color = if dark { Color32::from_rgb(140, 190, 255) } else { Color32::from_rgb(20, 90, 180) };
    v.striped = true;
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(6.0, 3.0);
        s.interaction.tooltip_delay = 0.4;
        // plain labels must not swallow row clicks / double clicks
        s.interaction.selectable_labels = false;
    });
}

pub fn kind_color(k: DbKind) -> Color32 {
    match k {
        DbKind::MySql => Color32::from_rgb(0, 117, 143),
        DbKind::MariaDb => Color32::from_rgb(192, 118, 90),
        DbKind::PostgreSql => Color32::from_rgb(51, 103, 145),
        DbKind::Sqlite => Color32::from_rgb(68, 138, 196),
        DbKind::Redis => Color32::from_rgb(214, 54, 41),
    }
}

/// Text glyph for a database kind (used inside plain-text labels).
pub fn kind_icon(_k: DbKind) -> &'static str {
    ph::DATABASE
}

/// Official product logo (Simple Icons, CC0) for a database kind.
pub fn kind_logo(k: DbKind, size: f32) -> egui::Image<'static> {
    let src = match k {
        DbKind::MySql => egui::include_image!("../../assets/logos/mysql.svg"),
        DbKind::MariaDb => egui::include_image!("../../assets/logos/mariadb.svg"),
        DbKind::PostgreSql => egui::include_image!("../../assets/logos/postgresql.svg"),
        DbKind::Sqlite => egui::include_image!("../../assets/logos/sqlite.svg"),
        DbKind::Redis => egui::include_image!("../../assets/logos/redis.svg"),
    };
    egui::Image::new(src).fit_to_exact_size(egui::vec2(size, size))
}

/// Logo followed by text, e.g. in the navigator and menus.
pub fn logo_label(ui: &mut egui::Ui, k: DbKind, size: f32) {
    ui.add(kind_logo(k, size));
}

pub fn object_icon(k: ObjectKind) -> &'static str {
    match k {
        ObjectKind::Table => ph::TABLE,
        ObjectKind::View => ph::EYE,
        ObjectKind::MaterializedView => ph::STACK,
        ObjectKind::Function => ph::FUNCTION,
        ObjectKind::Procedure => ph::GEAR_SIX,
        ObjectKind::Trigger => ph::LIGHTNING,
        ObjectKind::Event => ph::CLOCK,
        ObjectKind::Sequence => ph::LIST_NUMBERS,
        ObjectKind::Index => ph::LIST_MAGNIFYING_GLASS,
        ObjectKind::Key => ph::KEY,
    }
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

pub const NULL_COLOR: Color32 = Color32::from_rgb(150, 150, 150);
pub const MODIFIED_BG: Color32 = Color32::from_rgba_unmultiplied(230, 170, 30, 70);
pub const NEW_BG: Color32 = Color32::from_rgba_unmultiplied(60, 180, 100, 60);
pub const DELETED_BG: Color32 = Color32::from_rgba_unmultiplied(220, 60, 60, 70);

pub mod icon {
    pub const CONNECTION: &str = super::ph::PLUGS_CONNECTED;
    pub const QUERY: &str = super::ph::FILE_SQL;
    pub const NEW_QUERY: &str = super::ph::NOTE_PENCIL;
    pub const TABLE: &str = super::ph::TABLE;
    pub const VIEW: &str = super::ph::EYE;
    pub const FUNCTION: &str = super::ph::FUNCTION;
    pub const EVENT: &str = super::ph::CLOCK;
    pub const USER: &str = super::ph::USER;
    pub const OTHERS: &str = super::ph::DOTS_THREE_CIRCLE;
    pub const BACKUP: &str = super::ph::ARCHIVE;
    pub const AUTOMATION: &str = super::ph::ROBOT;
    pub const MODEL: &str = super::ph::TREE_STRUCTURE;
    pub const CHARTS: &str = super::ph::CHART_BAR;
    pub const DATABASE: &str = super::ph::DATABASE;
    pub const SCHEMA: &str = super::ph::FOLDER_SIMPLE;
    pub const REFRESH: &str = super::ph::ARROW_CLOCKWISE;
    pub const ADD: &str = super::ph::PLUS;
    pub const DELETE: &str = super::ph::TRASH;
    pub const SAVE: &str = super::ph::FLOPPY_DISK;
    pub const RUN: &str = super::ph::PLAY;
    pub const STOP: &str = super::ph::STOP;
    pub const EXPLAIN: &str = super::ph::MAGNIFYING_GLASS;
    pub const FILTER: &str = super::ph::FUNNEL;
    pub const SORT: &str = super::ph::SORT_ASCENDING;
    pub const IMPORT: &str = super::ph::DOWNLOAD_SIMPLE;
    pub const EXPORT: &str = super::ph::UPLOAD_SIMPLE;
    pub const DESIGN: &str = super::ph::PENCIL_SIMPLE;
    pub const MONITOR: &str = super::ph::PULSE;
    pub const CONSOLE: &str = super::ph::TERMINAL_WINDOW;
    pub const HISTORY: &str = super::ph::CLOCK_COUNTER_CLOCKWISE;
    pub const TOOLS: &str = super::ph::WRENCH;
    pub const SYNC: &str = super::ph::ARROWS_LEFT_RIGHT;
    pub const KEY: &str = super::ph::KEY;
}
