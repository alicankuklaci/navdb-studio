use egui::{Color32, Visuals};
use navdb_core::DbKind;

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

pub fn kind_icon(k: DbKind) -> &'static str {
    match k {
        DbKind::MySql => "🐬",
        DbKind::MariaDb => "🦭",
        DbKind::PostgreSql => "🐘",
        DbKind::Sqlite => "🪶",
        DbKind::Redis => "🟥",
    }
}

pub const NULL_COLOR: Color32 = Color32::from_rgb(150, 150, 150);
pub const MODIFIED_BG: Color32 = Color32::from_rgba_premultiplied(255, 210, 80, 60);
pub const NEW_BG: Color32 = Color32::from_rgba_premultiplied(80, 200, 120, 50);
pub const DELETED_BG: Color32 = Color32::from_rgba_premultiplied(230, 80, 80, 60);

pub mod icon {
    pub const CONNECTION: &str = "🔌";
    pub const QUERY: &str = "📝";
    pub const NEW_QUERY: &str = "🗒";
    pub const TABLE: &str = "▦";
    pub const VIEW: &str = "◫";
    pub const FUNCTION: &str = "ƒ";
    pub const EVENT: &str = "⏱";
    pub const USER: &str = "👤";
    pub const OTHERS: &str = "⋯";
    pub const BACKUP: &str = "💾";
    pub const AUTOMATION: &str = "⚙";
    pub const MODEL: &str = "🗺";
    pub const CHARTS: &str = "📊";
    pub const DATABASE: &str = "🛢";
    pub const SCHEMA: &str = "🗂";
    pub const REFRESH: &str = "⟳";
    pub const ADD: &str = "＋";
    pub const DELETE: &str = "✖";
    pub const SAVE: &str = "💾";
    pub const RUN: &str = "▶";
    pub const STOP: &str = "⏹";
    pub const EXPLAIN: &str = "🔍";
    pub const FILTER: &str = "⏷";
    pub const SORT: &str = "⇅";
    pub const IMPORT: &str = "⤓";
    pub const EXPORT: &str = "⤒";
    pub const DESIGN: &str = "✎";
    pub const MONITOR: &str = "📈";
    pub const CONSOLE: &str = "⌨";
    pub const HISTORY: &str = "🕘";
    pub const TOOLS: &str = "🛠";
    pub const SYNC: &str = "⇄";
    pub const KEY: &str = "🔑";
}
