use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use anyhow::{Context as _, Result, ensure};
use serde_json::json;
use sha2::{Digest as _, Sha256};

fn run(arguments: &[&std::ffi::OsStr]) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_zec"))
        .args(arguments)
        .output()
        .context("run actual zec update CLI")
}

fn write_manifest_for_version(
    directory: &Path,
    version: &str,
    asset: &str,
    sha256: &str,
    size: usize,
) -> Result<std::path::PathBuf> {
    let manifest = directory.join("zec-update-v1.json");
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "version": version,
            "release_url": format!(
                "https://github.com/neguse/zec/releases/tag/v{}",
                version
            ),
            "assets": [{
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "url": asset,
                "sha256": sha256,
                "size": size,
                "executable": format!("zec{}", std::env::consts::EXE_SUFFIX)
            }]
        }))?,
    )
    .context("write update manifest")?;
    Ok(manifest)
}

fn write_manifest(directory: &Path, sha256: &str, size: usize) -> Result<std::path::PathBuf> {
    write_manifest_for_version(
        directory,
        env!("CARGO_PKG_VERSION"),
        "zec-asset",
        sha256,
        size,
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn actual_binary_checks_verifies_and_downloads_a_local_release() -> Result<()> {
    let temp = tempfile::tempdir().context("create update CLI fixture")?;
    let asset = temp.path().join("zec-asset");
    // The asset must execute `--version` on every platform during download
    // verification, so the actual zec binary is the release payload.
    let asset_bytes = fs::read(env!("CARGO_BIN_EXE_zec")).context("read actual zec binary")?;
    fs::write(&asset, &asset_bytes).context("write fixture executable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&asset, fs::Permissions::from_mode(0o755))
            .context("make fixture asset executable")?;
    }
    let digest = sha256_hex(&asset_bytes);
    let manifest = write_manifest(temp.path(), &digest, asset_bytes.len())?;

    let version = run(&["--version".as_ref()])?;
    ensure!(version.status.success());
    let expected_version_output = format!("zec {}\n", env!("CARGO_PKG_VERSION")).into_bytes();
    ensure!(
        String::from_utf8(version.stdout.clone())?.trim()
            == format!("zec {}", env!("CARGO_PKG_VERSION"))
    );

    let check = run(&[
        "update".as_ref(),
        "check".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ])?;
    ensure!(
        check.status.success(),
        "update check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let check: serde_json::Value = serde_json::from_slice(&check.stdout)?;
    ensure!(check["status"] == "current");
    ensure!(check["asset_sha256"] == digest);

    let verify = run(&[
        "update".as_ref(),
        "verify".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
        "--binary".as_ref(),
        asset.as_os_str(),
    ])?;
    ensure!(
        verify.status.success(),
        "update verify failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    ensure!(serde_json::from_slice::<serde_json::Value>(&verify.stdout)?["status"] == "verified");

    let downloaded = temp.path().join("downloaded-zec");
    let download = run(&[
        "update".as_ref(),
        "download".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
        "--output".as_ref(),
        downloaded.as_os_str(),
    ])?;
    ensure!(
        download.status.success(),
        "update download failed: {}",
        String::from_utf8_lossy(&download.stderr)
    );
    ensure!(
        serde_json::from_slice::<serde_json::Value>(&download.stdout)?["status"] == "downloaded"
    );
    ensure!(fs::read(&downloaded)? == asset_bytes);
    ensure!(Command::new(&downloaded).arg("--version").output()?.stdout == expected_version_output);

    Ok(())
}

#[test]
fn checksum_failure_never_materializes_an_output() -> Result<()> {
    let temp = tempfile::tempdir().context("create rejected update fixture")?;
    let asset = temp.path().join("zec-asset");
    let bytes = b"not a trusted executable";
    fs::write(&asset, bytes)?;
    let manifest = write_manifest(temp.path(), &"0".repeat(64), bytes.len())?;
    let output = temp.path().join("must-not-exist");

    let result = run(&[
        "update".as_ref(),
        "download".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
        "--output".as_ref(),
        output.as_os_str(),
    ])?;
    ensure!(!result.status.success());
    ensure!(String::from_utf8_lossy(&result.stderr).contains("SHA-256 mismatch"));
    ensure!(!output.exists());
    Ok(())
}

/// Unix replaces the running executable atomically; Windows refuses by
/// contract and leaves the installed binary untouched.
#[test]
fn actual_binary_atomically_applies_an_update_to_a_disposable_copy() -> Result<()> {
    let actual_binary = Path::new(env!("CARGO_BIN_EXE_zec"));
    let binary_directory = actual_binary
        .parent()
        .context("actual zec binary has no parent directory")?;
    let temp = tempfile::tempdir_in(binary_directory)
        .context("create self-update fixture beside the actual binary")?;
    let mut next = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    next.patch += 1;
    next.pre = semver::Prerelease::EMPTY;
    next.build = semver::BuildMetadata::EMPTY;

    let candidate_name = "zec-next";
    let candidate = temp.path().join(candidate_name);
    let candidate_bytes = candidate_payload(&next);
    fs::write(&candidate, &candidate_bytes).context("write update candidate")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))?;
    }
    let digest = sha256_hex(&candidate_bytes);
    let manifest = write_manifest_for_version(
        temp.path(),
        &next.to_string(),
        candidate_name,
        &digest,
        candidate_bytes.len(),
    )?;

    let installed = temp
        .path()
        .join(format!("zec-under-test{}", std::env::consts::EXE_SUFFIX));
    fs::copy(actual_binary, &installed).context("copy actual zec binary")?;
    let installed_bytes = fs::read(&installed)?;
    let applied = Command::new(&installed)
        .args(["update", "apply", "--manifest"])
        .arg(&manifest)
        .output()
        .context("apply update through disposable actual binary")?;
    #[cfg(unix)]
    {
        ensure!(
            applied.status.success(),
            "update apply failed: {}",
            String::from_utf8_lossy(&applied.stderr)
        );
        ensure!(
            serde_json::from_slice::<serde_json::Value>(&applied.stdout)?["status"] == "updated"
        );
        ensure!(fs::read(&installed)? == candidate_bytes);
        ensure!(
            String::from_utf8(Command::new(&installed).arg("--version").output()?.stdout)?.trim()
                == format!("zec {next}")
        );
    }
    #[cfg(windows)]
    {
        ensure!(
            !applied.status.success(),
            "Windows must refuse to replace a running executable"
        );
        ensure!(
            String::from_utf8_lossy(&applied.stderr)
                .contains("Windows cannot replace a running executable safely"),
            "unexpected apply error: {}",
            String::from_utf8_lossy(&applied.stderr)
        );
        ensure!(fs::read(&installed)? == installed_bytes);
    }
    #[cfg(unix)]
    let _ = installed_bytes;
    Ok(())
}

/// A candidate that prints `zec <version>` when executed. Windows never
/// executes it (apply refuses first), so arbitrary bytes suffice there.
fn candidate_payload(next: &semver::Version) -> Vec<u8> {
    #[cfg(unix)]
    {
        format!("#!/bin/sh\nprintf '%s\\n' 'zec {next}'\n").into_bytes()
    }
    #[cfg(windows)]
    {
        format!("placeholder update candidate for zec {next}").into_bytes()
    }
}
