//! Unpacking an archive and publishing it atomically, plus temp file handling.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Renaming the unpacked tree into place is retried this many times, this long
/// apart, while the destination still does not exist.
pub(crate) const RENAME_ATTEMPTS: u32 = 5;
pub(crate) const RENAME_DELAY: Duration = Duration::from_millis(200);

/// Leftover temp files older than this are removed at the start of a run.
pub(crate) const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// `fs::rename`, retried while `to` still does not exist. On Windows a virus
/// scanner often holds files in a freshly unpacked tree, which makes the rename
/// fail with "Access is denied" for a moment. If `to` has appeared (another
/// process won the race) the error is returned at once for the caller to handle.
pub(crate) fn rename_retrying(
    from: &Path,
    to: &Path,
    attempts: u32,
    delay: Duration,
) -> std::io::Result<()> {
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
pub(crate) fn sweep_stale_temps(root: &Path, max_age: Duration) {
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

/// Unpack the `.tar.zst` `archive` into `partial`, then rename it to `dest`.
/// Returns `false` if `dest` appeared in the meantime (another process won the
/// race), leaving `partial` for the caller to clean up.
pub(crate) fn unpack_and_publish(archive: &Path, partial: &Path, dest: &Path) -> Result<bool> {
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

/// `<pid>-<nanoseconds since the epoch>`.
pub(crate) fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn unique_suffix_differs_between_calls() {
        let (a, b) = (unique_suffix(), unique_suffix());
        assert!(a.starts_with(&format!("{}-", std::process::id())), "{a}");
        assert_ne!(a, b);
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
}
