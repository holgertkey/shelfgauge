//! Folding text for search and sorting, shared by the library and the hub reader.
//!
//! The library searches with SQLite FTS5 (`unicode61 remove_diacritics 2`), which folds
//! case and Latin diacritics itself; it is given text folded with
//! [`fold_for_search`]. The hub reader searches a library's catalog without SQLite, so
//! [`fold_full`] does all of it: case, Latin diacritics, `ё` → `е`. [`normalize_key`]
//! orders and de-duplicates names (authors, series) on both sides.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Folds text for full-text search, applied to indexed text and queries alike.
///
/// `unicode61` folds case for all scripts and Latin diacritics, but not `ё`; it is
/// mapped to `е` here, as Russian text uses both spellings. `й` stays distinct.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::fold::fold_for_search;
///
/// assert_eq!(fold_for_search("Ёлка и ёж"), "Елка и еж");
/// assert_eq!(fold_for_search("Йод"), "Йод");
/// ```
pub fn fold_for_search(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'ё' => 'е',
            'Ё' => 'Е',
            c => c,
        })
        .collect()
}

/// A normalized key for sorting and de-duplicating names: lowercase, `ё` → `е`,
/// whitespace collapsed and trimmed.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::fold::normalize_key;
///
/// assert_eq!(normalize_key("  Лев   Толстой "), "лев толстой");
/// assert_eq!(normalize_key("Пётр"), "петр");
/// ```
pub fn normalize_key(text: &str) -> String {
    let lower = fold_for_search(&text.to_lowercase());
    lower.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Folds text as the library's search does as a whole ([`fold_for_search`] and then
/// SQLite's `unicode61 remove_diacritics 2`): lowercase, diacritics removed from Latin
/// letters, `ё` → `е`. Marks on letters of other scripts stay, so `й` is not `и`. The
/// text is composed (NFC) first, so decomposed metadata (common in EPUBs written on
/// macOS) folds as the composed one.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::fold::fold_full;
///
/// assert_eq!(fold_full("Émile Zola, Ærø"), "emile zola, ærø");
/// assert_eq!(fold_full("Ёжик Йожин"), "ежик йожин");
/// assert_eq!(fold_full("Ǖ"), "u");
/// ```
pub fn fold_full(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.nfc().flat_map(char::to_lowercase) {
        match c {
            'ё' => out.push('е'),
            c if c.is_ascii() => out.push(c),
            c => {
                let mut parts = std::iter::once(c).nfd();
                match parts.next() {
                    Some(base) if is_latin(base) => {
                        out.push(base);
                        out.extend(parts.filter(|&p| !is_combining_mark(p)));
                    }
                    _ => out.push(c),
                }
            }
        }
    }
    out
}

/// Whether `c` is a Latin letter (Basic Latin, Latin-1, Latin Extended A to E,
/// Latin Extended Additional).
fn is_latin(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(c,
            '\u{00c0}'..='\u{024f}'
            | '\u{1e00}'..='\u{1eff}'
            | '\u{2c60}'..='\u{2c7f}'
            | '\u{a720}'..='\u{a7ff}'
            | '\u{ab30}'..='\u{ab6f}')
}

/// The words of `text` after [`fold_full`]: runs of letters and digits, everything
/// else separates them (as the library's search splits a query).
///
/// # Examples
///
/// ```
/// use shelfgauge_core::fold::words;
///
/// assert_eq!(words("Война и мир, т. 1"), ["война", "и", "мир", "т", "1"]);
/// assert!(words(" -- ").is_empty());
/// ```
pub fn words(text: &str) -> Vec<String> {
    fold_full(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_folding_keeps_cyrillic_short_i_apart() {
        assert_eq!(fold_full("Й"), "й");
        assert_eq!(fold_full("й"), "й");
        assert_ne!(fold_full("йод"), fold_full("иод"));
        // A decomposed й (и + combining breve) is й, and ё is е either way.
        assert_eq!(fold_full("и\u{0306}"), "й");
        assert_eq!(fold_full("е\u{0308}лка"), "елка");
    }

    #[test]
    fn full_folding_removes_latin_diacritics() {
        assert_eq!(fold_full("Crème Brûlée"), "creme brulee");
        assert_eq!(fold_full("Łódź"), "łodz");
        assert_eq!(fold_full("Ṩ"), "s");
        assert_eq!(fold_full("ÅNGSTRÖM"), "angstrom");
        assert_eq!(fold_full("Ёё"), "ее");
    }

    #[test]
    fn decomposed_text_makes_the_same_words() {
        assert_eq!(words("re\u{301}sume\u{301}"), ["resume"]);
        assert_eq!(words("И\u{306}од"), ["йод"]);
        assert_eq!(words("Re\u{301}sume\u{301}"), words("Résumé"));
    }

    #[test]
    fn folding_for_search_is_unchanged() {
        // `books_fts` holds text folded this way: changing it needs a reindex.
        assert_eq!(fold_for_search("Ёжик ёлка Éé"), "Ежик елка Éé");
        assert_eq!(normalize_key(" Ёлки\tИ  Палки "), "елки и палки");
    }
}
