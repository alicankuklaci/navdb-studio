//! SQL editor widget: syntax highlighting, line numbers, auto-completion,
//! find/replace and comment toggling.

use egui::text::{CCursor, CCursorRange};
use egui::{Color32, Key, Modifiers, RichText, Ui};
use egui_phosphor::regular as ph;
use navdb_core::complete::{self, Completion, Item, ItemKind};
use navdb_core::sqlutil;

/// Produces completions for (text, cursor char index, explicitly requested).
pub type Completer<'a> = dyn FnMut(&str, usize, bool) -> Option<Completion> + 'a;

#[derive(Default)]
pub struct SqlEditor {
    pub text: String,
    popup: Option<Popup>,
    pub find_open: bool,
    pub find: String,
    pub replace: String,
    pub selection: Option<(usize, usize)>,
    pub cursor: usize,
    id: Option<egui::Id>,
    pending_cursor: Option<usize>,
    pending_select: Option<(usize, usize, egui::Id)>,
    pub font_size: f32,
    pub auto_complete: bool,
    pub uppercase_keywords: bool,
    pub language: String,
    /// Line-number gutter built from the last layout, so wrapped rows get no number.
    gutter: String,
    /// Text length last frame: completion opens on typing, not on deleting.
    last_len: usize,
}

struct Popup {
    start: usize,
    end: usize,
    items: Vec<Item>,
    selected: usize,
    pos: egui::Pos2,
    /// Cursor the list was computed for; moving away closes it.
    cursor: usize,
    /// Metadata was still loading: recompute until it's all there.
    incomplete: bool,
    explicit: bool,
    scroll: bool,
}

fn kind_icon(k: ItemKind) -> (&'static str, Color32) {
    match k {
        ItemKind::Keyword => (ph::TEXT_T, Color32::from_rgb(0x8a, 0x8f, 0x98)),
        ItemKind::Function => (ph::FUNCTION, Color32::from_rgb(0xb0, 0x7c, 0xe0)),
        ItemKind::Database => (ph::DATABASE, Color32::from_rgb(0xe0, 0xa0, 0x40)),
        ItemKind::Schema => (ph::FOLDER_SIMPLE, Color32::from_rgb(0xe0, 0xa0, 0x40)),
        ItemKind::Table => (ph::TABLE, Color32::from_rgb(0x4a, 0x9e, 0xe8)),
        ItemKind::View => (ph::EYE, Color32::from_rgb(0x4a, 0xb8, 0xa8)),
        ItemKind::Column => (ph::COLUMNS, Color32::from_rgb(0x5c, 0xb8, 0x5c)),
        ItemKind::Alias => (ph::AT, Color32::from_rgb(0x4a, 0x9e, 0xe8)),
        ItemKind::Snippet => (ph::CODE_SIMPLE, Color32::from_rgb(0xd0, 0x70, 0x70)),
        ItemKind::Word => (ph::TEXT_ALIGN_LEFT, Color32::from_rgb(0x8a, 0x8f, 0x98)),
    }
}

/// One number per logical line; soft-wrapped continuation rows stay blank.
fn gutter_for(galley: &egui::Galley) -> String {
    let mut out = String::new();
    let mut n = 1;
    let mut new_line = true;
    for row in &galley.rows {
        if new_line {
            out.push_str(&format!("{:>4}\n", n));
            n += 1;
        } else {
            out.push('\n');
        }
        new_line = row.ends_with_newline;
    }
    if new_line && galley.rows.last().is_some_and(|r| r.ends_with_newline) {
        out.push_str(&format!("{:>4}\n", n));
    }
    out
}

impl SqlEditor {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            font_size: 13.0,
            auto_complete: true,
            uppercase_keywords: true,
            language: "sql".into(),
            ..Default::default()
        }
    }

    /// Selected text, or empty when nothing is selected.
    pub fn selected_text(&self) -> String {
        match self.selection {
            Some((a, b)) if a != b => {
                let (a, b) = (a.min(b), a.max(b));
                self.text.chars().skip(a).take(b - a).collect()
            }
            _ => String::new(),
        }
    }

    pub fn set_cursor(&mut self, char_pos: usize) {
        self.pending_cursor = Some(char_pos);
    }

    pub fn insert_at_cursor(&mut self, s: &str) {
        let byte = char_to_byte(&self.text, self.cursor);
        self.text.insert_str(byte, s);
        self.pending_cursor = Some(self.cursor + s.chars().count());
    }

    fn replace_range(&mut self, start: usize, end: usize, with: &str) {
        let bs = char_to_byte(&self.text, start);
        let be = char_to_byte(&self.text, end);
        self.text.replace_range(bs..be, with);
        self.pending_cursor = Some(start + with.chars().count());
    }

    pub fn toggle_comment_selection(&mut self) {
        let (a, b) = self.selection.map(|(a, b)| (a.min(b), a.max(b))).unwrap_or((self.cursor, self.cursor));
        // extend to full lines
        let chars: Vec<char> = self.text.chars().collect();
        let mut s = a.min(chars.len());
        while s > 0 && chars[s - 1] != '\n' {
            s -= 1;
        }
        let mut e = b.min(chars.len());
        while e < chars.len() && chars[e] != '\n' {
            e += 1;
        }
        let segment: String = chars[s..e].iter().collect();
        let toggled = sqlutil::toggle_comment(&segment);
        self.replace_range(s, e, &toggled);
    }

    fn find_next(&mut self) {
        if self.find.is_empty() {
            return;
        }
        let hay = self.text.to_lowercase();
        let needle = self.find.to_lowercase();
        let from = char_to_byte(&self.text, self.cursor);
        let pos = hay[from.min(hay.len())..].find(&needle).map(|p| p + from).or_else(|| hay.find(&needle));
        if let Some(bp) = pos {
            let cs = self.text[..bp].chars().count();
            let ce = cs + self.find.chars().count();
            self.pending_cursor = Some(ce);
            self.selection = Some((cs, ce));
            if let Some(id) = self.id {
                self.pending_select = Some((cs, ce, id));
            }
        }
    }

    /// Editor with plain word completion over `extra_words` and SQL keywords.
    pub fn ui(&mut self, ui: &mut Ui, id_salt: &str, extra_words: &[String]) -> egui::Response {
        let mut words = |text: &str, cursor: usize, explicit: bool| complete::complete_words(text, cursor, extra_words, sqlutil::KEYWORDS, explicit);
        self.ui_with(ui, id_salt, &mut words)
    }

    /// Editor whose completion list comes from `completer`.
    pub fn ui_with(&mut self, ui: &mut Ui, id_salt: &str, completer: &mut Completer) -> egui::Response {
        let id = ui.make_persistent_id(id_salt);
        self.id = Some(id);
        // find / replace bar
        let cmd_f = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F));
        if cmd_f {
            self.find_open = !self.find_open;
        }
        let cmd_slash = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Slash));
        if cmd_slash {
            self.toggle_comment_selection();
        }
        if self.find_open {
            ui.horizontal(|ui| {
                ui.label("Find:");
                let r = ui.add(egui::TextEdit::singleline(&mut self.find).desired_width(160.0).min_size(egui::vec2(160.0, 0.0)));
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.find_next();
                }
                if ui.button("Next").clicked() {
                    self.find_next();
                }
                ui.label("Replace:");
                ui.add(egui::TextEdit::singleline(&mut self.replace).desired_width(160.0).min_size(egui::vec2(160.0, 0.0)));
                if ui.button("Replace All").clicked() && !self.find.is_empty() {
                    self.text = self.text.replace(&self.find, &self.replace);
                }
                if ui.button("✖").clicked() {
                    self.find_open = false;
                }
            });
        }

        // completion keys must be consumed before the TextEdit sees them
        let focused = ui.memory(|m| m.has_focus(id));
        let explicit = focused && ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space));
        let mut accept = false;
        if let Some(p) = &mut self.popup {
            let n = p.items.len().max(1);
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    p.selected = (p.selected + 1) % n;
                    p.scroll = true;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    p.selected = (p.selected + n - 1) % n;
                    p.scroll = true;
                }
                if i.consume_key(Modifiers::NONE, Key::PageDown) {
                    p.selected = (p.selected + 8).min(n - 1);
                    p.scroll = true;
                }
                if i.consume_key(Modifiers::NONE, Key::PageUp) {
                    p.selected = p.selected.saturating_sub(8);
                    p.scroll = true;
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab) {
                    accept = true;
                }
                if i.consume_key(Modifiers::NONE, Key::Escape) {
                    p.items.clear();
                }
            });
        }
        if accept {
            if let Some(p) = self.popup.take() {
                if let Some(it) = p.items.get(p.selected).cloned() {
                    self.accept(&p, &it);
                }
            }
        }

        let font_size = self.font_size;
        let language = self.language.clone();
        let mut layouter = |ui: &Ui, string: &str, wrap_width: f32| {
            let theme = egui_extras::syntax_highlighting::CodeTheme::from_memory(ui.ctx(), ui.style());
            let mut job = egui_extras::syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, string, &language);
            for s in job.sections.iter_mut() {
                s.format.font_id.size = font_size;
            }
            job.wrap.max_width = wrap_width;
            ui.fonts(|f| f.layout_job(job))
        };

        if self.gutter.is_empty() {
            let line_count = self.text.lines().count().max(1) + usize::from(self.text.ends_with('\n'));
            self.gutter = (1..=line_count).map(|n| format!("{:>4}\n", n)).collect();
        }
        let mut response = None;
        let mut output_state = None;
        egui::ScrollArea::both().id_salt(format!("{}-scroll", id_salt)).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.add(
                    egui::Label::new(RichText::new(self.gutter.as_str()).font(egui::FontId::monospace(font_size)).weak())
                        .selectable(false)
                        .extend(),
                );
                let out = egui::TextEdit::multiline(&mut self.text)
                    .id(id)
                    .code_editor()
                    .lock_focus(true)
                    .desired_width(f32::INFINITY)
                    .desired_rows(12)
                    .frame(false)
                    .layouter(&mut layouter)
                    .show(ui);
                response = Some(out.response.clone());
                output_state = Some(out);
            });
        });
        let Some(out) = output_state else { return ui.label("") };
        let gutter = gutter_for(&out.galley);
        if gutter != self.gutter {
            self.gutter = gutter;
            ui.ctx().request_repaint();
        }
        let resp = response.unwrap_or_else(|| out.response.clone());

        if let Some(range) = out.cursor_range {
            let a = range.primary.ccursor.index;
            let b = range.secondary.ccursor.index;
            self.cursor = a;
            self.selection = Some((b, a));
        }

        // apply programmatic cursor/selection changes
        let mut state = out.state.clone();
        let mut store = false;
        if let Some((s, e, _)) = self.pending_select.take() {
            state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(s), CCursor::new(e))));
            store = true;
            self.pending_cursor = None;
        } else if let Some(pc) = self.pending_cursor.take() {
            state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(pc))));
            self.cursor = pc;
            store = true;
        }
        if store {
            state.store(ui.ctx(), id);
            resp.request_focus();
        }

        // auto-completion popup
        let typed = resp.changed();
        let grew = typed && self.text.chars().count() > self.last_len;
        self.last_len = self.text.chars().count();
        let refresh = self.popup.as_ref().is_some_and(|p| p.incomplete && p.cursor == self.cursor);
        if self.auto_complete && (explicit || (typed && (grew || self.popup.is_some())) || refresh) {
            let was = self.popup.take().filter(|_| refresh && !typed && !explicit);
            let explicit = explicit || was.as_ref().is_some_and(|p| p.explicit);
            match completer(&self.text, self.cursor, explicit) {
                Some(c) if !c.items.is_empty() || c.incomplete => {
                    let pos = out
                        .galley
                        .pos_from_cursor(&out.galley.from_ccursor(CCursor::new(self.cursor)))
                        .left_bottom()
                        + out.galley_pos.to_vec2();
                    // keep the highlighted entry when a refresh only adds names
                    let selected = was
                        .filter(|w| w.selected > 0)
                        .and_then(|w| w.items.get(w.selected).and_then(|cur| c.items.iter().position(|i| i.label == cur.label)))
                        .unwrap_or(0);
                    self.popup = Some(Popup {
                        start: c.start,
                        end: c.end,
                        items: c.items,
                        selected,
                        pos,
                        cursor: self.cursor,
                        incomplete: c.incomplete,
                        explicit,
                        scroll: true,
                    });
                }
                _ => {}
            }
        } else if typed {
            self.popup = None;
        }
        if let Some(p) = &self.popup {
            if (p.items.is_empty() && !p.incomplete) || !resp.has_focus() || p.cursor != self.cursor {
                self.popup = None;
            }
        }
        let mut clicked: Option<usize> = None;
        if let Some(p) = &mut self.popup {
            let font = egui::FontId::monospace(self.font_size.max(12.0));
            let small = egui::FontId::proportional(11.0);
            egui::Area::new(id.with("completion"))
                .order(egui::Order::Foreground)
                .fixed_pos(p.pos + egui::vec2(0.0, 2.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).inner_margin(4.0).show(ui, |ui| {
                        let width = ui.fonts(|f| {
                            p.items
                                .iter()
                                .map(|it| {
                                    let l = f.layout_no_wrap(it.label.clone(), font.clone(), Color32::WHITE).size().x;
                                    let d = f.layout_no_wrap(it.detail.clone(), small.clone(), Color32::WHITE).size().x;
                                    26.0 + l + 24.0 + d
                                })
                                .fold(240.0f32, f32::max)
                                .min(620.0)
                        });
                        if p.items.is_empty() {
                            ui.label(RichText::new("Loading…").weak());
                            return;
                        }
                        let row_h = font.size + 8.0;
                        // height from the item count, not last frame's content: the list grows when
                        // catalog data arrives and egui wouldn't repaint to catch up
                        let h = p.items.len().min(10) as f32 * row_h;
                        egui::ScrollArea::vertical().max_height(h).min_scrolled_height(h).auto_shrink([true, true]).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.set_width(width);
                            for (i, it) in p.items.iter().enumerate() {
                                let (rect, r) = ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::click());
                                let v = ui.visuals();
                                if i == p.selected {
                                    ui.painter().rect_filled(rect, 3.0, v.selection.bg_fill);
                                    if p.scroll {
                                        ui.scroll_to_rect(rect, None);
                                    }
                                } else if r.hovered() {
                                    ui.painter().rect_filled(rect, 3.0, v.widgets.hovered.weak_bg_fill);
                                }
                                let text_col = if i == p.selected { v.selection.stroke.color } else { v.text_color() };
                                let (glyph, col) = kind_icon(it.kind);
                                let y = rect.center().y;
                                let p_ = ui.painter();
                                p_.text(egui::pos2(rect.left() + 4.0, y), egui::Align2::LEFT_CENTER, glyph, egui::FontId::proportional(13.0), col);
                                p_.text(egui::pos2(rect.left() + 24.0, y), egui::Align2::LEFT_CENTER, &it.label, font.clone(), text_col);
                                if !it.detail.is_empty() {
                                    p_.text(egui::pos2(rect.right() - 6.0, y), egui::Align2::RIGHT_CENTER, &it.detail, small.clone(), v.weak_text_color());
                                }
                                if r.clicked() {
                                    clicked = Some(i);
                                }
                            }
                        });
                        p.scroll = false;
                    });
                });
        }
        if let Some(i) = clicked {
            if let Some(p) = self.popup.take() {
                let it = p.items[i].clone();
                self.accept(&p, &it);
            }
        }
        resp
    }
}

impl SqlEditor {
    fn accept(&mut self, p: &Popup, it: &Item) {
        let mut ins = it.insert.clone();
        if !self.uppercase_keywords && matches!(it.kind, ItemKind::Keyword | ItemKind::Function) {
            ins = ins.to_lowercase();
        }
        let len = self.text.chars().count();
        self.replace_range(p.start.min(len), p.end.min(len), &ins);
        let back = it.cursor_back.min(ins.chars().count());
        self.pending_cursor = Some(p.start + ins.chars().count() - back);
        self.last_len = self.text.chars().count();
    }
}

fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}
