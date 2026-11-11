//! Two-language application preferences.

use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Language {
    #[default]
    Chinese,
    English,
}

use std::sync::atomic::{AtomicU8, Ordering};
static LANGUAGE: AtomicU8 = AtomicU8::new(0);
pub fn set_language(language: Language) {
    LANGUAGE.store(
        if language == Language::English { 1 } else { 0 },
        Ordering::Relaxed,
    );
}
struct Dictionary {
    exact: std::collections::HashMap<&'static str, &'static str>,
    formats: Vec<(&'static str, &'static str)>,
}
fn dictionary() -> &'static Dictionary {
    static DICTIONARY: std::sync::OnceLock<Dictionary> = std::sync::OnceLock::new();
    DICTIONARY.get_or_init(|| {
        let pairs: Vec<_> = include_str!("../../../assets/translations.tsv")
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .collect();
        Dictionary {
            exact: pairs.iter().copied().collect(),
            formats: pairs
                .into_iter()
                .filter(|(source, _)| source.contains("{}"))
                .collect(),
        }
    })
}
pub fn text(value: &str) -> &str {
    if LANGUAGE.load(Ordering::Relaxed) == 0 {
        value
    } else {
        dictionary().exact.get(value).copied().unwrap_or(value)
    }
}
pub fn localize(value: String) -> String {
    if LANGUAGE.load(Ordering::Relaxed) == 0 {
        return value;
    }
    if let Some(translated) = dictionary().exact.get(value.as_str()) {
        return translated.to_string();
    }
    for (source, target) in &dictionary().formats {
        if let Some(result) = translate_format(&value, source, target) {
            return result;
        }
    }
    value
}
fn translate_format(value: &str, source: &str, target: &str) -> Option<String> {
    let segments: Vec<_> = source.split("{}").collect();
    let mut remaining = value;
    let mut arguments = Vec::new();
    remaining = remaining.strip_prefix(segments[0])?;
    for (index, segment) in segments.iter().enumerate().skip(1) {
        if segment.is_empty() {
            if index == segments.len() - 1 {
                arguments.push(remaining);
                remaining = "";
            } else {
                return None;
            }
        } else {
            let end = remaining.find(segment)?;
            arguments.push(&remaining[..end]);
            remaining = &remaining[end + segment.len()..];
        }
    }
    if !remaining.is_empty() {
        return None;
    }
    let parts: Vec<_> = target.split("{}").collect();
    if parts.len() != arguments.len() + 1 {
        return None;
    }
    let mut result = String::new();
    for (index, part) in parts.into_iter().enumerate() {
        result.push_str(part);
        if let Some(argument) = arguments.get(index) {
            result.push_str(argument);
        }
    }
    Some(result)
}
