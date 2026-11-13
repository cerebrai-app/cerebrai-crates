//! Download a [python-build-standalone](https://github.com/astral-sh/python-build-standalone)
//! distribution into the user's XDG data directory.
//!
//! Shared by the `brainctl` and `workctl` command line tools. Progress and
//! notes are written to stderr unless [`Options::quiet`] is set; nothing is
//! written to stdout, so callers keep it for their own output. [`install`]
//! returns the install directory.
//!
//! ```no_run
//! let dir = cerebrai_python::install(&cerebrai_python::Options::new("3.14").quiet(true))?;
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! Layout: `${XDG_DATA_HOME}/python-build-standalone/<version>+<build options>/`
//! (e.g. `3.14.10+freethreaded+pgo+lto`), containing the archive's own
//! `python/` tree. If that directory already exists nothing is downloaded.
//!
//! Each archive is checked against the release's `SHA256SUMS`. That catches
//! corrupt or truncated downloads, but the sums come from the same GitHub
//! release as the archive, so it does not protect against a compromised release.

mod assets;
mod net;
mod platform;
mod store;
mod validate;

use anyhow::{Context, Result, bail};
use assets::{Asset, available, default_variants, newer_version_note, parse_sums, select};
use net::{LATEST_RELEASE_URL, RELEASE_DOWNLOAD_URL, agent, download, http_text, latest_tag};
use platform::{data_home_from, host_triple};
use std::fs;
use std::path::{Path, PathBuf};
use store::{STALE_TEMP_AGE, sweep_stale_temps, unique_suffix, unpack_and_publish};
use validate::validate_python_version;

/// The Python version installed when the caller does not ask for one.
pub const DEFAULT_PYTHON_VERSION: &str = "3.14";
/// The build options preferred when [`Options::variant`] is `None`.
pub const DEFAULT_VARIANT: &str = "freethreaded+pgo+lto";

/// What [`install`] should install.
///
/// Build it with [`Options::new`] (or [`Default`]) and the setters. It is
/// `#[non_exhaustive]`, so adding a field later does not break callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Options {
    /// A Python version: `3` (newest 3.x), `3.14` (newest 3.14.x) or `3.14.15`
    /// (exact). Not a python-build-standalone release date.
    pub python_version: String,
    /// Exact build options, e.g. `freethreaded+pgo+lto`. `None` means the
    /// default with fallbacks.
    pub variant: Option<String>,
    /// Print nothing to stderr (no progress, notes or result lines).
    pub quiet: bool,
}

impl Options {
    /// Install `python_version` (see [`Options::python_version`]) with the
    /// default build options, printing progress to stderr.
    pub fn new(python_version: impl Into<String>) -> Self {
        Self {
            python_version: python_version.into(),
            variant: None,
            quiet: false,
        }
    }

    /// Require exact build options, e.g. `Some("pgo+lto".into())`. `None`
    /// restores the default with fallbacks.
    pub fn variant(mut self, variant: Option<String>) -> Self {
        self.variant = variant;
        self
    }

    /// Print nothing to stderr when `true`.
    pub fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new(DEFAULT_PYTHON_VERSION)
    }
}

/// `eprintln!` unless the options ask for quiet.
macro_rules! say {
    ($opts:expr, $($arg:tt)*) => {
        if !$opts.quiet {
            eprintln!($($arg)*);
        }
    };
}

/// Where releases are looked up and downloaded from. Tests point it at a
/// local server.
struct Source<'a> {
    agent: &'a ureq::Agent,
    latest_release_url: &'a str,
    download_url: &'a str,
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
    let source = Source {
        agent: &agent,
        latest_release_url: LATEST_RELEASE_URL,
        download_url: RELEASE_DOWNLOAD_URL,
    };
    install_in(opts, &root, triple, &source)
}

/// [`install`] with the data directory, platform and release source given.
fn install_in(opts: &Options, root: &Path, triple: &str, src: &Source) -> Result<PathBuf> {
    validate_python_version(&opts.python_version)?;
    let tag = latest_tag(src.agent, src.latest_release_url)?;
    let sums_url = format!("{}/{tag}/SHA256SUMS", src.download_url);
    let sums = http_text(src.agent, &sums_url)?;
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
        say!(
            opts,
            "note: {DEFAULT_VARIANT} is not available for {triple}; using {}",
            asset.build_options
        );
    }
    if let Some(note) = newer_version_note(&assets, &opts.python_version, asset) {
        say!(opts, "{note}");
    }

    let dest = root.join(asset.dir_name());
    if dest.exists() {
        say!(opts, "already installed: {}", dest.display());
        return Ok(dest);
    }

    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    sweep_stale_temps(root, STALE_TEMP_AGE);
    // Temp names include the process id and the start time, so concurrent runs
    // of the same build don't delete each other's files even when two
    // containers sharing one data directory happen to have the same pid.
    let unique = unique_suffix();
    let archive = root.join(format!("{}.{unique}.download", asset.dir_name()));
    let partial = root.join(format!("{}.{unique}.partial", asset.dir_name()));
    let result = download_and_unpack(opts, src, &tag, asset, &archive, &partial, &dest);
    // Best-effort cleanup of temporaries, whether or not we succeeded.
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&partial);
    if result? {
        say!(opts, "installed: {}", dest.display());
    } else {
        say!(
            opts,
            "already installed by another process: {}",
            dest.display()
        );
    }
    Ok(dest)
}

/// Download, verify and unpack into `dest`. Returns `false` if another process
/// finished the same install first.
fn download_and_unpack(
    opts: &Options,
    src: &Source,
    tag: &str,
    asset: &Asset,
    archive: &Path,
    partial: &Path,
    dest: &Path,
) -> Result<bool> {
    let _ = fs::remove_dir_all(partial);
    // '+' must be percent-encoded in the URL path.
    let url = format!(
        "{}/{tag}/{}",
        src.download_url,
        asset.file_name.replace('+', "%2B")
    );
    say!(opts, "downloading {}", asset.file_name);
    download(src.agent, &url, archive, &asset.sha256, !opts.quiet)?;

    say!(opts, "extracting to {}", dest.display());
    unpack_and_publish(archive, partial, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const TRIPLE: &str = "x86_64-unknown-linux-gnu";
    const TAG: &str = "20990101";

    /// A minimal HTTP/1.1 server for `files` (request path -> body), on its own
    /// thread. Returns its base URL and the list of paths requested so far.
    fn serve(files: HashMap<String, Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                // Skip the headers.
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = request_line.split(' ').nth(1).unwrap_or("").to_string();
                seen.lock().unwrap().push(path.clone());
                let (status, body) = match files.get(&path) {
                    Some(body) => ("200 OK", body.clone()),
                    None => ("404 Not Found", Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (base, requests)
    }

    /// A tiny archive shaped like the real ones: `python/PYTHON.json`.
    fn archive() -> Vec<u8> {
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
        zstd::encode_all(&tar_bytes[..], 0).unwrap()
    }

    /// Release files for one asset, Python 3.14.1 freethreaded+pgo+lto, whose
    /// `SHA256SUMS` entry is `sha256` (the real hash when `None`).
    fn release(sha256: Option<&str>) -> (HashMap<String, Vec<u8>>, String) {
        let body = archive();
        let real: String = Sha256::digest(&body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let name = format!("cpython-3.14.1+{TAG}-{TRIPLE}-freethreaded+pgo+lto-full.tar.zst");
        let mut files = HashMap::new();
        files.insert(
            "/latest-release.json".to_string(),
            format!(r#"{{"version":1,"tag":"{TAG}"}}"#).into_bytes(),
        );
        files.insert(
            format!("/download/{TAG}/SHA256SUMS"),
            format!("{}  {name}\n", sha256.unwrap_or(&real)).into_bytes(),
        );
        let archive_path = format!("/download/{TAG}/{}", name.replace('+', "%2B"));
        files.insert(archive_path.clone(), body);
        (files, archive_path)
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pbs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Runs `install_in` against `base` with a direct (proxy-free) agent.
    fn install_from(base: &str, root: &Path, opts: &Options) -> Result<PathBuf> {
        let agent: ureq::Agent = ureq::Agent::config_builder().proxy(None).build().into();
        let latest = format!("{base}/latest-release.json");
        let download = format!("{base}/download");
        let src = Source {
            agent: &agent,
            latest_release_url: &latest,
            download_url: &download,
        };
        install_in(opts, root, TRIPLE, &src)
    }

    #[test]
    fn install_downloads_once_then_reuses_the_install() {
        let (files, archive_path) = release(None);
        let (base, requests) = serve(files);
        let root = temp_root("e2e");
        let opts = Options::new("3.14").quiet(true);

        let dest = install_from(&base, &root, &opts).unwrap();
        assert_eq!(dest, root.join("3.14.1+freethreaded+pgo+lto"));
        assert_eq!(fs::read(dest.join("python/PYTHON.json")).unwrap(), b"hi");
        // Only the install itself is left: no .download or .partial temp files.
        let names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["3.14.1+freethreaded+pgo+lto"]);

        // A second run finds the install and does not fetch the archive again.
        assert_eq!(install_from(&base, &root, &opts).unwrap(), dest);
        let archive_fetches = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| **p == archive_path)
            .count();
        assert_eq!(archive_fetches, 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn install_rejects_a_checksum_mismatch_and_leaves_nothing() {
        let (files, _) = release(Some(&"0".repeat(64)));
        let (base, _) = serve(files);
        let root = temp_root("badsum");

        let err = install_from(&base, &root, &Options::new("3.14").quiet(true)).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            0,
            "no install, no temps"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn install_reports_missing_build_options() {
        let (files, _) = release(None);
        let (base, _) = serve(files);
        let root = temp_root("novariant");

        let opts = Options::new("3.14").variant(Some("pgo".into())).quiet(true);
        let err = install_from(&base, &root, &opts).unwrap_err().to_string();
        assert!(
            err.contains("available build options for that version: freethreaded+pgo+lto"),
            "{err}"
        );
        assert!(
            !root.exists(),
            "nothing is created before a build is chosen"
        );
    }

    #[test]
    fn options_builder() {
        assert_eq!(Options::default(), Options::new(DEFAULT_PYTHON_VERSION));
        let o = Options::new("3")
            .variant(Some("pgo+lto".into()))
            .quiet(true);
        assert_eq!(o.python_version, "3");
        assert_eq!(o.variant.as_deref(), Some("pgo+lto"));
        assert!(o.quiet);
        assert_eq!(o.variant(None).variant, None);
    }
}
