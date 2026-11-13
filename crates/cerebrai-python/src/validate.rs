//! Checks on values that end up in URLs and paths.

use anyhow::{Result, bail};

/// Accepts `3`, `3.14` or `3.14.15`: up to three numbers of one to three digits.
/// That keeps a python-build-standalone release date such as `20261111` (which
/// would otherwise parse as a huge major version) from being taken as one.
pub(crate) fn validate_python_version(v: &str) -> Result<()> {
    let parts: Vec<&str> = v.split('.').collect();
    let ok = parts.len() <= 3
        && parts
            .iter()
            .all(|p| (1..=3).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
    if !ok {
        bail!(
            "invalid Python version {v:?}: use 3, 3.14 or 3.14.15 \
             (this is a Python version, not a python-build-standalone release)"
        );
    }
    Ok(())
}

/// The release tag from `latest-release.json` is a date like `20261026`; allow
/// only characters that are safe to put in a URL path, and nothing that could
/// climb out of it.
pub(crate) fn validate_tag(tag: &str) -> Result<&str> {
    let ok = !tag.is_empty()
        && !tag.starts_with('.')
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !ok {
        bail!("invalid release tag {tag:?}: use letters, digits, '.', '-' and '_' only");
    }
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_version_accepts_versions_and_rejects_release_dates() {
        for ok in ["3", "3.14", "3.14.15", "3.9", "3.9.1", "10.0"] {
            assert!(validate_python_version(ok).is_ok(), "{ok}");
        }
        for bad in [
            "20261111", // a release date, not a Python version
            "",
            "3.",
            ".3",
            "3..1",
            "3.14.15.1",
            "3.x",
            "v3.14",
            "3.14.15rc1",
            "-1",
            "3 ",
            "1000",
            "3.14.1000",
        ] {
            let err = validate_python_version(bad).unwrap_err().to_string();
            assert!(err.contains("invalid Python version"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn release_tag_validation() {
        assert_eq!(validate_tag("20261026").unwrap(), "20261026");
        assert!(validate_tag("v1.2-rc_1").is_ok());
        for bad in [
            "", "..", ".hidden", "../x", "a/b", "a\\b", "a b", "a?b", "a#b",
        ] {
            assert!(validate_tag(bad).is_err(), "{bad:?}");
        }
    }
}
