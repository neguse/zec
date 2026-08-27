//! Manifest-driven, checksum-verified updates for the standalone console binary.

use std::{
    ffi::OsString,
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    process::Command as ProcessCommand,
    sync::Arc,
};

use anyhow::{Context as _, Result, bail, ensure};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Url};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub(crate) const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/neguse/zec/releases/latest/download/zec-update-v1.json";
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_ASSET_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UpdateSource {
    File(PathBuf),
    Https(String),
}

impl UpdateSource {
    fn parse(value: OsString) -> Result<Self> {
        if let Some(value) = value.to_str() {
            if value.starts_with("https://") {
                validate_https_url(value, "manifest URL")?;
                return Ok(Self::Https(value.to_owned()));
            }
            ensure!(
                !value.contains("://"),
                "update sources must be local paths or HTTPS URLs"
            );
        }
        Ok(Self::File(PathBuf::from(value)))
    }

    fn default_remote() -> Self {
        Self::Https(DEFAULT_MANIFEST_URL.to_owned())
    }

    fn configured_default() -> Result<Self> {
        std::env::var_os("ZEC_UPDATE_MANIFEST")
            .map(Self::parse)
            .transpose()
            .map(|source| source.unwrap_or_else(Self::default_remote))
    }

    fn display(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Https(url) => url.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UpdateCommand {
    Check {
        manifest: UpdateSource,
    },
    Download {
        manifest: UpdateSource,
        output: PathBuf,
    },
    Apply {
        manifest: UpdateSource,
    },
    Verify {
        manifest: UpdateSource,
        binary: PathBuf,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateManifest {
    pub(crate) schema_version: u32,
    pub(crate) version: String,
    pub(crate) release_url: String,
    pub(crate) assets: Vec<UpdateAsset>,
    #[serde(default)]
    pub(crate) remote_servers: Vec<RemoteServerAsset>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateAsset {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) executable: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteServerAsset {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UpdateReport {
    schema_version: u32,
    status: &'static str,
    current_version: String,
    manifest_version: String,
    os: String,
    arch: String,
    manifest_source: String,
    asset_url: String,
    asset_sha256: String,
    asset_size: u64,
    output: Option<String>,
}

impl UpdateReport {
    fn new(
        status: &'static str,
        manifest: &ValidatedManifest,
        asset: &UpdateAsset,
        source: &UpdateSource,
        output: Option<&Path>,
    ) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            status,
            current_version: env!("CARGO_PKG_VERSION").to_owned(),
            manifest_version: manifest.version.to_string(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            manifest_source: source.display(),
            asset_url: asset.url.clone(),
            asset_sha256: asset.sha256.clone(),
            asset_size: asset.size,
            output: output.map(|path| path.display().to_string()),
        }
    }

    pub(crate) fn terminal_message(&self) -> String {
        match self.status {
            "update-available" => format!(
                "zec {} is available (current {}); exit and run `zec update apply`",
                self.manifest_version, self.current_version
            ),
            "current" => format!("zec {} is current", self.current_version),
            _ => format!("zec update {}: {}", self.status, self.manifest_version),
        }
    }
}

#[derive(Debug)]
struct ValidatedManifest {
    manifest: UpdateManifest,
    version: Version,
}

pub(crate) fn parse_update_command(arguments: &[OsString]) -> Result<UpdateCommand> {
    let Some(operation) = arguments.first().and_then(|argument| argument.to_str()) else {
        bail!(update_usage());
    };
    ensure!(
        matches!(operation, "check" | "download" | "apply" | "verify"),
        "unknown update operation {operation:?}\n{}",
        update_usage()
    );

    let mut manifest = None;
    let mut output = None;
    let mut binary = None;
    let mut index = 1;
    while index < arguments.len() {
        let option = arguments[index]
            .to_str()
            .context("update options must be UTF-8")?;
        let target = match option {
            "--manifest" => &mut manifest,
            "--output" => &mut output,
            "--binary" => &mut binary,
            _ => bail!("unknown update option {option:?}\n{}", update_usage()),
        };
        ensure!(target.is_none(), "duplicate update option {option}");
        index += 1;
        let value = arguments
            .get(index)
            .cloned()
            .with_context(|| format!("{option} requires a value"))?;
        *target = Some(value);
        index += 1;
    }

    let manifest = match manifest {
        Some(manifest) => UpdateSource::parse(manifest)?,
        None => UpdateSource::configured_default()?,
    };
    match operation {
        "check" => {
            ensure!(
                output.is_none() && binary.is_none(),
                "check only accepts --manifest"
            );
            Ok(UpdateCommand::Check { manifest })
        }
        "download" => {
            ensure!(binary.is_none(), "download does not accept --binary");
            let output = output
                .map(PathBuf::from)
                .context("download requires --output PATH")?;
            Ok(UpdateCommand::Download { manifest, output })
        }
        "apply" => {
            ensure!(
                output.is_none() && binary.is_none(),
                "apply only accepts --manifest"
            );
            Ok(UpdateCommand::Apply { manifest })
        }
        "verify" => {
            ensure!(output.is_none(), "verify does not accept --output");
            let binary = binary
                .map(PathBuf::from)
                .context("verify requires --binary PATH")?;
            Ok(UpdateCommand::Verify { manifest, binary })
        }
        _ => unreachable!(),
    }
}

pub(crate) fn update_usage() -> &'static str {
    "Usage:\n  zec update check [--manifest PATH|HTTPS_URL]\n  zec update download --output PATH [--manifest PATH|HTTPS_URL]\n  zec update apply [--manifest PATH|HTTPS_URL]\n  zec update verify --binary PATH [--manifest PATH|HTTPS_URL]"
}

pub(crate) async fn execute(
    command: UpdateCommand,
    http: Arc<dyn HttpClient>,
) -> Result<UpdateReport> {
    let source = match &command {
        UpdateCommand::Check { manifest }
        | UpdateCommand::Download { manifest, .. }
        | UpdateCommand::Apply { manifest }
        | UpdateCommand::Verify { manifest, .. } => manifest.clone(),
    };
    let manifest_bytes = load_source(&source, http.clone(), MAX_MANIFEST_BYTES).await?;
    let manifest = parse_manifest(&manifest_bytes, matches!(source, UpdateSource::Https(_)))?;
    let asset = select_asset(&manifest)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("zec package version is not valid semver")?;

    match command {
        UpdateCommand::Check { .. } => Ok(UpdateReport::new(
            if manifest.version > current {
                "update-available"
            } else {
                "current"
            },
            &manifest,
            asset,
            &source,
            None,
        )),
        UpdateCommand::Verify { binary, .. } => {
            let bytes = read_local_file(&binary, MAX_ASSET_BYTES)?;
            verify_asset(&bytes, asset)?;
            verify_candidate_version(&binary, &manifest.version)?;
            Ok(UpdateReport::new(
                "verified",
                &manifest,
                asset,
                &source,
                Some(&binary),
            ))
        }
        UpdateCommand::Download { output, .. } => {
            let asset_source = asset_source(&source, &asset.url)?;
            let bytes = load_source(&asset_source, http, MAX_ASSET_BYTES).await?;
            verify_asset(&bytes, asset)?;
            install_download(&bytes, &output, &manifest.version)?;
            Ok(UpdateReport::new(
                "downloaded",
                &manifest,
                asset,
                &source,
                Some(&output),
            ))
        }
        UpdateCommand::Apply { .. } => {
            ensure!(
                manifest.version > current,
                "zec {current} is already current; manifest version is {}",
                manifest.version
            );
            let asset_source = asset_source(&source, &asset.url)?;
            let bytes = load_source(&asset_source, http, MAX_ASSET_BYTES).await?;
            verify_asset(&bytes, asset)?;
            let output = apply_self_update(&bytes, &manifest.version)?;
            Ok(UpdateReport::new(
                "updated",
                &manifest,
                asset,
                &source,
                Some(&output),
            ))
        }
    }
}

pub(crate) async fn check_default(http: Arc<dyn HttpClient>) -> Result<UpdateReport> {
    execute(
        UpdateCommand::Check {
            manifest: UpdateSource::configured_default()?,
        },
        http,
    )
    .await
}

fn parse_manifest(bytes: &[u8], remote: bool) -> Result<ValidatedManifest> {
    let manifest: UpdateManifest =
        serde_json::from_slice(bytes).context("parse update manifest JSON")?;
    ensure!(
        manifest.schema_version == MANIFEST_SCHEMA_VERSION,
        "unsupported update manifest schema {}; expected {MANIFEST_SCHEMA_VERSION}",
        manifest.schema_version
    );
    let version = Version::parse(&manifest.version)
        .with_context(|| format!("invalid update version {:?}", manifest.version))?;
    ensure!(
        version.build.is_empty(),
        "update version must not contain build metadata"
    );
    validate_https_url(&manifest.release_url, "release URL")?;
    ensure!(!manifest.assets.is_empty(), "update manifest has no assets");

    let mut targets = std::collections::BTreeSet::new();
    for asset in &manifest.assets {
        ensure!(
            !asset.os.is_empty() && !asset.arch.is_empty(),
            "asset target is empty"
        );
        ensure!(
            targets.insert((&asset.os, &asset.arch)),
            "duplicate update asset for {}/{}",
            asset.os,
            asset.arch
        );
        ensure!(
            asset.size > 0 && asset.size <= MAX_ASSET_BYTES as u64,
            "asset size is outside 1..={MAX_ASSET_BYTES}: {}",
            asset.size
        );
        ensure!(
            is_lower_hex_sha256(&asset.sha256),
            "asset sha256 must be 64 lowercase hexadecimal digits"
        );
        let expected_executable = if asset.os == "windows" {
            "zec.exe"
        } else {
            "zec"
        };
        ensure!(
            asset.executable == expected_executable,
            "asset executable for {}/{} must be {expected_executable:?}",
            asset.os,
            asset.arch
        );
        if remote {
            validate_https_url(&asset.url, "asset URL")?;
        } else if asset.url.contains("://") {
            validate_https_url(&asset.url, "asset URL")?;
        } else {
            validate_relative_asset_path(&asset.url)?;
        }
    }
    let mut remote_targets = std::collections::BTreeSet::new();
    for asset in &manifest.remote_servers {
        ensure!(
            matches!(asset.os.as_str(), "linux" | "macos" | "windows")
                && matches!(asset.arch.as_str(), "x86_64" | "aarch64"),
            "unsupported remote server target {}/{}",
            asset.os,
            asset.arch
        );
        ensure!(
            remote_targets.insert((&asset.os, &asset.arch)),
            "duplicate remote server asset for {}/{}",
            asset.os,
            asset.arch
        );
        validate_download_record(
            &asset.url,
            &asset.sha256,
            asset.size,
            remote,
            "remote server asset",
        )?;
        let expected_suffix = if asset.os == "windows" { ".zip" } else { ".gz" };
        ensure!(
            asset.url.ends_with(expected_suffix),
            "remote server asset for {}/{} must end in {expected_suffix}",
            asset.os,
            asset.arch
        );
    }

    Ok(ValidatedManifest { manifest, version })
}

fn validate_download_record(
    url: &str,
    sha256: &str,
    size: u64,
    remote_manifest: bool,
    label: &str,
) -> Result<()> {
    ensure!(
        size > 0 && size <= MAX_ASSET_BYTES as u64,
        "{label} size is outside 1..={MAX_ASSET_BYTES}: {size}"
    );
    ensure!(
        is_lower_hex_sha256(sha256),
        "{label} sha256 must be 64 lowercase hexadecimal digits"
    );
    if remote_manifest {
        validate_https_url(url, label)
    } else if url.contains("://") {
        validate_https_url(url, label)
    } else {
        validate_relative_asset_path(url)
    }
}

pub(crate) async fn acquire_remote_server_archive(
    platform: remote::RemotePlatform,
    requested_version: Version,
    http: Arc<dyn HttpClient>,
) -> Result<PathBuf> {
    let source = if std::env::var_os("ZEC_UPDATE_MANIFEST").is_some() {
        UpdateSource::configured_default()?
    } else {
        UpdateSource::Https(format!(
            "https://github.com/neguse/zec/releases/download/v{requested_version}/zec-update-v1.json"
        ))
    };
    let bytes = load_source(&source, http.clone(), MAX_MANIFEST_BYTES).await?;
    let manifest = parse_manifest(&bytes, matches!(source, UpdateSource::Https(_)))?;
    ensure!(
        manifest.version == requested_version,
        "remote server manifest version {} does not match client {requested_version}",
        manifest.version
    );
    let os = platform.os.as_str();
    let arch = platform.arch.as_str();
    let asset = manifest
        .manifest
        .remote_servers
        .iter()
        .find(|asset| asset.os == os && asset.arch == arch)
        .with_context(|| {
            format!("release {requested_version} has no remote server for {os}/{arch}")
        })?;
    let asset_source = asset_source(&source, &asset.url)?;
    let bytes = load_source(&asset_source, http, MAX_ASSET_BYTES).await?;
    verify_download_record(&bytes, &asset.sha256, asset.size, "remote server asset")?;

    let extension = if platform.os.is_windows() {
        "zip"
    } else {
        "gz"
    };
    let directory = paths::remote_servers_dir()
        .join("zec")
        .join(requested_version.to_string());
    fs::create_dir_all(&directory)
        .with_context(|| format!("create remote server cache {}", directory.display()))?;
    let destination = directory.join(format!(
        "zec-remote-server-{os}-{arch}-{}.{}",
        &asset.sha256[..16],
        extension
    ));
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "remote server cache entry is not a regular file: {}",
                destination.display()
            );
            let cached = read_local_file(&destination, MAX_ASSET_BYTES)?;
            verify_download_record(&cached, &asset.sha256, asset.size, "cached remote server")?;
            return Ok(destination);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect remote server cache {}", destination.display()));
        }
    }
    let mut candidate = tempfile::Builder::new()
        .prefix(".zec-remote-server-")
        .suffix(&format!(".{extension}"))
        .tempfile_in(&directory)
        .with_context(|| {
            format!(
                "create remote server cache candidate in {}",
                directory.display()
            )
        })?;
    candidate
        .write_all(&bytes)
        .context("write remote server cache candidate")?;
    candidate
        .flush()
        .context("flush remote server cache candidate")?;
    candidate
        .as_file()
        .sync_all()
        .context("sync remote server cache candidate")?;
    candidate
        .persist(&destination)
        .map_err(|error| error.error)
        .with_context(|| format!("install remote server cache {}", destination.display()))?;
    sync_directory(&directory)?;
    Ok(destination)
}

fn verify_download_record(bytes: &[u8], sha256: &str, size: u64, label: &str) -> Result<()> {
    ensure!(
        bytes.len() as u64 == size,
        "{label} size mismatch: expected {size}, got {}",
        bytes.len()
    );
    let digest = hex_sha256(bytes);
    ensure!(
        digest == sha256,
        "{label} SHA-256 mismatch: expected {sha256}, got {digest}"
    );
    Ok(())
}

fn select_asset(manifest: &ValidatedManifest) -> Result<&UpdateAsset> {
    manifest
        .manifest
        .assets
        .iter()
        .find(|asset| asset.os == std::env::consts::OS && asset.arch == std::env::consts::ARCH)
        .with_context(|| {
            format!(
                "release {} has no asset for {}/{}",
                manifest.version,
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })
}

fn asset_source(manifest_source: &UpdateSource, asset: &str) -> Result<UpdateSource> {
    if asset.starts_with("https://") {
        validate_https_url(asset, "asset URL")?;
        return Ok(UpdateSource::Https(asset.to_owned()));
    }
    let UpdateSource::File(manifest_path) = manifest_source else {
        bail!("a remote manifest may only reference HTTPS assets");
    };
    validate_relative_asset_path(asset)?;
    let parent = manifest_path
        .parent()
        .context("local update manifest has no parent directory")?;
    Ok(UpdateSource::File(parent.join(asset)))
}

async fn load_source(
    source: &UpdateSource,
    http: Arc<dyn HttpClient>,
    limit: usize,
) -> Result<Vec<u8>> {
    match source {
        UpdateSource::File(path) => read_local_file(path, limit),
        UpdateSource::Https(url) => {
            validate_https_url(url, "download URL")?;
            let mut response = http
                .get(url, AsyncBody::default(), true)
                .await
                .with_context(|| format!("download {url}"))?;
            ensure!(
                response.status().is_success(),
                "download {url} returned HTTP {}",
                response.status()
            );
            if let Some(length) = response
                .headers()
                .get(http_client::http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
            {
                ensure!(length <= limit, "download exceeds {limit} bytes");
            }
            let mut body = response.body_mut().take((limit + 1) as u64);
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes)
                .await
                .context("read download body")?;
            ensure!(bytes.len() <= limit, "download exceeds {limit} bytes");
            Ok(bytes)
        }
    }
}

fn read_local_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read update source metadata {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "update source is not a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= limit as u64,
        "update source exceeds {limit} bytes: {}",
        path.display()
    );
    fs::read(path).with_context(|| format!("read update source {}", path.display()))
}

fn verify_asset(bytes: &[u8], asset: &UpdateAsset) -> Result<()> {
    ensure!(
        bytes.len() as u64 == asset.size,
        "asset size mismatch: expected {}, got {}",
        asset.size,
        bytes.len()
    );
    let digest = hex_sha256(bytes);
    ensure!(
        digest == asset.sha256,
        "asset SHA-256 mismatch: expected {}, got {digest}",
        asset.sha256
    );
    Ok(())
}

fn install_download(bytes: &[u8], output: &Path, version: &Version) -> Result<()> {
    ensure_target_absent(output)?;
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.is_dir(),
        "output parent does not exist: {}",
        parent.display()
    );
    let candidate = write_candidate(bytes, parent, None)?;
    verify_candidate_version(&candidate, version)?;
    candidate
        .persist(output)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "atomically install downloaded binary at {}",
                output.display()
            )
        })?;
    sync_directory(parent)?;
    Ok(())
}

#[cfg(not(windows))]
fn apply_self_update(bytes: &[u8], version: &Version) -> Result<PathBuf> {
    let current = std::env::current_exe().context("resolve current zec executable")?;
    let metadata = fs::symlink_metadata(&current)
        .with_context(|| format!("inspect current executable {}", current.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "current executable is not a regular file: {}",
        current.display()
    );
    let parent = current
        .parent()
        .context("current executable has no parent directory")?;
    let candidate = write_candidate(bytes, parent, Some(metadata.permissions()))?;
    verify_candidate_version(&candidate, version)?;
    candidate
        .persist(&current)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "atomically replace current executable {}",
                current.display()
            )
        })?;
    sync_directory(parent)?;
    Ok(current)
}

#[cfg(windows)]
fn apply_self_update(_bytes: &[u8], _version: &Version) -> Result<PathBuf> {
    bail!(
        "Windows cannot replace a running executable safely; use `zec update download --output zec-new.exe`, exit zec, then replace zec.exe explicitly"
    )
}

fn write_candidate(
    bytes: &[u8],
    parent: &Path,
    permissions: Option<fs::Permissions>,
) -> Result<tempfile::TempPath> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let mut file = tempfile::Builder::new()
        .prefix(".zec-update-")
        .suffix(suffix)
        .tempfile_in(parent)
        .with_context(|| format!("create update candidate in {}", parent.display()))?;
    file.write_all(bytes).context("write update candidate")?;
    file.flush().context("flush update candidate")?;
    file.as_file().sync_all().context("sync update candidate")?;
    let path = file.into_temp_path();
    let permissions = permissions.unwrap_or_else(executable_permissions);
    fs::set_permissions(&path, permissions).context("mark update candidate executable")?;
    Ok(path)
}

fn verify_candidate_version(path: &Path, version: &Version) -> Result<()> {
    let output = ProcessCommand::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("execute update candidate {}", path.display()))?;
    ensure!(
        output.status.success(),
        "update candidate --version failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8(output.stdout).context("candidate --version is not UTF-8")?;
    ensure!(
        stdout.trim() == format!("zec {version}"),
        "update candidate version mismatch: expected `zec {version}`, got {:?}",
        stdout.trim()
    );
    Ok(())
}

fn ensure_target_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!("refusing to overwrite existing output: {}", path.display()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect update output {}", path.display()))
        }
    }
}

#[cfg(unix)]
fn executable_permissions() -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt as _;
    fs::Permissions::from_mode(0o755)
}

#[cfg(windows)]
fn executable_permissions() -> fs::Permissions {
    let temporary = tempfile::tempfile().expect("create permissions template");
    temporary
        .metadata()
        .expect("read permissions template")
        .permissions()
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("open update directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync update directory {}", path.display()))
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn validate_https_url(value: &str, label: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| format!("invalid {label}: {value:?}"))?;
    ensure!(url.scheme() == "https", "{label} must use HTTPS");
    ensure!(url.host().is_some(), "{label} must have a host");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{label} must not contain credentials"
    );
    Ok(())
}

fn validate_relative_asset_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(!value.is_empty(), "local asset path is empty");
    ensure!(path.is_relative(), "local asset path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir)),
        "local asset path must not escape the manifest directory"
    );
    Ok(())
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(asset_url: &str, sha256: &str, size: usize) -> Vec<u8> {
        serde_json::to_vec(&UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            version: "1.2.3".to_owned(),
            release_url: "https://github.com/neguse/zec/releases/tag/v1.2.3".to_owned(),
            assets: vec![UpdateAsset {
                os: std::env::consts::OS.to_owned(),
                arch: std::env::consts::ARCH.to_owned(),
                url: asset_url.to_owned(),
                sha256: sha256.to_owned(),
                size: size as u64,
                executable: if cfg!(windows) { "zec.exe" } else { "zec" }.to_owned(),
            }],
            remote_servers: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn local_manifest_selects_current_platform_and_verifies_digest() {
        let bytes = b"candidate";
        let digest = hex_sha256(bytes);
        let manifest = parse_manifest(&manifest("zec", &digest, bytes.len()), false).unwrap();
        let asset = select_asset(&manifest).unwrap();
        verify_asset(bytes, asset).unwrap();
        assert_eq!(manifest.version, Version::new(1, 2, 3));
    }

    #[test]
    fn remote_manifest_rejects_insecure_or_escaping_assets() {
        let digest = "0".repeat(64);
        let insecure = manifest("http://example.com/zec", &digest, 1);
        assert!(parse_manifest(&insecure, true).is_err());
        let escaping = manifest("../zec", &digest, 1);
        assert!(parse_manifest(&escaping, false).is_err());
    }

    #[test]
    fn remote_server_manifest_records_are_targeted_and_integrity_checked() {
        let archive = b"compressed remote server";
        let digest = hex_sha256(archive);
        let mut value: UpdateManifest =
            serde_json::from_slice(&manifest("zec", &"0".repeat(64), 1)).unwrap();
        value.remote_servers.push(RemoteServerAsset {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            url: "zec-remote-server-linux-x86_64.gz".to_owned(),
            sha256: digest.clone(),
            size: archive.len() as u64,
        });

        let parsed = parse_manifest(&serde_json::to_vec(&value).unwrap(), false).unwrap();
        let asset = &parsed.manifest.remote_servers[0];
        verify_download_record(archive, &asset.sha256, asset.size, "remote server").unwrap();
        assert!(
            verify_download_record(b"tampered", &asset.sha256, asset.size, "remote server")
                .is_err()
        );
        assert!(
            verify_download_record(archive, &asset.sha256, asset.size + 1, "remote server")
                .is_err()
        );

        let mut duplicate = value.clone();
        duplicate
            .remote_servers
            .push(value.remote_servers[0].clone());
        assert!(parse_manifest(&serde_json::to_vec(&duplicate).unwrap(), false).is_err());

        let mut wrong_suffix = value.clone();
        wrong_suffix.remote_servers[0].url = "remote.zip".to_owned();
        assert!(parse_manifest(&serde_json::to_vec(&wrong_suffix).unwrap(), false).is_err());

        let mut unsupported = value;
        unsupported.remote_servers[0].arch = "riscv64".to_owned();
        assert!(parse_manifest(&serde_json::to_vec(&unsupported).unwrap(), false).is_err());
    }

    #[test]
    fn hosted_remote_server_records_require_https() {
        let digest = "0".repeat(64);
        let mut value: UpdateManifest =
            serde_json::from_slice(&manifest("https://example.com/zec", &digest, 1)).unwrap();
        value.remote_servers.push(RemoteServerAsset {
            os: "windows".to_owned(),
            arch: "aarch64".to_owned(),
            url: "http://example.com/remote.zip".to_owned(),
            sha256: digest,
            size: 1,
        });
        assert!(parse_manifest(&serde_json::to_vec(&value).unwrap(), true).is_err());
    }

    #[test]
    fn parser_requires_explicit_outputs_and_rejects_http() {
        assert!(parse_update_command(&["download".into()]).is_err());
        assert!(
            parse_update_command(&[
                "check".into(),
                "--manifest".into(),
                "http://example.com/update.json".into()
            ])
            .is_err()
        );
        assert!(matches!(
            parse_update_command(&[
                "verify".into(),
                "--manifest".into(),
                "update.json".into(),
                "--binary".into(),
                "zec".into()
            ])
            .unwrap(),
            UpdateCommand::Verify { .. }
        ));
    }
}
