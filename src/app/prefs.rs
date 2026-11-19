//! The settings card: the search hotkey (set by pressing it), staying in the
//! tray, and starting with Windows.

use super::*;
use crate::settings::Appearance;

impl App {
    /// The hotkey, in the status bar. A click opens the settings.
    pub(super) fn hotkey_chip(&mut self, ui: &mut Ui, p: &Pal) {
        let k = self.settings.hotkey;
        let (text, color, tip) = match tray::hotkey_state() {
            HotkeyState::Taken => {
                (format!("⌨ {k} unavailable"), p.warn, format!("Another app is using {k}. Click to pick a different hotkey."))
            }
            _ => (format!("⌨ {k} to search anywhere"), p.weak, "Opens a search bar over whatever you're doing. Click to change.".into()),
        };
        let b = ui.add(egui::Button::new(RichText::new(text).size(11.5).color(color)).frame(false)).on_hover_text(tip);
        if b.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            self.toggle_settings();
        }
    }

    /// Opens or closes the settings card (the gear in the header, or the
    /// hotkey in the status bar).
    pub(super) fn toggle_settings(&mut self) {
        self.settings_open = !self.settings_open;
        self.stop_recording();
    }

    fn stop_recording(&mut self) {
        if self.recording {
            self.recording = false;
            tray::set_hotkey(Some(self.settings.hotkey));
        }
    }

    /// While recording, the next key combination becomes the hotkey; nothing
    /// pressed meanwhile reaches the rest of the app. Esc closes the card.
    pub(super) fn settings_keys(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        if !self.recording {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.settings_open = false;
            }
            return;
        }
        let (mut got, mut cancel, mut unusable) = (None, false, false);
        ctx.input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    match (*key, Hotkey::from_key(*modifiers, *key)) {
                        (Key::Escape, _) => cancel = true,
                        (_, Some(h)) => got = Some(h),
                        (_, None) => unusable = true,
                    }
                    false
                }
                egui::Event::Key { .. } | egui::Event::Text(_) => false,
                _ => true,
            })
        });
        if cancel {
            self.stop_recording();
        } else if let Some(h) = got {
            self.recording = false;
            self.settings.hotkey = h;
            self.settings.save();
            tray::set_hotkey(Some(h));
            self.settings_note = None;
        } else if unusable {
            self.settings_note = Some("Use Ctrl or Alt with a letter, a number or Space, or an F key on its own.".into());
        }
    }

    pub(super) fn settings_card(&mut self, ctx: &egui::Context, p: &Pal) {
        let t = ease(ctx.animate_bool_with_time(Id::new("settings-open"), self.settings_open, 0.16));
        if t <= 0.001 {
            return;
        }
        let name = edition::get().name;
        egui::Area::new(Id::new("settings"))
            .order(egui::Order::Foreground)
            // Just under the settings button at the header's right end.
            .anchor(Align2::RIGHT_TOP, vec2(-14.0, 10.0 + HEADER_H + 8.0 - (1.0 - t) * 8.0))
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                Frame::new()
                    .fill(p.raised)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .corner_radius(14.0)
                    .inner_margin(Margin::same(18))
                    .show(ui, |ui| {
                        ui.set_width(380.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Settings").font(theme::semibold(15.0)).color(p.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let x = ui.add(egui::Button::new(RichText::new("✕").size(12.0).color(p.weak)).frame(false));
                                if x.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                                    self.settings_open = false;
                                    self.stop_recording();
                                }
                            });
                        });
                        ui.add_space(12.0);
                        ui.label(RichText::new("Search from anywhere").font(theme::semibold(13.0)).color(p.text));
                        ui.label(RichText::new("A hotkey opens a search bar over whatever you're doing.").size(12.0).color(p.weak));
                        ui.add_space(10.0);

                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Hotkey").size(13.0).color(p.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let rec = self.recording;
                                let label = if rec { "Press keys…".to_string() } else { self.settings.hotkey.to_string() };
                                let b = egui::Button::new(RichText::new(label).font(theme::semibold(13.0)).color(if rec { p.accent } else { p.text }))
                                    .fill(p.field)
                                    .stroke(Stroke::new(1.0_f32, if rec { p.accent } else { p.border }))
                                    .corner_radius(8.0)
                                    .min_size(vec2(140.0, 30.0));
                                let b = ui.add(b).on_hover_text("Click, then press the keys you want");
                                if b.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                                    if rec {
                                        self.stop_recording();
                                    } else {
                                        // Off while recording, so pressing it again can pick it.
                                        self.recording = true;
                                        tray::set_hotkey(None);
                                        self.settings_note = None;
                                    }
                                }
                                if !rec && self.settings.hotkey != Hotkey::DEFAULT {
                                    let reset = ui.add(egui::Button::new(RichText::new("Reset").size(12.0).color(p.accent)).frame(false));
                                    if reset.on_hover_text(format!("Back to {}", Hotkey::DEFAULT)).clicked() {
                                        self.settings.hotkey = Hotkey::DEFAULT;
                                        self.settings.save();
                                        tray::set_hotkey(Some(Hotkey::DEFAULT));
                                    }
                                }
                            });
                        });
                        ui.add_space(4.0);
                        let k = self.settings.hotkey;
                        let (text, color) = if self.recording {
                            ("Press the new combination. Esc cancels.".to_string(), p.weak)
                        } else {
                            match tray::hotkey_state() {
                                HotkeyState::Ready if self.settings.background => {
                                    ("✓ Works everywhere, even with this window closed".to_string(), p.good)
                                }
                                HotkeyState::Ready => (format!("✓ Works everywhere while {name} is open"), p.good),
                                HotkeyState::Taken => (format!("⚠ Another app already uses {k}. Pick a different one."), p.warn),
                                HotkeyState::Off => ("Setting it up…".to_string(), p.faint),
                            }
                        };
                        ui.label(RichText::new(text).size(12.0).color(color));
                        if let Some(n) = &self.settings_note {
                            ui.label(RichText::new(n).size(12.0).color(p.warn));
                        }
                        ui.add_space(12.0);
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                        ui.painter().rect_filled(r, 0.0, p.divider);
                        ui.add_space(10.0);

                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Appearance").size(13.0).color(p.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let opts = Appearance::ALL.map(|a| (a.label(), ""));
                                let now = Appearance::ALL.iter().position(|&a| a == self.settings.appearance).unwrap_or(0);
                                if let Some(i) = segmented(ui, "appearance", &opts, now, 210.0, 30.0, p) {
                                    self.settings.appearance = Appearance::ALL[i];
                                    self.settings.save();
                                    theme::apply_appearance(ui.ctx(), Appearance::ALL[i]);
                                }
                            });
                        });
                        ui.add_space(10.0);
                        let bg = self.settings.background;
                        if toggle_row(ui, p, bg, "Keep running in the tray", "Closing the window keeps the hotkey working.") {
                            self.settings.background = !bg;
                            self.settings.save();
                        }
                        ui.add_space(8.0);
                        let on = self.settings.autostart;
                        if toggle_row(ui, p, on, "Start with Windows", "Starts quietly in the tray when you sign in.") {
                            match tray::set_autostart(!on) {
                                Ok(()) => {
                                    self.settings.autostart = !on;
                                    self.settings.save();
                                    self.settings_note = None;
                                }
                                Err(e) => self.settings_note = Some(format!("Couldn't change that: {e}")),
                            }
                        }
                        ui.add_space(8.0);
                        let on = self.settings.explorer_column;
                        let sub = "Shows how big every file and folder is in File Explorer's Details view.";
                        if toggle_row(ui, p, on, "Size column in File Explorer", sub) {
                            self.settings.explorer_column = !on;
                            self.settings.save();
                            crate::column::ON.store(!on, std::sync::atomic::Ordering::Relaxed);
                        }
                        ui.add_space(12.0);
                        let quit = ui.add(egui::Button::new(RichText::new(format!("Quit {name}")).size(12.0).color(p.weak)).frame(false));
                        if quit.on_hover_text("Closes it completely; the hotkey stops working").clicked() {
                            self.quit(ui.ctx());
                        }
                    });
            });
    }
}

/// A setting with a switch on the right. True when the switch was clicked.
fn toggle_row(ui: &mut Ui, p: &Pal, on: bool, title: &str, sub: &str) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.allocate_ui(vec2(300.0, 36.0), |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(title).size(13.0).color(p.text));
                ui.label(RichText::new(sub).size(11.5).color(p.faint));
            });
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            clicked = theme::toggle(ui, on, p).clicked();
        });
    });
    clicked
}
