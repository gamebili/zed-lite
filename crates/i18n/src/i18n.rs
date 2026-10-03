use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{
        OnceLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    #[default]
    English,
    SimplifiedChinese,
    TraditionalChinese,
}

// Initialize at application startup, rather than changing isolated UI tests by host locale.
static LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);
static REVISION: AtomicU64 = AtomicU64::new(0);
static SIMPLIFIED: OnceLock<HashMap<String, String>> = OnceLock::new();
static TRADITIONAL: OnceLock<HashMap<String, String>> = OnceLock::new();

const SIMPLIFIED_SOURCES: &[&str] = &[
    include_str!("../locales/ui.zh-CN.json"),
    include_str!("../locales/settings.zh-CN.json"),
    include_str!("../locales/commands.zh-CN.json"),
    include_str!("../locales/common.zh-CN.json"),
];
const TRADITIONAL_SOURCES: &[&str] = &[
    include_str!("../locales/ui.zh-TW.json"),
    include_str!("../locales/settings.zh-TW.json"),
    include_str!("../locales/commands.zh-TW.json"),
    include_str!("../locales/common.zh-TW.json"),
];

pub fn language() -> Language {
    match LANGUAGE.load(Ordering::Relaxed) {
        1 => Language::SimplifiedChinese,
        2 => Language::TraditionalChinese,
        _ => Language::English,
    }
}

pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

pub fn resolve_language(selection: &str, system_locale: Option<&str>) -> Language {
    let selected = selection.trim();
    let locale = if selected.is_empty()
        || selected.eq_ignore_ascii_case("system")
        || selected.eq_ignore_ascii_case("auto")
    {
        system_locale.unwrap_or("en")
    } else {
        selected
    };
    let normalized = locale.replace('_', "-").to_ascii_lowercase();
    let subtags: Vec<_> = normalized.split('-').collect();
    if subtags.first() != Some(&"zh") {
        return Language::English;
    }
    if subtags.contains(&"hant") || subtags.contains(&"cht") {
        Language::TraditionalChinese
    } else if subtags.contains(&"hans") || subtags.contains(&"chs") {
        Language::SimplifiedChinese
    } else if subtags
        .iter()
        .any(|region| matches!(*region, "tw" | "hk" | "mo"))
    {
        Language::TraditionalChinese
    } else {
        Language::SimplifiedChinese
    }
}

pub fn set_language(selection: &str) -> bool {
    let system_locale = sys_locale::get_locale();
    let selected = resolve_language(selection, system_locale.as_deref());
    if LANGUAGE.swap(selected as u8, Ordering::Relaxed) != selected as u8 {
        REVISION.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn load_catalog(sources: &[&str]) -> HashMap<String, String> {
    let mut catalog = HashMap::new();
    for source in sources {
        match serde_json::from_str::<HashMap<String, String>>(source) {
            Ok(entries) => catalog.extend(entries),
            Err(error) => log::error!("Could not load bundled UI translations: {error}"),
        }
    }
    catalog
}

fn catalog(language: Language) -> Option<&'static HashMap<String, String>> {
    match language {
        Language::English => None,
        Language::SimplifiedChinese => {
            Some(SIMPLIFIED.get_or_init(|| load_catalog(SIMPLIFIED_SOURCES)))
        }
        Language::TraditionalChinese => {
            Some(TRADITIONAL.get_or_init(|| load_catalog(TRADITIONAL_SOURCES)))
        }
    }
}

pub fn lookup_for(language: Language, source: &str) -> Option<&'static str> {
    catalog(language)?.get(source).map(String::as_str)
}

pub fn lookup(source: &str) -> Option<&'static str> {
    lookup_for(language(), source)
}

pub fn text(source: &str) -> &str {
    lookup(source).unwrap_or(source)
}

pub fn search_texts(source: &str) -> [&str; 3] {
    [
        source,
        lookup_for(Language::SimplifiedChinese, source).unwrap_or(source),
        lookup_for(Language::TraditionalChinese, source).unwrap_or(source),
    ]
}

pub fn translate(source: &str) -> Cow<'_, str> {
    Cow::Borrowed(text(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_system_display_language_and_honors_explicit_override() {
        assert_eq!(
            resolve_language("system", Some("zh-Hans-CN")),
            Language::SimplifiedChinese
        );
        assert_eq!(
            resolve_language("system", Some("zh-Hant-HK")),
            Language::TraditionalChinese
        );
        assert_eq!(resolve_language("en", Some("zh-CN")), Language::English);
        assert_eq!(
            resolve_language("zh-TW", Some("en-US")),
            Language::TraditionalChinese
        );
        assert_eq!(resolve_language("system", None), Language::English);
        assert_eq!(resolve_language("system", Some("fr-FR")), Language::English);
    }

    #[test]
    fn script_takes_precedence_over_region_and_locale_spelling_is_normalized() {
        assert_eq!(
            resolve_language("ZH_hans_TW", None),
            Language::SimplifiedChinese
        );
        assert_eq!(
            resolve_language("zh-Hant-CN", None),
            Language::TraditionalChinese
        );
        assert_eq!(
            resolve_language("zh_HK", None),
            Language::TraditionalChinese
        );
        assert_eq!(resolve_language("zh-SG", None), Language::SimplifiedChinese);
    }

    #[test]
    fn untranslated_content_and_english_are_preserved() {
        assert_eq!(lookup_for(Language::English, "Cancel"), None);
        assert_eq!(
            lookup_for(Language::SimplifiedChinese, "user-defined-file.rs"),
            None
        );
        assert_eq!(
            lookup_for(Language::SimplifiedChinese, "Cancel"),
            Some("取消")
        );
        assert_eq!(
            lookup_for(Language::TraditionalChinese, "Settings"),
            Some("設定")
        );
    }

    #[test]
    fn bundled_catalogs_are_valid_and_have_matching_keys() -> Result<(), serde_json::Error> {
        for (simplified, traditional) in SIMPLIFIED_SOURCES.iter().zip(TRADITIONAL_SOURCES) {
            let simplified: HashMap<String, String> = serde_json::from_str(simplified)?;
            let traditional: HashMap<String, String> = serde_json::from_str(traditional)?;
            let mut simplified_keys: Vec<_> = simplified.keys().collect();
            let mut traditional_keys: Vec<_> = traditional.keys().collect();
            simplified_keys.sort();
            traditional_keys.sort();
            assert_eq!(simplified_keys, traditional_keys);
            assert!(simplified.values().all(|text| !text.is_empty()));
            assert!(traditional.values().all(|text| !text.is_empty()));
        }
        Ok(())
    }
}
