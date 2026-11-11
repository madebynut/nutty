//! Installed font discovery and independent Latin/CJK typography.

use std::sync::{Arc, OnceLock};

use gpui_kit::{App, Font, FontFallbacks, font};

use crate::config::Preferences;

pub(crate) use nutty_core::font_defaults::{default_chinese, default_latin};

fn installed(cx: &App) -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| cx.text_system().all_font_names())
}

fn resolve<'a>(requested: &'a str, default: &'a str, names: &[String]) -> &'a str {
    if requested == ".SystemUIFont" {
        return requested;
    }
    [
        requested,
        default,
        "Monaco",
        "Cascadia Mono",
        "Liberation Mono",
    ]
    .into_iter()
    .take(if default == default_latin() { 5 } else { 2 })
    .find(|candidate| names.iter().any(|name| name == candidate))
    .unwrap_or(".SystemUIFont")
}

#[derive(Clone)]
pub(crate) struct FontPair {
    pub latin: Font,
    pub chinese: Font,
}

impl FontPair {
    fn new(latin: &str, chinese: &str, cx: &App) -> Self {
        let names = installed(cx);
        let latin = resolve(latin, default_latin(), names);
        let chinese = resolve(chinese, default_chinese(), names);
        let mut latin_font = font(latin.to_owned());
        latin_font.fallbacks = Some(FontFallbacks::from_fonts(vec![chinese.to_owned()]));
        let mut chinese_font = font(chinese.to_owned());
        chinese_font.fallbacks = Some(FontFallbacks::from_fonts(vec![latin.to_owned()]));
        Self {
            latin: latin_font,
            chinese: chinese_font,
        }
    }

    pub fn ui(preferences: &Preferences, cx: &App) -> Self {
        Self::new(
            &preferences.ui_font_family,
            &preferences.ui_font_family_zh,
            cx,
        )
    }

    pub fn agent(preferences: &Preferences, cx: &App) -> Self {
        Self::new(
            &preferences.agent_font_family,
            &preferences.agent_font_family_zh,
            cx,
        )
    }

    pub fn terminal(preferences: &Preferences, cx: &App) -> Self {
        let mut pair = Self::new(&preferences.font_family, &preferences.font_family_zh, cx);
        let features = gpui_kit::FontFeatures(Arc::new(
            ["liga", "clig", "dlig", "calt"]
                .map(|tag| (tag.to_owned(), 0))
                .into(),
        ));
        pair.latin.features = features.clone();
        pair.chinese.features = features;
        pair
    }

    pub fn for_text(&self, text: &str) -> &Font {
        if text.chars().any(is_cjk) {
            &self.chinese
        } else {
            &self.latin
        }
    }
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32, 0x2e80..=0x9fff | 0xf900..=0xfaff | 0xfe30..=0xfe4f | 0xff01..=0xff60 | 0x20000..=0x323af)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fonts_use_installed_alternatives_without_losing_configured_names() {
        let names = vec!["Custom Mono".into(), "Monaco".into()];
        assert_eq!(
            resolve("Custom Mono", default_latin(), &names),
            "Custom Mono"
        );
        assert_eq!(resolve("Unavailable", default_latin(), &names), "Monaco");
        assert_eq!(
            resolve("Unavailable", default_latin(), &[]),
            ".SystemUIFont"
        );
        assert_eq!(
            resolve("Unavailable", default_chinese(), &names),
            ".SystemUIFont"
        );
        assert!(is_cjk('中') && is_cjk('𠀀') && is_cjk('，'));
        assert!(!is_cjk('A') && !is_cjk('é'));
    }
}
