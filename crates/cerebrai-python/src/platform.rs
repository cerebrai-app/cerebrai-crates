//! The host platform: which build to fetch and where to put it.

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub(crate) fn host_triple() -> Result<&'static str> {
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
pub(crate) fn data_home_from(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn os(p: &Path) -> Option<OsString> {
        Some(p.as_os_str().to_owned())
    }

    // Paths come from temp_dir() so they are absolute on every platform
    // (a bare "/x/data" has no drive letter on Windows).
    #[test]
    fn data_home_prefers_absolute_xdg_everywhere() {
        let xdg = std::env::temp_dir().join("xdg-data");
        let other = std::env::temp_dir().join("other");
        for windows in [false, true] {
            let p = data_home_from(os(&xdg), os(&other), os(&other), os(&other), windows).unwrap();
            assert_eq!(p, xdg);
        }
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
