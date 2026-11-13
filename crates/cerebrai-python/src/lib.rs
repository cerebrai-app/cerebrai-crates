//! Download a [python-build-standalone](https://github.com/astral-sh/python-build-standalone)
//! distribution into the user's XDG data directory.
//!
//! Shared by the `brainctl` and `workctl` command line tools. Progress and
//! notes are written to stderr; nothing is written to stdout, so callers keep
//! it for their own output. [`install`] returns the install directory.
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
use net::{RELEASE_DOWNLOAD_URL, agent, download, http_text, latest_tag};
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// A Python version: `3` (newest 3.x), `3.14` (newest 3.14.x) or `3.14.15`
    /// (exact). Not a python-build-standalone release date.
    pub python_version: String,
    /// Exact build options, e.g. `freethreaded+pgo+lto`. `None` means the
    /// default with fallbacks.
    pub variant: Option<String>,
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
        eprintln!("already installed: {}", dest.display());
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
        eprintln!("installed: {}", dest.display());
    }
    Ok(dest)
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
