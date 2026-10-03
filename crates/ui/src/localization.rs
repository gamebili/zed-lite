use gpui::SharedString;

pub use i18n::{
    Language, language, revision as locale_revision, search_texts, text as localized_text,
};

/// Translate explicitly identified interface text, preserving IDs and user content elsewhere.
pub fn tr(source: impl Into<SharedString>) -> SharedString {
    let source = source.into();
    i18n::lookup(source.as_ref())
        .map(SharedString::from)
        .unwrap_or(source)
}
