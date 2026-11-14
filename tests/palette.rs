//! The Custom theme's palette (`assets/palette.js`): the vectors in
//! `tests/fixtures/palette/vectors.json`, written by `.debug/TESTS/palette_check.mjs`
//! (`SHELFGAUGE_WRITE_VECTORS=1`), keep every contrast limit. CI runs no Node, so the
//! limits are checked here again on what the script derived; the script itself checks
//! a grid of some 26 000 pairs.

use serde_json::Value;

const VECTORS: &str = include_str!("fixtures/palette/vectors.json");

/// The WCAG 2 relative luminance of `#rrggbb`.
fn luminance(hex: &str) -> f64 {
    assert!(
        hex.len() == 7
            && hex.starts_with('#')
            && hex[1..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "not #rrggbb: {hex}"
    );
    let channel = |i: usize| {
        let c = f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap()) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
}

/// The WCAG 2 contrast ratio of two colors.
fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn vectors() -> Vec<Value> {
    serde_json::from_str::<Value>(VECTORS)
        .expect("vectors are JSON")
        .as_array()
        .expect("an array of vectors")
        .clone()
}

/// Each derived palette: text at least 7:1 on the background and the sidebar; muted
/// text at least 4.5:1 there and on the hover color; the accent, the error color and
/// the profile colors at least 4.5:1 on the background and the sidebar; the text on
/// them, and the text on the tinted surfaces, at least 4.5:1.
#[test]
fn every_vector_keeps_the_contrast_limits() {
    let vectors = vectors();
    assert!(vectors.len() >= 20, "{} vectors", vectors.len());
    for vector in &vectors {
        let input = &vector["input"];
        let vars = &vector["vars"];
        let var = |name: &str| {
            vars[name]
                .as_str()
                .unwrap_or_else(|| panic!("{name} missing for {input}"))
        };
        let need = |a: &str, b: &str, min: f64| {
            let ratio = contrast(var(a), var(b));
            assert!(
                ratio >= min,
                "{input}: {a} {} on {b} {} is {ratio:.2}, under {min}",
                var(a),
                var(b)
            );
        };
        for surface in ["--bg", "--sidebar-bg"] {
            need("--fg", surface, 7.0);
            need("--accent", surface, 4.5);
            need("--error", surface, 4.5);
            for i in 0..8 {
                need(&format!("--pcolor-{i}"), surface, 4.5);
            }
        }
        for surface in ["--bg", "--sidebar-bg", "--hover"] {
            need("--muted", surface, 4.5);
        }
        for surface in [
            "--active-bg",
            "--banner-bg",
            "--warning-bg",
            "--hover",
            "--cover-bg",
            "--input-bg",
        ] {
            need("--fg", surface, 4.5);
        }
        need("--on-accent", "--accent", 4.5);
        for i in 0..8 {
            need(&format!("--on-pcolor-{i}"), &format!("--pcolor-{i}"), 4.5);
        }
        for name in ["--border", "--sidebar-bg", "--hover", "--cover-bg"] {
            luminance(var(name));
        }
        assert_eq!(var("--pcolor-0"), var("--accent"), "{input}");

        // Light or dark by the background in use: white text has more contrast on a
        // dark one.
        let bg = var("--bg");
        let dark = contrast(bg, "#ffffff") >= contrast(bg, "#000000");
        assert_eq!(
            vector["scheme"].as_str(),
            Some(if dark { "dark" } else { "light" }),
            "{input}"
        );

        // A note for each of the two given colors that had to change, none otherwise.
        let notes: Vec<&str> = vector["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        assert_eq!(
            notes.contains(&"background"),
            input["bg"].as_str() != Some(bg),
            "{input}"
        );
        assert_eq!(
            notes.contains(&"accent"),
            input["accent"].as_str() != Some(var("--accent")),
            "{input}"
        );
    }
}

/// The vectors start with every built-in theme's own background and accent (in the
/// order of the theme menu), and those keep both colors: the built-in themes already
/// meet the limits.
#[test]
fn the_built_in_themes_keep_their_two_colors() {
    let style = include_str!("../assets/style.css");
    let theme_vars = |selector: &str| {
        let start = style.find(selector).unwrap_or_else(|| panic!("{selector}"));
        let body = &style[start..start + style[start..].find('}').unwrap()];
        let get = |name: &str| {
            let at = body.find(&format!("{name}: ")).unwrap() + name.len() + 2;
            body[at..at + 7].to_owned()
        };
        (get("--bg"), get("--accent"))
    };
    let mut themes = vec![theme_vars(":root {")];
    for theme in ["sepia", "mist", "dusk", "dark", "navy", "forest", "wine"] {
        themes.push(theme_vars(&format!(":root[data-theme=\"{theme}\"] {{")));
    }
    let vectors = vectors();
    for (vector, (bg, accent)) in vectors.iter().zip(&themes) {
        assert_eq!(vector["input"]["bg"].as_str(), Some(bg.as_str()));
        assert_eq!(vector["input"]["accent"].as_str(), Some(accent.as_str()));
        assert_eq!(vector["vars"]["--bg"].as_str(), Some(bg.as_str()));
        assert_eq!(vector["vars"]["--accent"].as_str(), Some(accent.as_str()));
        assert_eq!(vector["notes"], serde_json::json!([]), "{bg} {accent}");
    }
}
