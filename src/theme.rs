//! Dark terminal themes. Default is a dark-red palette on a near-black
//! background; every slot is overridable per theme.

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::ThemeConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub fg: Color,
    pub accent: Color,
    pub accent_dim: Color,
    pub border: Color,
    pub muted: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub user: Color,
    pub diff_add: Color,
    pub diff_del: Color,
}

pub const SLOTS: &[&str] = &[
    "bg",
    "panel",
    "fg",
    "accent",
    "accent_dim",
    "border",
    "muted",
    "ok",
    "warn",
    "err",
    "user",
    "diff_add",
    "diff_del",
];

impl Theme {
    pub fn preset(name: &str) -> Option<Theme> {
        let t = match name {
            // Default: near-black warm background, dark red accent, readable text.
            "dark-red" => Theme {
                bg: rgb("#0e0b0b"),
                panel: rgb("#171112"),
                fg: rgb("#e2d9d8"),
                accent: rgb("#c8352c"),
                accent_dim: rgb("#7e2a25"),
                border: rgb("#4a2622"),
                muted: rgb("#9a8b89"),
                ok: rgb("#6fa86f"),
                warn: rgb("#d3a355"),
                err: rgb("#e0554c"),
                user: rgb("#d98a83"),
                diff_add: rgb("#5f9e6a"),
                diff_del: rgb("#b74a42"),
            },
            "ember" => Theme {
                bg: rgb("#0c0a09"),
                panel: rgb("#191310"),
                fg: rgb("#e8ded6"),
                accent: rgb("#e2703a"),
                accent_dim: rgb("#8a4426"),
                border: rgb("#4d2c1c"),
                muted: rgb("#a08f83"),
                ok: rgb("#79a86f"),
                warn: rgb("#dfb05a"),
                err: rgb("#e05a4a"),
                user: rgb("#e0a184"),
                diff_add: rgb("#639e6b"),
                diff_del: rgb("#b85042"),
            },
            "mono" => Theme {
                bg: rgb("#0b0b0c"),
                panel: rgb("#161617"),
                fg: rgb("#dcdcdc"),
                accent: rgb("#bdbdbd"),
                accent_dim: rgb("#7a7a7a"),
                border: rgb("#3a3a3a"),
                muted: rgb("#8c8c8c"),
                ok: rgb("#9ccf9c"),
                warn: rgb("#d6c07a"),
                err: rgb("#e08585"),
                user: rgb("#c8c8c8"),
                diff_add: rgb("#8fbf8f"),
                diff_del: rgb("#c98b8b"),
            },
            "oxblood" => Theme {
                bg: rgb("#0a0709"),
                panel: rgb("#160f13"),
                fg: rgb("#e4dade"),
                accent: rgb("#a01f3a"),
                accent_dim: rgb("#6b1626"),
                border: rgb("#45202c"),
                muted: rgb("#9d8a91"),
                ok: rgb("#6fa886"),
                warn: rgb("#cfa25c"),
                err: rgb("#df5560"),
                user: rgb("#cf8f9c"),
                diff_add: rgb("#5d9a72"),
                diff_del: rgb("#b84551"),
            },
            "slate-red" => Theme {
                bg: rgb("#0c0f12"),
                panel: rgb("#151a1f"),
                fg: rgb("#dbe1e6"),
                accent: rgb("#cf4b3f"),
                accent_dim: rgb("#7f2f28"),
                border: rgb("#2f3a42"),
                muted: rgb("#8a97a0"),
                ok: rgb("#74a98a"),
                warn: rgb("#d9b063"),
                err: rgb("#e06055"),
                user: rgb("#d9948c"),
                diff_add: rgb("#5f9c79"),
                diff_del: rgb("#b8544a"),
            },
            _ => return None,
        };
        Some(t)
    }

    pub fn preset_names() -> &'static [&'static str] {
        &["dark-red", "ember", "mono", "oxblood", "slate-red"]
    }

    pub fn resolve(cfg: &ThemeConfig) -> Theme {
        let mut theme = Theme::preset(&cfg.name)
            .or_else(|| Theme::preset("dark-red"))
            .expect("default theme exists");
        for (slot, value) in &cfg.custom {
            if let Some(color) = parse(value) {
                theme.set_slot(slot, color);
            }
        }
        theme
    }

    /// Apply one custom color. Unknown slot names are ignored.
    pub fn set_slot(&mut self, slot: &str, color: Color) {
        match slot {
            "bg" => self.bg = color,
            "panel" => self.panel = color,
            "fg" => self.fg = color,
            "accent" => self.accent = color,
            "accent_dim" => self.accent_dim = color,
            "border" => self.border = color,
            "muted" => self.muted = color,
            "ok" => self.ok = color,
            "warn" => self.warn = color,
            "err" => self.err = color,
            "user" => self.user = color,
            "diff_add" => self.diff_add = color,
            "diff_del" => self.diff_del = color,
            _ => {}
        }
    }

    pub fn to_map(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for (slot, color) in self.slots() {
            map.insert(slot.to_string(), to_hex(color));
        }
        map
    }

    pub fn slots(&self) -> Vec<(&'static str, Color)> {
        vec![
            ("bg", self.bg),
            ("panel", self.panel),
            ("fg", self.fg),
            ("accent", self.accent),
            ("accent_dim", self.accent_dim),
            ("border", self.border),
            ("muted", self.muted),
            ("ok", self.ok),
            ("warn", self.warn),
            ("err", self.err),
            ("user", self.user),
            ("diff_add", self.diff_add),
            ("diff_del", self.diff_del),
        ]
    }
}

/// Parse `#rrggbb`. Also accepts `rrggbb` and named ANSI colors.
pub fn parse(value: &str) -> Option<Color> {
    let v = value.trim();
    let named = match v.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "white" => Some(Color::White),
        "darkgray" | "dark-grey" => Some(Color::DarkGray),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let hex = v.strip_prefix('#').unwrap_or(v);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

pub fn to_hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

const fn rgb(hex: &str) -> Color {
    let bytes = hex.as_bytes();
    // const fn cannot use from_str_radix; decode manually.
    let r = dec(bytes[1]) * 16 + dec(bytes[2]);
    let g = dec(bytes[3]) * 16 + dec(bytes[4]);
    let b = dec(bytes[5]) * 16 + dec(bytes[6]);
    Color::Rgb(r, g, b)
}

const fn dec(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

/// A theme selection the user can persist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeChoice {
    pub name: String,
    pub custom: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_names() {
        assert_eq!(parse("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse("00FF00"), Some(Color::Rgb(0, 255, 0)));
        assert_eq!(parse("red"), Some(Color::Red));
        assert_eq!(parse("nope"), None);
    }

    #[test]
    fn custom_overrides_apply() {
        let mut custom = BTreeMap::new();
        custom.insert("accent".to_string(), "#123456".to_string());
        custom.insert("bogus".to_string(), "#ffffff".to_string());
        let cfg = ThemeConfig {
            name: "dark-red".into(),
            custom,
        };
        let t = Theme::resolve(&cfg);
        assert_eq!(t.accent, Color::Rgb(0x12, 0x34, 0x56));
    }
}
