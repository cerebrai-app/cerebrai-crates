//! Download a [python-build-standalone](https://github.com/astral-sh/python-build-standalone)
//! distribution into the user's XDG data directory.
//!
//! Shared by the `brainctl` and `workctl` command line tools. Progress and
//! notes are written to stderr, and the outcome of [`install`] is printed to stdout.
//!
//! Layout: `${XDG_DATA_HOME}/python-build-standalone/<version>+<build options>/`
//! (e.g. `3.14.10+freethreaded+pgo+lto`), containing the archive's own
//! `python/` tree. If that directory already exists nothing is downloaded.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const LATEST_RELEASE_URL: &str = "https://raw.githubusercontent.com/astral-sh/python-build-standalone/latest-release/latest-release.json";
const RELEASE_DOWNLOAD_URL: &str =
    "https://github.com/astral-sh/python-build-standalone/releases/download";

/// The Python version installed when the caller does not ask for one.
pub const DEFAULT_PYTHON_VERSION: &str = "3.14";
/// The build options preferred when [`Options::variant`] is `None`.
pub const DEFAULT_VARIANT: &str = "freethreaded+pgo+lto";

/// Tried in order, after `DEFAULT_VARIANT`, when `Options::variant` is `None` and
/// the default is unavailable for this platform (e.g. Windows has no LTO
/// builds). Compiler optimizations are preferred over free-threading.
const FALLBACK_VARIANTS: &[&str] = &[
    "pgo+lto",
    "freethreaded+pgo",
    "pgo",
    "freethreaded+lto",
    "lto",
    "freethreaded+noopt",
    "noopt",
];

/// What [`install`] should install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// A Python version: `3` (newest 3.x), `3.14` (newest 3.14.x) or `3.14.15`
    /// (exact). Not a python-build-standalone release date.
    pub python_version: String,
    /// Exact build options, e.g. `freethreaded+pgo+lto`. `None` means the
    /// default with fallbacks.
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Asset {
    file_name: String,
    sha256: String,
    version: Vec<u32>,
    build_options: String,
}

impl Asset {
    fn dir_name(&self) -> String {
        let v: Vec<String> = self.version.iter().map(u32::to_string).collect();
        format!("{}+{}", v.join("."), self.build_options)
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            python_version: DEFAULT_PYTHON_VERSION.to_string(),
            variant: None,
        }
    }
}

/// Install the newest distribution matching `opts` and return its directory,
/// `<data dir>/python-build-standalone/<version>+<build options>`, which holds
/// the archive's `python/` tree.
///
/// Nothing is downloaded if that directory already exists, but the latest
/// release is looked up on every call, so this always needs network access.
/// An invalid [`Options::python_version`] is rejected before anything touches
/// the network or the file system.
pub fn install(opts: &Options) -> Result<PathBuf> {
    // Checked first, so a bad value fails before anything touches the network.
    validate_python_version(&opts.python_version)?;
    let root = data_home_from(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("USERPROFILE"),
        cfg!(windows),
    )?
    .join("python-build-standalone");
    let triple = host_triple()?;
    let agent = agent();

    let tag = latest_tag(&agent)?;
    let sums_url = format!("{RELEASE_DOWNLOAD_URL}/{tag}/SHA256SUMS");
    let sums = http_text(&agent, &sums_url)?;
    let assets = parse_sums(&sums, triple);

    let variants: Vec<String> = match &opts.variant {
        Some(v) => vec![v.clone()],
        None => default_variants(),
    };
    let Some(asset) = select(&assets, &opts.python_version, &variants) else {
        bail!(
            "no python-build-standalone {tag} distribution for Python {} ({}) on {triple}.\n\
             available build options for that version: {}",
            opts.python_version,
            variants.join(", "),
            available(&assets, &opts.python_version)
        );
    };
    if opts.variant.is_none() && asset.build_options != DEFAULT_VARIANT {
        eprintln!(
            "note: {DEFAULT_VARIANT} is not available for {triple}; using {}",
            asset.build_options
        );
    }
    if let Some(note) = newer_version_note(&assets, &opts.python_version, asset) {
        eprintln!("{note}");
    }

    let dest = root.join(asset.dir_name());
    if dest.exists() {
        println!("already installed: {}", dest.display());
        return Ok(dest);
    }

    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    sweep_stale_temps(&root, STALE_TEMP_AGE);
    // Temp names include the process id and the start time, so concurrent runs
    // of the same build don't delete each other's files even when two
    // containers sharing one data directory happen to have the same pid.
    let unique = unique_suffix();
    let archive = root.join(format!("{}.{unique}.download", asset.dir_name()));
    let partial = root.join(format!("{}.{unique}.partial", asset.dir_name()));
    let result = download_and_unpack(&agent, &tag, asset, &archive, &partial, &dest);
    // Best-effort cleanup of temporaries, whether or not we succeeded.
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&partial);
    if result? {
        println!("installed: {}", dest.display());
    }
    Ok(dest)
}

/// Renaming the unpacked tree into place is retried this many times, this long
/// apart, while the destination still does not exist.
const RENAME_ATTEMPTS: u32 = 5;
const RENAME_DELAY: Duration = Duration::from_millis(200);

/// Leftover temp files older than this are removed at the start of a run.
const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// `fs::rename`, retried while `to` still does not exist. On Windows a virus
/// scanner often holds files in a freshly unpacked tree, which makes the rename
/// fail with "Access is denied" for a moment. If `to` has appeared (another
/// process won the race) the error is returned at once for the caller to handle.
fn rename_retrying(from: &Path, to: &Path, attempts: u32, delay: Duration) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(err) if to.exists() || attempt >= attempts => return Err(err),
            Err(_) => {
                std::thread::sleep(delay);
                attempt += 1;
            }
        }
    }
}

/// Remove `*.download` / `*.partial` entries in `root` not modified for
/// `max_age`: leftovers from a run that was killed (e.g. Ctrl-C). Recent ones
/// may belong to a run in progress, so they are left alone. Best effort.
fn sweep_stale_temps(root: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".download") || name.ends_with(".partial")) {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok());
        if age.is_some_and(|a| a >= max_age) {
            let path = entry.path();
            let _ = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
        }
    }
}

/// The default build followed by the fallbacks, in preference order.
fn default_variants() -> Vec<String> {
    std::iter::once(DEFAULT_VARIANT)
        .chain(FALLBACK_VARIANTS.iter().copied())
        .map(String::from)
        .collect()
}

/// Download, verify and unpack into `dest`. Returns `false` if another process
/// finished the same install first.
fn download_and_unpack(
    agent: &ureq::Agent,
    tag: &str,
    asset: &Asset,
    archive: &Path,
    partial: &Path,
    dest: &Path,
) -> Result<bool> {
    let _ = fs::remove_dir_all(partial);
    // '+' must be percent-encoded in the URL path.
    let url = format!(
        "{RELEASE_DOWNLOAD_URL}/{tag}/{}",
        asset.file_name.replace('+', "%2B")
    );
    eprintln!("downloading {}", asset.file_name);
    download(agent, &url, archive, &asset.sha256)?;

    eprintln!("extracting to {}", dest.display());
    unpack_and_publish(archive, partial, dest)
}

/// Unpack the `.tar.zst` `archive` into `partial`, then rename it to `dest`.
/// Returns `false` if `dest` appeared in the meantime (another process won the
/// race), leaving `partial` for the caller to clean up.
fn unpack_and_publish(archive: &Path, partial: &Path, dest: &Path) -> Result<bool> {
    let file = File::open(archive)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.unpack(partial).context("extracting archive")?;
    if let Err(err) = rename_retrying(partial, dest, RENAME_ATTEMPTS, RENAME_DELAY) {
        // Another process may have finished the same install first.
        if dest.exists() {
            println!("already installed by another process: {}", dest.display());
            return Ok(false);
        }
        return Err(err).with_context(|| format!("moving into {}", dest.display()));
    }
    Ok(true)
}

fn download(agent: &ureq::Agent, url: &str, out: &Path, expected_sha256: &str) -> Result<()> {
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let total = resp.body().content_length();
    let reader = resp.body_mut().as_reader();
    let mut file = File::create(out)?;
    copy_verified(reader, &mut file, total, expected_sha256)
        .with_context(|| format!("downloading {url}"))
}

/// Copy `reader` to `writer`, then check the SHA-256 of what was copied
/// against `expected` (hex, case-insensitive). This is the only integrity
/// check on the downloaded archive. `total`, if known, drives progress output.
fn copy_verified(
    mut reader: impl Read,
    mut writer: impl Write,
    total: Option<u64>,
    expected: &str,
) -> Result<()> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let (mut done, mut last_pct) = (0u64, 0u64);
    loop {
        let n = reader.read(&mut buf).context("reading download")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
        done += n as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            let pct = done * 100 / total / 10 * 10;
            if pct > last_pct {
                eprintln!("  {pct}%");
                last_pct = pct;
            }
        }
    }
    writer.flush()?;
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn agent() -> ureq::Agent {
    // Use the OS trust store so corporate/proxy CAs work, and pick up
    // HTTPS_PROXY from the environment (ureq's default).
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    // No total timeout: the archive is large. `timeout_per_read` restarts on
    // every read, so it only fires when the connection stalls.
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn http_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

fn latest_tag(agent: &ureq::Agent) -> Result<String> {
    tag_from_latest_release(&http_text(agent, LATEST_RELEASE_URL)?)
}

/// The release tag in `latest-release.json`. It ends up in a URL, so it is
/// checked by `validate_tag`.
fn tag_from_latest_release(body: &str) -> Result<String> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("parsing latest-release.json")?;
    let tag = json
        .get("tag")
        .and_then(|t| t.as_str())
        .context("latest-release.json has no \"tag\"")?;
    Ok(validate_tag(tag)?.to_string())
}

/// Accepts `3`, `3.14` or `3.14.15`: up to three numbers of one to three digits.
/// That keeps a python-build-standalone release date such as `20261111` (which
/// would otherwise parse as a huge major version) from being taken as one.
fn validate_python_version(v: &str) -> Result<()> {
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
fn validate_tag(tag: &str) -> Result<&str> {
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

fn host_triple() -> Result<&'static str> {
    let t = if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "gnu"
    )) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else {
        bail!("unsupported platform for python-build-standalone");
    };
    Ok(t)
}

/// Where per-user data lives:
/// 1. `$XDG_DATA_HOME`, if set to an absolute path (honoured on every platform);
/// 2. Windows: `%LOCALAPPDATA%` (falling back to `%USERPROFILE%\AppData\Local`).
///    Local rather than roaming, since this is large and machine-specific;
/// 3. otherwise `$HOME/.local/share`.
fn data_home_from(
    xdg: Option<OsString>,
    home: Option<OsString>,
    local_app_data: Option<OsString>,
    user_profile: Option<OsString>,
    windows: bool,
) -> Result<PathBuf> {
    if let Some(p) = xdg.filter(|p| Path::new(p).is_absolute()) {
        return Ok(p.into());
    }
    if windows {
        if let Some(p) = local_app_data.filter(|p| !p.is_empty()) {
            return Ok(p.into());
        }
        if let Some(p) = user_profile.as_ref().filter(|p| !p.is_empty()) {
            return Ok(PathBuf::from(p).join("AppData").join("Local"));
        }
    }
    let home = home
        .filter(|h| !h.is_empty())
        .or(user_profile.filter(|h| !h.is_empty()))
        .context("cannot determine the data directory: set XDG_DATA_HOME or HOME")?;
    Ok(PathBuf::from(home).join(".local").join("share"))
}

/// Parse a release `SHA256SUMS` file, keeping `full` archives for `triple`
/// with a plain numeric CPython version (pre-releases are skipped).
fn parse_sums(sums: &str, triple: &str) -> Vec<Asset> {
    sums.lines().filter_map(|l| parse_line(l, triple)).collect()
}

fn parse_line(line: &str, triple: &str) -> Option<Asset> {
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
fn parse_version(s: &str) -> Option<Vec<u32>> {
    s.split('.').map(|p| p.parse::<u32>().ok()).collect()
}

fn version_matches(version: &[u32], want: &str) -> bool {
    parse_version(want).is_some_and(|w| version.starts_with(&w))
}

/// `<pid>-<nanoseconds since the epoch>`.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// The preferred build wins over a newer version (`select` below). When that
/// means skipping a newer matching version that only exists as another build,
/// say so, so asking for Python `3` never quietly gives an older Python.
fn newer_version_note(assets: &[Asset], want: &str, chosen: &Asset) -> Option<String> {
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
fn select<'a>(assets: &'a [Asset], want: &str, variants: &[String]) -> Option<&'a Asset> {
    variants.iter().find_map(|v| {
        assets
            .iter()
            .filter(|a| &a.build_options == v && version_matches(&a.version, want))
            .max_by(|a, b| a.version.cmp(&b.version))
    })
}

fn available(assets: &[Asset], want: &str) -> String {
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

    fn os(p: &Path) -> Option<OsString> {
        Some(p.as_os_str().to_owned())
    }

    // Paths come from temp_dir() so they are absolute on every platform
    // (a bare "/x/data" has no drive letter on Windows).
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
    fn data_home_prefers_absolute_xdg_everywhere() {
        let xdg = std::env::temp_dir().join("xdg-data");
        let other = std::env::temp_dir().join("other");
        for windows in [false, true] {
            let p = data_home_from(os(&xdg), os(&other), os(&other), os(&other), windows).unwrap();
            assert_eq!(p, xdg);
        }
    }

    // sha256("hello")
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn copy_verified_accepts_matching_hash() {
        let mut out = Vec::new();
        copy_verified(&b"hello"[..], &mut out, Some(5), HELLO_SHA256).unwrap();
        assert_eq!(out, b"hello");
        // Hex case does not matter.
        copy_verified(
            &b"hello"[..],
            Vec::new(),
            None,
            &HELLO_SHA256.to_uppercase(),
        )
        .unwrap();
    }

    #[test]
    fn copy_verified_rejects_wrong_hash_and_tampered_data() {
        let wrong = "0".repeat(64);
        let err = copy_verified(&b"hello"[..], Vec::new(), None, &wrong).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        let err = copy_verified(&b"hellO"[..], Vec::new(), None, HELLO_SHA256).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        // Empty input must not slip through either.
        assert!(copy_verified(&b""[..], Vec::new(), None, HELLO_SHA256).is_err());
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
    fn unpack_and_publish_installs_once_and_reports_lost_race() {
        let dir = std::env::temp_dir().join(format!("pbs-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // A tiny archive shaped like the real ones: python/PYTHON.json.
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "python/PYTHON.json", &b"hi"[..])
                .unwrap();
            b.finish().unwrap();
        }
        let archive = dir.join("a.tar.zst");
        fs::write(&archive, zstd::encode_all(&tar_bytes[..], 0).unwrap()).unwrap();

        let dest = dir.join("3.14.1+x");
        let partial = dir.join("3.14.1+x.1.partial");
        assert!(unpack_and_publish(&archive, &partial, &dest).unwrap());
        assert_eq!(fs::read(dest.join("python/PYTHON.json")).unwrap(), b"hi");
        assert!(!partial.exists(), "partial dir is renamed away");

        // Second run: dest now exists, so this one lost the race.
        let partial2 = dir.join("3.14.1+x.2.partial");
        assert!(!unpack_and_publish(&archive, &partial2, &dest).unwrap());
        assert_eq!(fs::read(dest.join("python/PYTHON.json")).unwrap(), b"hi");

        // A corrupt archive is an error and installs nothing.
        let bad = dir.join("bad.tar.zst");
        fs::write(&bad, b"not a zstd file").unwrap();
        let dest2 = dir.join("3.14.2+x");
        assert!(unpack_and_publish(&bad, &dir.join("3.14.2+x.3.partial"), &dest2).is_err());
        assert!(!dest2.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn latest_release_tag_is_validated() {
        assert_eq!(
            tag_from_latest_release(r#"{"version":1,"tag":"20261026"}"#).unwrap(),
            "20261026"
        );
        for bad in [
            r#"{"tag":"../x"}"#,
            r#"{"tag":"a/b"}"#,
            r#"{"tag":""}"#,
            r#"{"tag":42}"#,
            r#"{"version":1}"#,
            "not json",
        ] {
            assert!(tag_from_latest_release(bad).is_err(), "{bad}");
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
    fn rename_retrying_moves_retries_and_gives_up() {
        use std::time::Instant;
        let dir = std::env::temp_dir().join(format!("pbs-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();

        // Plain success.
        let dest = dir.join("dest");
        rename_retrying(&dir.join("src"), &dest, 3, Duration::ZERO).unwrap();
        assert!(dest.is_dir() && !dir.join("src").exists());

        // Destination exists (lost race): the error comes back at once, with no
        // sleeping, so the caller can report "already installed".
        fs::create_dir_all(dir.join("src2/sub")).unwrap();
        fs::create_dir_all(dest.join("sub")).unwrap();
        let start = Instant::now();
        assert!(rename_retrying(&dir.join("src2"), &dest, 5, Duration::from_secs(5)).is_err());
        assert!(start.elapsed() < Duration::from_secs(2), "must not retry");

        // Still failing and the destination never appears: tries every attempt
        // (two sleeps for three attempts), then returns the error.
        let start = Instant::now();
        let missing = dir.join("does-not-exist");
        assert!(
            rename_retrying(&missing, &dir.join("never"), 3, Duration::from_millis(60)).is_err()
        );
        assert!(start.elapsed() >= Duration::from_millis(120), "must retry");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unpack_refuses_entries_that_escape_the_install_dir() {
        let dir = std::env::temp_dir().join(format!("pbs-escape-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();

        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            // 1. A path that climbs out with `..`. The Builder refuses to write
            //    such a name, so put it into the header bytes by hand.
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            let name = b"../evil.txt";
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_cksum();
            b.append(&h, &b"evil"[..]).unwrap();
            // 2. A symlink out of the tree, then a file written through it.
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_path("link").unwrap();
            h.set_link_name(&outside).unwrap();
            h.set_cksum();
            b.append(&h, std::io::empty()).unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_path("link/pwned.txt").unwrap();
            h.set_cksum();
            b.append(&h, &b"evil"[..]).unwrap();
            b.finish().unwrap();
        }
        // The archive really holds the hostile entries (otherwise this test
        // would pass without testing anything).
        let names: Vec<Vec<u8>> = tar::Archive::new(&tar_bytes[..])
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path_bytes().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                b"../evil.txt".to_vec(),
                b"link".to_vec(),
                b"link/pwned.txt".to_vec()
            ]
        );
        let archive = dir.join("evil.tar.zst");
        fs::write(&archive, zstd::encode_all(&tar_bytes[..], 0).unwrap()).unwrap();

        // The result (an error, or a skipped entry) does not matter. What matters
        // is that nothing was written outside the unpack directory.
        let partial = dir.join("work").join("x.partial");
        fs::create_dir_all(dir.join("work")).unwrap();
        let _ = unpack_and_publish(&archive, &partial, &dir.join("work").join("dest"));
        assert!(!dir.join("work").join("evil.txt").exists(), "`..` escaped");
        assert!(!dir.join("evil.txt").exists(), "`..` escaped");
        assert!(!outside.join("pwned.txt").exists(), "symlink escaped");
        fs::remove_dir_all(&dir).unwrap();
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

    #[test]
    fn unique_suffix_differs_between_calls() {
        let (a, b) = (unique_suffix(), unique_suffix());
        assert!(a.starts_with(&format!("{}-", std::process::id())), "{a}");
        assert_ne!(a, b);
    }

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

    #[test]
    fn sweep_removes_only_old_temp_entries() {
        let root = std::env::temp_dir().join(format!("pbs-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("3.14.1+x.99.partial")).unwrap();
        fs::write(root.join("3.14.1+x.99.download"), b"x").unwrap();
        fs::create_dir_all(root.join("3.14.1+installed")).unwrap();

        // Nothing is older than a day yet.
        sweep_stale_temps(&root, STALE_TEMP_AGE);
        assert!(root.join("3.14.1+x.99.partial").exists());
        assert!(root.join("3.14.1+x.99.download").exists());

        // With a zero max age, temp entries go but real installs stay.
        sweep_stale_temps(&root, Duration::ZERO);
        assert!(!root.join("3.14.1+x.99.partial").exists());
        assert!(!root.join("3.14.1+x.99.download").exists());
        assert!(root.join("3.14.1+installed").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn data_home_unix_falls_back_to_local_share() {
        let home = std::env::temp_dir().join("home-u");
        let expected = home.join(".local").join("share");
        // Relative XDG_DATA_HOME is invalid per the spec and ignored.
        let p = data_home_from(Some("relative".into()), os(&home), None, None, false).unwrap();
        assert_eq!(p, expected);
        let p = data_home_from(None, os(&home), None, None, false).unwrap();
        assert_eq!(p, expected);
        assert!(data_home_from(None, None, None, None, false).is_err());
        // An empty HOME must not turn into a relative ".local/share".
        assert!(data_home_from(None, Some("".into()), None, None, false).is_err());
        let p = data_home_from(None, Some("".into()), None, os(&home), false).unwrap();
        assert_eq!(p, expected);
    }

    #[test]
    fn data_home_windows_uses_local_app_data() {
        let local = std::env::temp_dir().join("Local");
        let profile = std::env::temp_dir().join("profile");
        let p = data_home_from(None, None, os(&local), os(&profile), true).unwrap();
        assert_eq!(p, local);
        // No LOCALAPPDATA: derive it from the user profile.
        let p = data_home_from(None, None, None, os(&profile), true).unwrap();
        assert_eq!(p, profile.join("AppData").join("Local"));
        // LOCALAPPDATA is ignored off Windows.
        let p = data_home_from(None, os(&profile), os(&local), None, false).unwrap();
        assert_eq!(p, profile.join(".local").join("share"));
    }
}
