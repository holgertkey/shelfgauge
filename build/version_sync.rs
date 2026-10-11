//! Version synchronization between `Cargo.toml` and the documentation files.
//!
//! The version is defined only in `Cargo.toml`. On every build, `build.rs` uses these
//! functions to update `README.md` and `CHANGELOG.md`. All functions here are pure
//! (string in, string out) so they can be tested with `cargo test`: this file is
//! included both by `build.rs` and by `tests/version_sync.rs`.
//!
//! Rules:
//! - The `+BUILD` counter is a local development aid only. It is shown only by debug
//!   builds of the program ([`program_version`]), never in release builds, `README.md`
//!   or `CHANGELOG.md`. A `+000` suffix is never shown.
//! - `README.md`: lines starting with one of [`README_PREFIXES`] get the version token
//!   after the prefix replaced with the base version ([`base_version`]).
//! - `CHANGELOG.md` has one section per `MAJOR.MINOR.PATCH`. Existing headers are never modified (renaming the topmost one would
//!   relabel the previous version's entries). If no section exists for the current base
//!   version, an empty `## [VERSION] - DATE` header is inserted below `## [Unreleased]`,
//!   above the previous version's section.
//! - Nothing is returned (and nothing written) when a file is already in sync.

/// Line prefixes in `README.md` that are followed by the version.
pub const README_PREFIXES: &[&str] = &["# shelfgauge v", "**Current version:** ", "_shelfgauge v"];

/// Name of the changelog section for changes not yet assigned to a version.
const UNRELEASED: &str = "Unreleased";

/// The version reported by the program (`--version`).
///
/// Release builds report the base version; debug builds keep a non-zero `+BUILD`
/// suffix (`0.1.0+003`) and omit `+000`.
pub fn program_version(version: &str, release: bool) -> &str {
    if release {
        base_version(version)
    } else {
        version.strip_suffix("+000").unwrap_or(version)
    }
}

/// The version without its `+BUILD` suffix (`0.1.0+013` → `0.1.0`).
pub fn base_version(version: &str) -> &str {
    version.split('+').next().unwrap_or(version)
}

/// Returns the updated `README.md` content, or `None` if it is already in sync.
pub fn sync_readme(content: &str, version: &str) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let (body, eol) = split_eol(line);
        match README_PREFIXES.iter().find(|p| body.starts_with(**p)) {
            Some(prefix) => {
                let rest = &body[prefix.len()..];
                let token_len = rest
                    .find(|c: char| !is_version_char(c))
                    .unwrap_or(rest.len());
                if &rest[..token_len] != version {
                    changed = true;
                }
                out.push_str(prefix);
                out.push_str(version);
                out.push_str(&rest[token_len..]);
                out.push_str(eol);
            }
            None => out.push_str(line),
        }
    }
    changed.then_some(out)
}

/// Returns the updated `CHANGELOG.md` content, or `None` if it already has a section
/// for `base_version`.
///
/// `date` is the `YYYY-MM-DD` date written into a newly inserted header.
pub fn sync_changelog(content: &str, base_version: &str, date: &str) -> Option<String> {
    let headers = || {
        content
            .split_inclusive('\n')
            .scan(0usize, |offset, line| {
                let start = *offset;
                *offset += line.len();
                Some((start, line))
            })
            .filter_map(|(start, line)| header_name(split_eol(line).0).map(|name| (start, name)))
    };
    if headers().any(|(_, name)| name == base_version) {
        return None;
    }

    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let header = format!("## [{base_version}] - {date}{eol}");
    let mut out = String::with_capacity(content.len() + header.len() + 2 * eol.len());
    match headers().find(|(_, name)| *name != UNRELEASED) {
        Some((start, _)) => {
            out.push_str(&content[..start]);
            out.push_str(&header);
            out.push_str(eol);
            out.push_str(&content[start..]);
        }
        None => {
            out.push_str(content);
            if !content.is_empty() && !content.ends_with('\n') {
                out.push_str(eol);
            }
            if !content.is_empty() && !content.ends_with("\n\n") && !content.ends_with("\n\r\n") {
                out.push_str(eol);
            }
            out.push_str(&header);
        }
    }
    Some(out)
}

/// Formats a number of days since the Unix epoch as a `YYYY-MM-DD` date (UTC).
pub fn date_from_unix_days(days: i64) -> String {
    // Howard Hinnant's `civil_from_days` algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Today's date (UTC) as `YYYY-MM-DD`.
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    date_from_unix_days((secs / 86_400) as i64)
}

/// The name inside a `## [NAME]` changelog header line, if `line` is one.
fn header_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("## [")?;
    rest.split_once(']').map(|(name, _)| name)
}

/// Splits a line into its body and its line ending (`""`, `"\n"` or `"\r\n"`).
fn split_eol(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

/// Characters that may appear in a SemVer version string.
fn is_version_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = "# shelfgauge v0.1.0\n\n\
                          Text mentioning shelfgauge v0.0.1 inline.\n\n\
                          **Current version:** 0.1.0\n\n\
                          ---\n\n\
                          _shelfgauge v0.1.0 — MIT License_\n";

    #[test]
    fn debug_program_version_keeps_only_a_nonzero_build() {
        assert_eq!(program_version("0.1.0", false), "0.1.0");
        assert_eq!(program_version("0.1.0+000", false), "0.1.0");
        assert_eq!(program_version("0.1.0+001", false), "0.1.0+001");
        assert_eq!(program_version("1.2.3+100", false), "1.2.3+100");
    }

    #[test]
    fn release_program_version_never_has_a_build() {
        assert_eq!(program_version("0.1.0", true), "0.1.0");
        assert_eq!(program_version("0.1.0+000", true), "0.1.0");
        assert_eq!(program_version("0.1.0+001", true), "0.1.0");
    }

    #[test]
    fn base_version_drops_the_build_counter() {
        assert_eq!(base_version("0.1.0+013"), "0.1.0");
        assert_eq!(base_version("0.1.0"), "0.1.0");
    }

    #[test]
    fn readme_in_sync_is_left_alone() {
        assert_eq!(sync_readme(README, "0.1.0"), None);
    }

    #[test]
    fn readme_updates_title_current_version_and_footer_only() {
        let updated = sync_readme(README, "0.1.0+002").unwrap();
        assert_eq!(
            updated,
            "# shelfgauge v0.1.0+002\n\n\
             Text mentioning shelfgauge v0.0.1 inline.\n\n\
             **Current version:** 0.1.0+002\n\n\
             ---\n\n\
             _shelfgauge v0.1.0+002 — MIT License_\n"
        );
        // A second sync with the same version is a no-op.
        assert_eq!(sync_readme(&updated, "0.1.0+002"), None);
    }

    #[test]
    fn readme_preserves_crlf_line_endings() {
        let crlf = README.replace('\n', "\r\n");
        assert_eq!(sync_readme(&crlf, "0.1.0"), None);
        let updated = sync_readme(&crlf, "0.2.0").unwrap();
        assert_eq!(
            updated,
            sync_readme(README, "0.2.0").unwrap().replace('\n', "\r\n")
        );
    }

    #[test]
    fn readme_without_version_lines_is_left_alone() {
        assert_eq!(sync_readme("# Other\n\nNo version here.\n", "0.1.0"), None);
    }

    #[test]
    fn changelog_with_current_section_is_left_alone_on_build_bump() {
        let content = "# Changelog\n\n## [Unreleased]\n\n- Pending.\n\n\
                       ## [0.1.0] - 2026-10-11\n\n### Added\n- Skeleton.\n";
        assert_eq!(
            sync_changelog(content, base_version("0.1.0+005"), "2026-12-01"),
            None
        );
    }

    #[test]
    fn changelog_new_version_is_inserted_below_unreleased_entries() {
        let content = "# Changelog\n\n\
                       ## [Unreleased]\n\n\
                       ### Changed\n\
                       - Entry that must stay under Unreleased.\n\n\
                       ## [0.2.0] - 2026-11-01\n\n\
                       ### Added\n\
                       - Current release notes.\n\n\
                       ## [0.1.0] - 2026-10-11\n\n\
                       ### Added\n\
                       - Old entry.\n";
        let updated = sync_changelog(content, "0.3.0", "2026-12-01").unwrap();
        assert_eq!(
            updated,
            "# Changelog\n\n\
             ## [Unreleased]\n\n\
             ### Changed\n\
             - Entry that must stay under Unreleased.\n\n\
             ## [0.3.0] - 2026-12-01\n\n\
             ## [0.2.0] - 2026-11-01\n\n\
             ### Added\n\
             - Current release notes.\n\n\
             ## [0.1.0] - 2026-10-11\n\n\
             ### Added\n\
             - Old entry.\n"
        );
        // Idempotent: the next build finds the section and writes nothing.
        assert_eq!(sync_changelog(&updated, "0.3.0", "2026-12-02"), None);
    }

    #[test]
    fn changelog_historical_section_is_never_renamed() {
        // An older section for the same base version counts as present, wherever it is.
        let content = "## [Unreleased]\n\n## [0.2.0] - 2026-11-01\n\n## [0.1.0] - 2026-10-11\n";
        assert_eq!(sync_changelog(content, "0.1.0", "2026-12-01"), None);
    }

    #[test]
    fn changelog_with_only_unreleased_gets_section_appended() {
        let content = "# Changelog\n\n## [Unreleased]\n\n- Pending.\n";
        assert_eq!(
            sync_changelog(content, "0.1.0", "2026-10-11").unwrap(),
            "# Changelog\n\n## [Unreleased]\n\n- Pending.\n\n## [0.1.0] - 2026-10-11\n"
        );
        assert_eq!(
            sync_changelog("## [Unreleased]", "0.1.0", "2026-10-11").unwrap(),
            "## [Unreleased]\n\n## [0.1.0] - 2026-10-11\n"
        );
    }

    #[test]
    fn changelog_insertion_uses_crlf_when_the_file_does() {
        let content = "## [Unreleased]\r\n\r\n## [0.1.0] - 2026-10-11\r\n";
        assert_eq!(
            sync_changelog(content, "0.2.0", "2026-11-01").unwrap(),
            "## [Unreleased]\r\n\r\n## [0.2.0] - 2026-11-01\r\n\r\n## [0.1.0] - 2026-10-11\r\n"
        );
    }

    #[test]
    fn dates_from_unix_days() {
        assert_eq!(date_from_unix_days(0), "1970-01-01");
        assert_eq!(date_from_unix_days(-1), "1969-12-31");
        assert_eq!(date_from_unix_days(19_782), "2024-02-29");
        assert_eq!(date_from_unix_days(20_737), "2026-10-11");
    }

    #[test]
    fn today_is_a_plausible_date() {
        let today = today_utc();
        assert_eq!(today.len(), 10);
        assert!(today.as_str() >= "2026-01-01", "{today}");
    }
}
