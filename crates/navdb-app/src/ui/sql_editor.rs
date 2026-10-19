//! SQL editor widget: syntax highlighting, line numbers, auto-completion,
//! find/replace and comment toggling.

use egui::text::{CCursor, CCursorRange};
use egui::{Key, Modifiers, RichText, Ui};
use navdb_core::sqlutil;

#[derive(Default)]
pub struct SqlEditor {
    pub text: String,
    pub completions: Vec<String>,
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
}

struct Popup {
    start: usize,
    items: Vec<String>,
    selected: usize,
    pos: egui::Pos2,
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

    pub fn ui(&mut self, ui: &mut Ui, id_salt: &str, extra_words: &[String]) -> egui::Response {
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
        let mut accept = false;
        if let Some(p) = &mut self.popup {
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    p.selected = (p.selected + 1).min(p.items.len().saturating_sub(1));
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    p.selected = p.selected.saturating_sub(1);
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
                if let Some(word) = p.items.get(p.selected) {
                    let w = word.clone();
                    let end = self.cursor;
                    self.replace_range(p.start, end, &w);
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
        if self.auto_complete && resp.changed() {
            let (start, word) = sqlutil::word_before(&self.text, self.cursor);
            if word.chars().count() >= 2 {
                let lw = word.to_ascii_lowercase();
                let mut items: Vec<String> = Vec::new();
                for w in extra_words.iter().chain(self.completions.iter()) {
                    if w.to_ascii_lowercase().starts_with(&lw) && !w.eq_ignore_ascii_case(&word) && !items.contains(w) {
                        items.push(w.clone());
                    }
                }
                for k in sqlutil::KEYWORDS {
                    if k.to_ascii_lowercase().starts_with(&lw) && !k.eq_ignore_ascii_case(&word) {
                        let k = if self.uppercase_keywords { k.to_string() } else { k.to_ascii_lowercase() };
                        if !items.contains(&k) {
                            items.push(k);
                        }
                    }
                }
                items.truncate(12);
                let pos = out
                    .galley
                    .pos_from_cursor(&out.galley.from_ccursor(CCursor::new(self.cursor)))
                    .left_bottom()
                    + out.galley_pos.to_vec2();
                self.popup = if items.is_empty() { None } else { Some(Popup { start, items, selected: 0, pos }) };
            } else {
                self.popup = None;
            }
        }
        if let Some(p) = &self.popup {
            if p.items.is_empty() || !resp.has_focus() {
                self.popup = None;
            }
        }
        let mut clicked: Option<usize> = None;
        if let Some(p) = &self.popup {
            egui::Area::new(id.with("completion"))
                .order(egui::Order::Foreground)
                .fixed_pos(p.pos + egui::vec2(0.0, 2.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(180.0);
                        for (i, it) in p.items.iter().enumerate() {
                            if ui.selectable_label(i == p.selected, RichText::new(it).monospace()).clicked() {
                                clicked = Some(i);
                            }
                        }
                    });
                });
        }
        if let Some(i) = clicked {
            if let Some(p) = self.popup.take() {
                let w = p.items[i].clone();
                let end = self.cursor;
                self.replace_range(p.start, end, &w);
            }
        }
        resp
    }
}

fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}
