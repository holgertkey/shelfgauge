//! Matching books that disappeared from one location with books that appeared at
//! another, so moved and renamed books keep their id.
//!
//! Identity is a content key: a plain file's content hash, or a zip entry's
//! `(size, CRC32)`. A match is made only when exactly one candidate (a book that
//! vanished in this scan or was removed earlier) and exactly one new location share a
//! key; identical copies are never guessed between, they become new books.

use std::collections::HashMap;
use std::hash::Hash;

/// Splits `new` locations into unique matches `(book id, location)` and the rest.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
/// use shelfgauge::scanner::matcher::match_unique;
///
/// let candidates = HashMap::from([("h1", vec![10]), ("h2", vec![20, 21])]);
/// let new = vec![("h1", "a.fb2"), ("h2", "b.fb2"), ("h3", "c.fb2")];
/// let (matched, rest) = match_unique(new, &candidates);
/// assert_eq!(matched, [(10, "a.fb2")]);
/// assert_eq!(rest, ["b.fb2", "c.fb2"]);
/// ```
pub fn match_unique<K: Hash + Eq, T>(
    new: Vec<(K, T)>,
    candidates: &HashMap<K, Vec<i64>>,
) -> (Vec<(i64, T)>, Vec<T>) {
    let mut new_per_key: HashMap<&K, usize> = HashMap::new();
    for (key, _) in &new {
        *new_per_key.entry(key).or_default() += 1;
    }
    let unique: Vec<bool> = new
        .iter()
        .map(|(key, _)| {
            new_per_key.get(key) == Some(&1)
                && candidates.get(key).is_some_and(|ids| ids.len() == 1)
        })
        .collect();
    let mut matched = Vec::new();
    let mut rest = Vec::new();
    for ((key, location), unique) in new.into_iter().zip(unique) {
        match candidates.get(&key) {
            Some(ids) if unique => matched.push((ids[0], location)),
            _ => rest.push(location),
        }
    }
    (matched, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguity_on_either_side_prevents_matching() {
        let candidates = HashMap::from([(1, vec![100]), (2, vec![200, 201]), (3, vec![300])]);
        let new = vec![(1, 'a'), (2, 'b'), (3, 'c'), (3, 'd'), (4, 'e')];
        let (matched, rest) = match_unique(new, &candidates);
        assert_eq!(matched, [(100, 'a')]);
        assert_eq!(rest, ['b', 'c', 'd', 'e']);
    }

    #[test]
    fn empty_inputs() {
        let (matched, rest) = match_unique::<u8, ()>(Vec::new(), &HashMap::new());
        assert!(matched.is_empty() && rest.is_empty());
    }
}
