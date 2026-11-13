//! HTTP access: the release lookup and the verified download.

use crate::validate::validate_tag;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

pub(crate) const LATEST_RELEASE_URL: &str = "https://raw.githubusercontent.com/astral-sh/python-build-standalone/latest-release/latest-release.json";
pub(crate) const RELEASE_DOWNLOAD_URL: &str =
    "https://github.com/astral-sh/python-build-standalone/releases/download";

pub(crate) fn download(
    agent: &ureq::Agent,
    url: &str,
    out: &Path,
    expected_sha256: &str,
) -> Result<()> {
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let total = resp.body().content_length();
    let reader = resp.body_mut().as_reader();
    let mut file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    copy_verified(reader, &mut file, total, expected_sha256)
        .with_context(|| format!("downloading {url}"))
}

/// Copy `reader` to `writer`, then check the SHA-256 of what was copied
/// against `expected` (hex, case-insensitive). This is the only integrity
/// check on the downloaded archive. `total`, if known, drives progress output.
pub(crate) fn copy_verified(
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

pub(crate) fn agent() -> ureq::Agent {
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

pub(crate) fn http_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

pub(crate) fn latest_tag(agent: &ureq::Agent) -> Result<String> {
    tag_from_latest_release(&http_text(agent, LATEST_RELEASE_URL)?)
}

/// The release tag in `latest-release.json`. It ends up in a URL, so it is
/// checked by `validate_tag`.
pub(crate) fn tag_from_latest_release(body: &str) -> Result<String> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("parsing latest-release.json")?;
    let tag = json
        .get("tag")
        .and_then(|t| t.as_str())
        .context("latest-release.json has no \"tag\"")?;
    Ok(validate_tag(tag)?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
