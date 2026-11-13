//! Parsing the release `SHA256SUMS` file and choosing which build to install.

use crate::DEFAULT_VARIANT;

/// Tried in order, after `DEFAULT_VARIANT`, when `Options::variant` is `None` and
/// the default is unavailable for this platform (e.g. Windows has no LTO
/// builds). Compiler optimizations are preferred over free-threading.
pub(crate) const FALLBACK_VARIANTS: &[&str] = &[
    "pgo+lto",
    "freethreaded+pgo",
    "pgo",
    "freethreaded+lto",
    "lto",
    "freethreaded+noopt",
    "noopt",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asset {
    pub(crate) file_name: String,
    pub(crate) sha256: String,
    pub(crate) version: Vec<u32>,
    pub(crate) build_options: String,
}

impl Asset {
    pub(crate) fn dir_name(&self) -> String {
        let v: Vec<String> = self.version.iter().map(u32::to_string).collect();
        format!("{}+{}", v.join("."), self.build_options)
    }
}

/// The default build followed by the fallbacks, in preference order.
pub(crate) fn default_variants() -> Vec<String> {
    std::iter::once(DEFAULT_VARIANT)
        .chain(FALLBACK_VARIANTS.iter().copied())
        .map(String::from)
        .collect()
}

/// Parse a release `SHA256SUMS` file, keeping `full` archives for `triple`
/// with a plain numeric CPython version (pre-releases are skipped).
pub(crate) fn parse_sums(sums: &str, triple: &str) -> Vec<Asset> {
    sums.lines().filter_map(|l| parse_line(l, triple)).collect()
}

pub(crate) fn parse_line(line: &str, triple: &str) -> Option<Asset> {
    let (hash, name) = line.trim().split_once(char::is_whitespace)?;
    let name = name.trim().trim_start_matches('*');
    // Names come from a remote file and end up in paths and URLs.
    if name.contains(['/', '\\']) {
        return None;
    }
    // cpython-<version>+<tag>-<triple>-<build options>-full.tar.zst
    let rest = name.strip_prefix("cpython-")?;
    let (version, rest) = rest.split_once('+')?;
    let (_tag, rest) = rest.split_once('-')?;
    let rest = rest.strip_prefix(triple)?.strip_prefix('-')?;
    let build_options = rest.strip_suffix("-full.tar.zst")?;
    let version = parse_version(version)?;
    Some(Asset {
        file_name: name.to_string(),
        sha256: hash.to_string(),
        version,
        build_options: build_options.to_string(),
    })
}

/// `"3.14.10"` -> `[3, 14, 10]`; `None` if any part is not a plain number.
pub(crate) fn parse_version(s: &str) -> Option<Vec<u32>> {
    s.split('.').map(|p| p.parse::<u32>().ok()).collect()
}

pub(crate) fn version_matches(version: &[u32], want: &str) -> bool {
    parse_version(want).is_some_and(|w| version.starts_with(&w))
}

/// The preferred build wins over a newer version (`select` below). When that
/// means skipping a newer matching version that only exists as another build,
/// say so, so asking for Python `3` never quietly gives an older Python.
pub(crate) fn newer_version_note(assets: &[Asset], want: &str, chosen: &Asset) -> Option<String> {
    let newest = assets
        .iter()
        .filter(|a| version_matches(&a.version, want))
        .map(|a| &a.version)
        .max()?;
    if *newest <= chosen.version {
        return None;
    }
    let mut builds: Vec<&str> = assets
        .iter()
        .filter(|a| &a.version == newest)
        .map(|a| a.build_options.as_str())
        .collect();
    builds.sort();
    let newest: Vec<String> = newest.iter().map(u32::to_string).collect();
    Some(format!(
        "note: Python {} exists only as {}; using {} with {}",
        newest.join("."),
        builds.join(", "),
        chosen.dir_name().split('+').next().unwrap_or_default(),
        chosen.build_options
    ))
}

/// First variant (in preference order) with any matching asset wins; within
/// it, the highest version.
pub(crate) fn select<'a>(
    assets: &'a [Asset],
    want: &str,
    variants: &[String],
) -> Option<&'a Asset> {
    variants.iter().find_map(|v| {
        assets
            .iter()
            .filter(|a| &a.build_options == v && version_matches(&a.version, want))
            .max_by(|a, b| a.version.cmp(&b.version))
    })
}

pub(crate) fn available(assets: &[Asset], want: &str) -> String {
    let mut v: Vec<&str> = assets
        .iter()
        .filter(|a| version_matches(&a.version, want))
        .map(|a| a.build_options.as_str())
        .collect();
    v.sort();
    v.dedup();
    if v.is_empty() {
        "none (is the Python version right? pre-releases such as 3.15.0a1 are not offered)".into()
    } else {
        v.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LNX: &str = "x86_64-unknown-linux-gnu";
    const SUMS: &str = "\
aaa  cpython-3.14.9+20261001-x86_64-unknown-linux-gnu-freethreaded+pgo+lto-full.tar.zst
bbb  cpython-3.14.10+20261026-x86_64-unknown-linux-gnu-freethreaded+pgo+lto-full.tar.zst
ccc  cpython-3.14.10+20261026-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst
ddd  cpython-3.14.10+20261026-x86_64-unknown-linux-gnu-freethreaded-install_only.tar.gz
eee  cpython-3.15.0a1+20261026-x86_64-unknown-linux-gnu-freethreaded+pgo+lto-full.tar.zst
fff  cpython-3.14.10+20261026-aarch64-unknown-linux-gnu-freethreaded+pgo+lto-full.tar.zst
ggg  cpython-3.13.9+20261026-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst
";

    fn variants(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_only_full_archives_for_triple() {
        let assets = parse_sums(SUMS, LNX);
        // install_only, aarch64 and the 3.15.0a1 pre-release are skipped.
        assert_eq!(assets.len(), 4);
        assert_eq!(assets[1].version, vec![3, 14, 10]);
        assert_eq!(assets[1].build_options, "freethreaded+pgo+lto");
        assert_eq!(assets[1].sha256, "bbb");
    }

    #[test]
    fn picks_newest_patch_for_version_prefix() {
        let assets = parse_sums(SUMS, LNX);
        let a = select(&assets, "3.14", &variants(&[DEFAULT_VARIANT])).unwrap();
        assert_eq!(a.version, vec![3, 14, 10]);
        assert_eq!(a.dir_name(), "3.14.10+freethreaded+pgo+lto");
    }

    #[test]
    fn exact_version_and_no_partial_component_match() {
        let assets = parse_sums(SUMS, LNX);
        let a = select(&assets, "3.14.9", &variants(&[DEFAULT_VARIANT])).unwrap();
        assert_eq!(a.sha256, "aaa");
        // "3.1" must not match 3.14.x.
        assert!(select(&assets, "3.1", &variants(&[DEFAULT_VARIANT])).is_none());
    }

    #[test]
    fn falls_back_in_preference_order() {
        let assets = parse_sums(SUMS, LNX);
        // 3.13 has no free-threaded build in the sample, so pgo+lto is used.
        let v = variants(&[DEFAULT_VARIANT, "freethreaded+pgo", "pgo+lto"]);
        let a = select(&assets, "3.13", &v).unwrap();
        assert_eq!(a.build_options, "pgo+lto");
        assert!(select(&assets, "3.13", &variants(&[DEFAULT_VARIANT])).is_none());
    }

    #[test]
    fn default_chain_prefers_optimizations_over_freethreaded() {
        assert_eq!(
            default_variants(),
            variants(&[
                "freethreaded+pgo+lto",
                "pgo+lto",
                "freethreaded+pgo",
                "pgo",
                "freethreaded+lto",
                "lto",
                "freethreaded+noopt",
                "noopt",
            ])
        );
        // Windows-like: no LTO and a free-threaded PGO build exists. The
        // non-free-threaded pgo+lto would win if present; here only
        // freethreaded+pgo and pgo exist, so freethreaded+pgo is chosen.
        let sums = "\
a  cpython-3.14.10+1-x86_64-pc-windows-msvc-freethreaded+pgo-full.tar.zst
b  cpython-3.14.10+1-x86_64-pc-windows-msvc-pgo-full.tar.zst
c  cpython-3.14.10+1-x86_64-pc-windows-msvc-noopt-full.tar.zst
";
        let assets = parse_sums(sums, "x86_64-pc-windows-msvc");
        let a = select(&assets, "3.14", &default_variants()).unwrap();
        assert_eq!(a.build_options, "freethreaded+pgo");
        // Without it, plain pgo beats freethreaded+noopt/noopt.
        let assets = parse_sums(
            &sums.replace("freethreaded+pgo", "x"),
            "x86_64-pc-windows-msvc",
        );
        let a = select(&assets, "3.14", &default_variants()).unwrap();
        assert_eq!(a.build_options, "pgo");
    }

    #[test]
    fn rejects_asset_names_with_path_separators() {
        let ok = "aaa  cpython-3.14.10+1-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst";
        assert!(parse_line(ok, LNX).is_some());
        for bad in [
            "aaa  cpython-3.14.10+../../x-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst",
            "aaa  cpython-3.14.10+1-x86_64-unknown-linux-gnu-a/b-full.tar.zst",
            "aaa  cpython-3.14.10+1-x86_64-unknown-linux-gnu-a\\b-full.tar.zst",
        ] {
            assert!(parse_line(bad, LNX).is_none(), "{bad}");
        }
    }

    #[test]
    fn parse_version_accepts_only_plain_numbers() {
        assert_eq!(parse_version("3.14.10"), Some(vec![3, 14, 10]));
        assert_eq!(parse_version("3"), Some(vec![3]));
        for bad in ["", "3.", ".3", "3.x", "3.15.0a1", "v3.14"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert!(!version_matches(&[3, 14, 10], ""));
    }

    #[test]
    fn preferred_build_beats_a_newer_version_and_says_so() {
        let sums = "\
a  cpython-3.15.1+1-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst
b  cpython-3.15.1+1-x86_64-unknown-linux-gnu-pgo-full.tar.zst
c  cpython-3.14.10+1-x86_64-unknown-linux-gnu-freethreaded+pgo+lto-full.tar.zst
d  cpython-3.14.10+1-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst
";
        let assets = parse_sums(sums, LNX);

        // "3": 3.15.1 has no free-threaded build, 3.14.10 does, so the preferred
        // build wins and the user is told about the newer version.
        let a = select(&assets, "3", &default_variants()).unwrap();
        assert_eq!(
            (a.version.clone(), a.build_options.as_str()),
            (vec![3, 14, 10], "freethreaded+pgo+lto")
        );
        let note = newer_version_note(&assets, "3", a).unwrap();
        assert!(
            note.contains("3.15.1") && note.contains("pgo, pgo+lto"),
            "{note}"
        );
        assert!(
            note.contains("3.14.10") && note.contains("freethreaded+pgo+lto"),
            "{note}"
        );

        // "3.14": the newest 3.14.x already has the preferred build: no note.
        let a = select(&assets, "3.14", &default_variants()).unwrap();
        assert_eq!(a.version, vec![3, 14, 10]);
        assert_eq!(newer_version_note(&assets, "3.14", a), None);

        // "3.15": only the fallback builds exist, and the newest is chosen: no note.
        let a = select(&assets, "3.15", &default_variants()).unwrap();
        assert_eq!(
            (a.version.clone(), a.build_options.as_str()),
            (vec![3, 15, 1], "pgo+lto")
        );
        assert_eq!(newer_version_note(&assets, "3.15", a), None);
    }
}
