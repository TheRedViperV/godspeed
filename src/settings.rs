//! The few choices the app remembers, in `settings.txt` (`key=value` lines).

use crate::edition;
use crate::tray::Hotkey;

#[derive(Clone, PartialEq)]
pub struct Settings {
    /// Opens the floating search bar from anywhere.
    pub hotkey: Hotkey,
    /// Closing the window leaves the app in the tray, so the hotkey keeps working.
    pub background: bool,
    /// Starts in the tray when Windows starts.
    pub autostart: bool,
    /// The "still running in the tray" notice has been shown once.
    pub told: bool,
    /// The search bar stays open until closed, rather than when you click elsewhere.
    pub bar_pinned: bool,
    /// Where the search bar was dragged to (top left, in points).
    pub bar_pos: Option<[f32; 2]>,
    /// The window was last showing the Space view rather than search.
    pub space: bool,
    /// Light or dark colors, or whatever Windows uses for apps.
    pub appearance: Appearance,
    /// Draws a size column in File Explorer's Details view.
    pub explorer_column: bool,
    /// The column writes sizes as a share of the folder ("56%").
    pub column_share: bool,
    /// The column's arrows mean "grew this week" rather than "today".
    pub column_week: bool,
    /// The column's bars are coloured by share of the folder rather than by size.
    pub column_by_share: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Appearance::System => "system",
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { hotkey: Hotkey::DEFAULT, background: true, autostart: false, told: false, bar_pinned: false, bar_pos: None, space: false, appearance: Appearance::System, explorer_column: true, column_share: false, column_week: false, column_by_share: false }
    }
}

fn file() -> Option<std::path::PathBuf> {
    Some(edition::data_dir()?.join("settings.txt"))
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Some(Ok(text)) = file().map(std::fs::read_to_string) else { return s };
        for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
            let on = v.trim() == "1";
            match k.trim() {
                "hotkey" => s.hotkey = Hotkey::parse(v.trim()).unwrap_or(Hotkey::DEFAULT),
                "background" => s.background = on,
                "autostart" => s.autostart = on,
                "told" => s.told = on,
                "bar_pinned" => s.bar_pinned = on,
                "space" => s.space = on,
                "explorer_column" => s.explorer_column = on,
                "column_share" => s.column_share = on,
                "column_week" => s.column_week = on,
                "column_by_share" => s.column_by_share = on,
                "appearance" => s.appearance = Appearance::ALL.into_iter().find(|a| a.key() == v.trim()).unwrap_or_default(),
                "bar_pos" => {
                    let mut xy = v.split(',').filter_map(|n| n.trim().parse::<f32>().ok());
                    s.bar_pos = xy.next().zip(xy.next()).map(|(x, y)| [x, y]);
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let b = |on: bool| if on { "1" } else { "0" };
        let text = format!(
            "hotkey={}\nbackground={}\nautostart={}\ntold={}\nbar_pinned={}\nspace={}\nappearance={}\nexplorer_column={}\ncolumn_share={}\ncolumn_week={}\ncolumn_by_share={}\n",
            self.hotkey,
            b(self.background),
            b(self.autostart),
            b(self.told),
            b(self.bar_pinned),
            b(self.space),
            self.appearance.key(),
            b(self.explorer_column),
            b(self.column_share),
            b(self.column_week),
            b(self.column_by_share)
        );
        let text = match self.bar_pos {
            Some([x, y]) => format!("{text}bar_pos={x:.0},{y:.0}\n"),
            None => text,
        };
        if let Some(f) = file() {
            let _ = f.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(f, text);
        }
    }
}
