//! QR codes as inline SVG (no script, no image file; presentation attributes only, so
//! the page's CSP allows them).

use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};

/// The QR code of `text` as an `<svg>` element with class `qr` and `label` as its
/// accessible name; `None` if the text is too long for a QR code.
pub(crate) fn svg(text: &str, label: &str) -> Option<String> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M).ok()?;
    let image = code
        .render::<svg::Color<'_>>()
        .module_dimensions(1, 1)
        .dark_color(svg::Color("#000"))
        .light_color(svg::Color("#fff"))
        .build();
    // Without the XML prolog: the element goes inside the page.
    let rest = image.split_once("<svg ")?.1;
    Some(format!(
        "<svg class=\"qr\" role=\"img\" aria-label=\"{}\" {rest}",
        escape_attribute(label)
    ))
}

fn escape_attribute(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_inline_svg_without_styles() {
        let svg = svg("https://hub.example.org/pair#abc.def", "Pairing \"code\"").unwrap();
        assert!(svg.starts_with(
            "<svg class=\"qr\" role=\"img\" aria-label=\"Pairing &quot;code&quot;\" xmlns="
        ));
        assert!(svg.ends_with("</svg>"));
        assert!(!svg.contains("<?xml"));
        assert!(!svg.contains("style"));
        assert!(!svg.contains("<script"));
    }

    #[test]
    fn too_long_text_has_no_code() {
        assert!(svg(&"x".repeat(5000), "long").is_none());
    }
}
