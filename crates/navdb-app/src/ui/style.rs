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
    let (r, g, b) = match k {
        DbKind::MySql => (0, 117, 143),
        DbKind::MariaDb => (192, 118, 90),
        DbKind::PostgreSql => (51, 103, 145),
        DbKind::Sqlite => (68, 138, 196),
        DbKind::Redis => (214, 54, 41),
        DbKind::SqlServer => (204, 41, 39),
        DbKind::Oracle => (224, 30, 30),
        DbKind::MongoDb => (71, 162, 72),
        DbKind::DuckDb => (200, 168, 0),
        DbKind::Iceberg => (42, 111, 191),
        DbKind::Vertica => (27, 79, 156),
        DbKind::ClickHouse => (230, 172, 0),
        DbKind::Trino => (221, 0, 161),
        DbKind::Presto => (88, 144, 255),
        DbKind::OpenSearch => (0, 94, 184),
        DbKind::Elasticsearch => (0, 85, 113),
        DbKind::Cassandra => (18, 135, 177),
        DbKind::ScyllaDb => (63, 182, 204),
        DbKind::TiDb => (220, 21, 11),
        DbKind::SingleStore => (170, 0, 255),
        DbKind::OceanBase => (0, 106, 255),
        DbKind::StarRocks => (62, 100, 230),
        DbKind::Doris => (46, 167, 195),
        DbKind::CockroachDb => (105, 51, 255),
        DbKind::YugabyteDb => (255, 110, 66),
        DbKind::TimescaleDb => (240, 165, 0),
        DbKind::Redshift => (140, 79, 255),
        DbKind::Greenplum => (58, 161, 74),
        DbKind::Valkey => (95, 111, 232),
        DbKind::KeyDb => (215, 53, 76),
        DbKind::Dragonfly => (122, 77, 255),
    };
    Color32::from_rgb(r, g, b)
}

/// Text glyph for a database kind (used inside plain-text labels).
pub fn kind_icon(_k: DbKind) -> &'static str {
    ph::DATABASE
}

/// Product logo for a database kind, in its original colours (see assets/logos/SOURCES.md).
pub fn kind_logo(k: DbKind, size: f32) -> egui::Image<'static> {
    let src = match k {
        DbKind::MySql => egui::include_image!("../../assets/logos/mysql.svg"),
        DbKind::MariaDb => egui::include_image!("../../assets/logos/mariadb.svg"),
        DbKind::PostgreSql => egui::include_image!("../../assets/logos/postgresql.svg"),
        DbKind::Sqlite => egui::include_image!("../../assets/logos/sqlite.svg"),
        DbKind::Redis => egui::include_image!("../../assets/logos/redis.svg"),
        DbKind::SqlServer => egui::include_image!("../../assets/logos/sqlserver.svg"),
        DbKind::Oracle => egui::include_image!("../../assets/logos/oracle.svg"),
        DbKind::MongoDb => egui::include_image!("../../assets/logos/mongodb.svg"),
        DbKind::DuckDb => egui::include_image!("../../assets/logos/duckdb.svg"),
        DbKind::Iceberg => egui::include_image!("../../assets/logos/iceberg.svg"),
        DbKind::Vertica => egui::include_image!("../../assets/logos/vertica.svg"),
        DbKind::ClickHouse => egui::include_image!("../../assets/logos/clickhouse.svg"),
        DbKind::Trino => egui::include_image!("../../assets/logos/trino.svg"),
        DbKind::Presto => egui::include_image!("../../assets/logos/presto.svg"),
        DbKind::OpenSearch => egui::include_image!("../../assets/logos/opensearch.svg"),
        DbKind::Elasticsearch => egui::include_image!("../../assets/logos/elasticsearch.svg"),
        DbKind::Cassandra => egui::include_image!("../../assets/logos/apachecassandra.svg"),
        DbKind::ScyllaDb => egui::include_image!("../../assets/logos/scylladb.svg"),
        DbKind::TiDb => egui::include_image!("../../assets/logos/tidb.svg"),
        DbKind::SingleStore => egui::include_image!("../../assets/logos/singlestore.svg"),
        DbKind::OceanBase => egui::include_image!("../../assets/logos/oceanbase.svg"),
        DbKind::StarRocks => egui::include_image!("../../assets/logos/starrocks.svg"),
        DbKind::Doris => egui::include_image!("../../assets/logos/doris.svg"),
        DbKind::CockroachDb => egui::include_image!("../../assets/logos/cockroachlabs.svg"),
        DbKind::YugabyteDb => egui::include_image!("../../assets/logos/yugabytedb.svg"),
        DbKind::TimescaleDb => egui::include_image!("../../assets/logos/timescale.svg"),
        DbKind::Redshift => egui::include_image!("../../assets/logos/amazonredshift.svg"),
        DbKind::Greenplum => egui::include_image!("../../assets/logos/greenplum.svg"),
        DbKind::Valkey => egui::include_image!("../../assets/logos/valkey.svg"),
        DbKind::KeyDb => egui::include_image!("../../assets/logos/keydb.svg"),
        DbKind::Dragonfly => egui::include_image!("../../assets/logos/dragonfly.svg"),
    };
    egui::Image::new(src).fit_to_exact_size(egui::vec2(size, size))
}

/// "New Connection" picker: one submenu per group of database kinds.
pub fn kind_menu(ui: &mut egui::Ui, mut on_pick: impl FnMut(DbKind)) {
    for (group, kinds) in DbKind::GROUPS {
        ui.menu_button(group, |ui| {
            for &k in kinds {
                if ui.add(egui::Button::image_and_text(kind_logo(k, 16.0), format!("{}…", k.label()))).clicked() {
                    on_pick(k);
                    ui.close_menu();
                }
            }
        });
    }
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

const FILL_FAMILY: &str = "phosphor-fill";

/// Filled variant of the icon font (same code points as the outline icons).
pub fn fill_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(FILL_FAMILY.into()))
}

/// Accent colour of an icon glyph, so the same object reads the same everywhere.
pub fn icon_color(glyph: &str) -> Option<Color32> {
    let rgb = |r, g, b| Some(Color32::from_rgb(r, g, b));
    match glyph {
        ph::TABLE => rgb(0x3B, 0x82, 0xF6),
        ph::EYE => rgb(0x14, 0xA8, 0x9A),
        ph::STACK => rgb(0x0E, 0x9F, 0xB8),
        ph::FUNCTION => rgb(0xA2, 0x55, 0xF0),
        ph::GEAR_SIX => rgb(0xE0, 0x7A, 0x1F),
        ph::LIGHTNING => rgb(0xE8, 0xA3, 0x0C),
        ph::CLOCK => rgb(0xE0, 0x4E, 0x8A),
        ph::LIST_NUMBERS => rgb(0x5B, 0x7C, 0x99),
        ph::LIST_MAGNIFYING_GLASS => rgb(0x22, 0xA0, 0x5A),
        ph::KEY => rgb(0xD9, 0x9A, 0x06),
        ph::DATABASE => rgb(0xE0, 0x9A, 0x2E),
        ph::FOLDER_SIMPLE => rgb(0xE8, 0xB0, 0x3A),
        ph::PLUGS_CONNECTED => rgb(0x10, 0xA3, 0x7F),
        ph::NOTE_PENCIL | ph::FILE_SQL => rgb(0x4F, 0x6B, 0xED),
        ph::USER => rgb(0xF0, 0x8C, 0x1A),
        ph::DOTS_THREE_CIRCLE => rgb(0x6B, 0x7A, 0x90),
        ph::ARCHIVE => rgb(0xD9, 0x6A, 0x2B),
        ph::ROBOT => rgb(0x0E, 0x9A, 0xD8),
        ph::TREE_STRUCTURE => rgb(0x7C, 0x5C, 0xF0),
        ph::CHART_BAR => rgb(0x16, 0xA3, 0x4A),
        ph::PULSE => rgb(0xE5, 0x48, 0x4D),
        ph::TERMINAL_WINDOW => rgb(0x47, 0x55, 0x69),
        ph::CLOCK_COUNTER_CLOCKWISE => rgb(0x64, 0x74, 0x8B),
        ph::PENCIL_SIMPLE => rgb(0x25, 0x8E, 0xD8),
        _ => None,
    }
}

/// Two-tone icon: a tinted filled shape under the outline.
pub fn paint_duotone(painter: &egui::Painter, center: egui::Pos2, glyph: &str, size: f32, color: Color32) {
    painter.text(center, egui::Align2::CENTER_CENTER, glyph, fill_font(size), color.gamma_multiply(0.22));
    painter.text(center, egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(size), color);
}

/// Label whose leading icon glyph is drawn filled in its accent colour.
pub fn icon_text(ui: &egui::Ui, text: &str, size: Option<f32>) -> egui::text::LayoutJob {
    let body = ui.style().text_styles.get(&egui::TextStyle::Body).cloned().unwrap_or_default();
    let font = egui::FontId::new(size.unwrap_or(body.size), body.family);
    let mut job = egui::text::LayoutJob::default();
    let plain = |font: egui::FontId| egui::TextFormat { font_id: font, color: Color32::PLACEHOLDER, valign: egui::Align::Center, ..Default::default() };
    let first = text.chars().next().filter(|c| ('\u{E000}'..='\u{F8FF}').contains(c));
    match first {
        Some(c) => {
            let glyph = c.to_string();
            let color = icon_color(&glyph).unwrap_or(ui.visuals().text_color());
            let f = egui::TextFormat { font_id: fill_font(font.size), color, valign: egui::Align::Center, ..Default::default() };
            job.append(&glyph, 0.0, f);
            job.append(&text[c.len_utf8()..], 0.0, plain(font));
        }
        None => job.append(text, 0.0, plain(font)),
    }
    job
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    fonts.font_data.insert(FILL_FAMILY.into(), egui_phosphor::Variant::Fill.font_data().into());
    fonts.families.insert(egui::FontFamily::Name(FILL_FAMILY.into()), vec![FILL_FAMILY.into(), "Ubuntu-Light".into()]);
    // Hack covers arrows/symbols (→ ↵ ⇶) that Ubuntu lacks
    if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        list.push("Hack".to_owned());
    }
    ctx.set_fonts(fonts);
}

pub const NULL_COLOR: Color32 = Color32::from_rgb(150, 150, 150);
pub const MODIFIED_BG: Color32 = Color32::from_rgba_premultiplied(63, 47, 8, 70);
pub const NEW_BG: Color32 = Color32::from_rgba_premultiplied(14, 42, 24, 60);
pub const DELETED_BG: Color32 = Color32::from_rgba_premultiplied(60, 16, 16, 70);

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
