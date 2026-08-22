//! Secure, repo-independent self-update support.
//!
//! Release metadata and artifacts come from a fixed GitHub origin. The
//! published checksum catches corruption; HTTPS and GitHub's release controls
//! are the authenticity boundary. Installation writes an exclusive temporary
//! beside the running executable and renames it into place only after all
//! validation succeeds.

use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/bermudi/stitch/releases/latest";
const RELEASE_BASE_URL: &str = "https://github.com/bermudi/stitch/releases/download";
const METADATA_LIMIT: usize = 1024 * 1024;
const CHECKSUM_LIMIT: usize = 4096;
const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
const BINARY_LIMIT: u64 = 128 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize)]
pub(crate) struct UpdateData {
    pub status: &'static str,
    pub current_version: String,
    pub latest_version: String,
    pub target: String,
    pub artifact: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug)]
struct Release {
    version: Version,
    artifact_name: String,
    artifact_url: String,
    checksum_url: String,
}

trait Transport {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>, String>;
}

struct HttpsTransport {
    agent: ureq::Agent,
}

impl HttpsTransport {
    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(5)
            .max_redirects_will_error(true)
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(format!("stitch/{}", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Transport for HttpsTransport {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|error| format!("GET {url} failed: {error}"))?;
        response
            .body_mut()
            .with_config()
            .limit(u64::try_from(limit).map_err(|_| "response limit is too large".to_string())?)
            .read_to_vec()
            .map_err(|error| format!("reading {url} failed: {error}"))
    }
}

pub(crate) fn run(check_only: bool, progress: bool) -> Result<UpdateData, String> {
    let target = release_target()?;
    if progress {
        eprintln!("Checking GitHub Releases for stitch updates...");
    }
    run_with(
        &HttpsTransport::new(),
        check_only,
        progress,
        env!("CARGO_PKG_VERSION"),
        target,
        None,
        &VersionProbe,
    )
}

fn run_with(
    transport: &dyn Transport,
    check_only: bool,
    progress: bool,
    current_version: &str,
    target: &str,
    install_path: Option<&Path>,
    probe: &dyn CandidateProbe,
) -> Result<UpdateData, String> {
    let current = parse_version(current_version, "current binary version")?;
    let release = resolve_release(transport, target)?;
    let base = UpdateData {
        status: "up-to-date",
        current_version: current.to_string(),
        latest_version: release.version.to_string(),
        target: target.to_string(),
        artifact: release.artifact_name.clone(),
        sha256: None,
        installed_path: None,
    };

    if release.version == current {
        return Ok(base);
    }
    if release.version < current {
        return Ok(UpdateData {
            status: "newer",
            ..base
        });
    }
    if check_only {
        return Ok(UpdateData {
            status: "update-available",
            ..base
        });
    }

    let executable = match install_path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe()
            .map_err(|error| format!("could not locate the running executable: {error}"))?,
    };
    let install_target = InstallTarget::preflight(&executable)?;

    if progress {
        eprintln!("Downloading {}...", release.artifact_name);
    }
    let checksum_bytes = transport.get(&release.checksum_url, CHECKSUM_LIMIT)?;
    let expected_hash = parse_checksum(&checksum_bytes, &release.artifact_name)?;
    let archive = transport.get(&release.artifact_url, ARCHIVE_LIMIT)?;
    let actual_hash = hex_digest(&archive);
    if actual_hash != expected_hash {
        return Err(format!(
            "checksum mismatch for {}: expected {expected_hash}, got {actual_hash}",
            release.artifact_name
        ));
    }
    if progress {
        eprintln!("Checksum verified; installing atomically...");
    }
    install_target.install(&archive, target, &release.version, probe)?;

    Ok(UpdateData {
        status: "updated",
        sha256: Some(actual_hash),
        installed_path: Some(executable.display().to_string()),
        ..base
    })
}

fn release_target() -> Result<&'static str, String> {
    target_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else {
            "other"
        },
    )
}

fn target_for(os: &str, arch: &str, env: &str) -> Result<&'static str, String> {
    match (os, arch, env) {
        ("linux", "x86_64", "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64", "gnu") => Ok("aarch64-unknown-linux-gnu"),
        _ => Err(format!(
            "no published self-update build for {arch}-{os}-{env}; install manually"
        )),
    }
}

fn resolve_release(transport: &dyn Transport, target: &str) -> Result<Release, String> {
    let bytes = transport.get(LATEST_RELEASE_URL, METADATA_LIMIT)?;
    let metadata: GithubRelease = serde_json::from_slice(&bytes)
        .map_err(|error| format!("GitHub returned invalid release metadata: {error}"))?;
    if metadata.draft || metadata.prerelease {
        return Err("GitHub's latest release is not a stable published release".into());
    }

    let tag_version = metadata.tag_name.strip_prefix('v').ok_or_else(|| {
        format!(
            "release tag `{}` does not start with `v`",
            metadata.tag_name
        )
    })?;
    let version = parse_version(tag_version, "latest release tag")?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(format!(
            "latest release tag `{}` is not a plain stable version",
            metadata.tag_name
        ));
    }
    if metadata.tag_name != format!("v{version}") {
        return Err(format!(
            "release tag `{}` is not canonical semantic version syntax",
            metadata.tag_name
        ));
    }

    let artifact_name = format!("stitch-v{version}-{target}.tar.gz");
    let checksum_name = format!("{artifact_name}.sha256");
    let artifact_url = expected_asset(&metadata.assets, &metadata.tag_name, &artifact_name)?;
    let checksum_url = expected_asset(&metadata.assets, &metadata.tag_name, &checksum_name)?;

    Ok(Release {
        version,
        artifact_name,
        artifact_url,
        checksum_url,
    })
}

fn expected_asset(assets: &[GithubAsset], tag: &str, name: &str) -> Result<String, String> {
    let matches: Vec<&GithubAsset> = assets.iter().filter(|asset| asset.name == name).collect();
    let [asset] = matches.as_slice() else {
        return Err(format!(
            "release {tag} must contain exactly one `{name}` asset (found {})",
            matches.len()
        ));
    };
    let expected = format!("{RELEASE_BASE_URL}/{tag}/{name}");
    if asset.browser_download_url != expected {
        return Err(format!(
            "release asset `{name}` has an unexpected download origin"
        ));
    }
    Ok(expected)
}

fn parse_version(value: &str, label: &str) -> Result<Version, String> {
    Version::parse(value).map_err(|error| format!("{label} `{value}` is invalid: {error}"))
}

fn parse_checksum(bytes: &[u8], artifact_name: &str) -> Result<String, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "release checksum is not valid UTF-8".to_string())?;
    let line = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .strip_suffix('\r')
        .unwrap_or_else(|| text.strip_suffix('\n').unwrap_or(text));
    if line.contains('\n') || line.contains('\r') {
        return Err("release checksum must contain exactly one line".into());
    }
    let (hash, filename) = line
        .split_once("  ")
        .ok_or_else(|| "release checksum is not in sha256sum format".to_string())?;
    if filename != artifact_name {
        return Err(format!(
            "release checksum names `{filename}`, expected `{artifact_name}`"
        ));
    }
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("release checksum is not a lowercase SHA-256 digest".into());
    }
    Ok(hash.to_string())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

struct InstallTarget {
    path: PathBuf,
    parent: PathBuf,
    file_identity: Identity,
    parent_identity: Identity,
    mode: u32,
    gid: u32,
}

impl InstallTarget {
    fn preflight(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "running executable {} is not a regular file",
                path.display()
            ));
        }
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(format!(
                "running executable {} is owned by uid {}, not effective uid {effective_uid}",
                path.display(),
                metadata.uid()
            ));
        }
        if metadata.nlink() != 1 {
            return Err(format!(
                "running executable {} has {} hard links; refusing ambiguous replacement",
                path.display(),
                metadata.nlink()
            ));
        }
        let mode = metadata.mode();
        if mode & 0o7000 != 0 {
            return Err(format!(
                "running executable {} has special permission bits set",
                path.display()
            ));
        }
        let mut attributes = xattr::list(path)
            .map_err(|error| {
                format!(
                    "cannot inspect extended attributes on {}: {error}",
                    path.display()
                )
            })?
            .peekable();
        if attributes.peek().is_some() {
            return Err(format!(
                "running executable {} has extended attributes (ACLs, capabilities, or security labels); refusing to discard them",
                path.display()
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| format!("running executable {} has no parent", path.display()))?
            .to_path_buf();
        let parent_metadata = fs::metadata(&parent)
            .map_err(|error| format!("cannot inspect {}: {error}", parent.display()))?;
        if !parent_metadata.is_dir() {
            return Err(format!("{} is not a directory", parent.display()));
        }
        Ok(Self {
            path: path.to_path_buf(),
            parent,
            file_identity: Identity::from(&metadata),
            parent_identity: Identity::from(&parent_metadata),
            mode: mode & 0o777,
            gid: metadata.gid(),
        })
    }

    fn revalidate(&self) -> Result<(), String> {
        let file = fs::symlink_metadata(&self.path)
            .map_err(|error| format!("cannot revalidate {}: {error}", self.path.display()))?;
        let parent = fs::metadata(&self.parent)
            .map_err(|error| format!("cannot revalidate {}: {error}", self.parent.display()))?;
        if Identity::from(&file) != self.file_identity
            || Identity::from(&parent) != self.parent_identity
            || !file.file_type().is_file()
            || file.uid() != unsafe { libc::geteuid() }
            || file.gid() != self.gid
            || file.nlink() != 1
            || file.mode() & 0o777 != self.mode
        {
            return Err(
                "the executable or its parent directory changed during the update; retry".into(),
            );
        }
        if xattr::list(&self.path)
            .map_err(|error| {
                format!(
                    "cannot revalidate extended attributes on {}: {error}",
                    self.path.display()
                )
            })?
            .next()
            .is_some()
        {
            return Err(
                "the executable gained extended attributes during the update; retry".into(),
            );
        }
        Ok(())
    }

    fn install(
        &self,
        archive: &[u8],
        target: &str,
        expected_version: &Version,
        probe: &dyn CandidateProbe,
    ) -> Result<(), String> {
        self.revalidate()?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.parent.join(format!(
            ".stitch-update.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut temp = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
            .map_err(|error| {
                format!(
                    "cannot stage update beside {}: {error}",
                    self.path.display()
                )
            })?;

        let mut committed = false;
        let result = (|| {
            extract_binary(archive, &mut temp)?;
            validate_elf(&mut temp, target)?;
            let chown_result = unsafe { libc::fchown(temp.as_raw_fd(), libc::geteuid(), self.gid) };
            if chown_result != 0 {
                return Err(format!(
                    "cannot preserve executable group ownership: {}",
                    std::io::Error::last_os_error()
                ));
            }
            temp.set_permissions(fs::Permissions::from_mode(self.mode))
                .map_err(|error| format!("cannot set update permissions: {error}"))?;
            temp.sync_all()
                .map_err(|error| format!("cannot sync staged update: {error}"))?;
            let staged_metadata = temp
                .metadata()
                .map_err(|error| format!("cannot inspect staged update: {error}"))?;
            if staged_metadata.gid() != self.gid || staged_metadata.mode() & 0o777 != self.mode {
                return Err("staged update did not preserve ownership and permissions".into());
            }
            if xattr::list(&temp_path)
                .map_err(|error| format!("cannot inspect staged update attributes: {error}"))?
                .next()
                .is_some()
            {
                return Err(
                    "staged update inherited extended attributes; refusing metadata drift".into(),
                );
            }
            probe.probe(&temp_path, expected_version)?;
            self.revalidate()?;
            fs::rename(&temp_path, &self.path).map_err(|error| {
                format!("cannot replace {} atomically: {error}", self.path.display())
            })?;
            committed = true;
            File::open(&self.parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    format!(
                        "updated {}, but could not sync its directory: {error}; the new binary remains installed",
                        self.path.display()
                    )
                })
        })();
        drop(temp);

        if !committed
            && let Err(cleanup_error) = fs::remove_file(&temp_path)
            && cleanup_error.kind() != std::io::ErrorKind::NotFound
        {
            return match result {
                Ok(()) => Err(format!(
                    "could not remove staged update {}: {cleanup_error}",
                    temp_path.display()
                )),
                Err(error) => Err(format!(
                    "{error}; also could not remove staged update {}: {cleanup_error}",
                    temp_path.display()
                )),
            };
        }
        result
    }
}

trait CandidateProbe {
    fn probe(&self, path: &Path, expected_version: &Version) -> Result<(), String>;
}

struct VersionProbe;

impl CandidateProbe for VersionProbe {
    fn probe(&self, path: &Path, expected_version: &Version) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| "staged binary has no parent directory".to_string())?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let output_path = parent.join(format!(
            ".stitch-update.{}.{}.version",
            std::process::id(),
            sequence
        ));
        let mut output_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&output_path)
            .map_err(|error| format!("cannot stage version probe output: {error}"))?;

        let result = (|| {
            let mut child = Command::new(path)
                .arg("--version")
                .env_clear()
                .stdin(Stdio::null())
                .stdout(
                    output_file
                        .try_clone()
                        .map_err(|error| format!("cannot capture staged version: {error}"))?,
                )
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("staged binary cannot start on this host: {error}"))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            enum WaitResult {
                Exited(std::process::ExitStatus),
                Failed(String),
            }
            let wait_result = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break WaitResult::Exited(status),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        break WaitResult::Failed(
                            "staged binary did not answer `--version` within 5 seconds".into(),
                        );
                    }
                    Err(error) => {
                        break WaitResult::Failed(format!(
                            "could not wait for staged binary: {error}"
                        ));
                    }
                }
            };
            let status = match wait_result {
                WaitResult::Exited(status) => status,
                WaitResult::Failed(mut message) => {
                    if let Err(error) = child.kill() {
                        message.push_str(&format!("; could not kill it: {error}"));
                    }
                    if let Err(error) = child.wait() {
                        message.push_str(&format!("; could not reap it: {error}"));
                    }
                    return Err(message);
                }
            };
            if !status.success() {
                return Err(format!(
                    "staged binary failed its `--version` probe with {status}"
                ));
            }
            output_file
                .seek(SeekFrom::Start(0))
                .map_err(|error| format!("cannot read staged binary version: {error}"))?;
            let mut output = Vec::new();
            output_file
                .by_ref()
                .take(1025)
                .read_to_end(&mut output)
                .map_err(|error| format!("could not read staged binary version: {error}"))?;
            if output.len() > 1024 {
                return Err("staged binary returned oversized version output".into());
            }
            let output = std::str::from_utf8(&output)
                .map_err(|_| "staged binary returned non-UTF-8 version output".to_string())?;
            let expected = format!("stitch {expected_version}");
            if output.trim() != expected {
                return Err(format!(
                    "staged binary reported version `{}`, expected `{expected}`",
                    output.trim()
                ));
            }
            Ok(())
        })();
        drop(output_file);
        if let Err(cleanup_error) = fs::remove_file(&output_path)
            && cleanup_error.kind() != std::io::ErrorKind::NotFound
        {
            return match result {
                Ok(()) => Err(format!(
                    "could not remove version probe output {}: {cleanup_error}",
                    output_path.display()
                )),
                Err(error) => Err(format!(
                    "{error}; also could not remove version probe output {}: {cleanup_error}",
                    output_path.display()
                )),
            };
        }
        result
    }
}

fn extract_binary(archive_bytes: &[u8], output: &mut File) -> Result<(), String> {
    let decoder = GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("release archive is invalid: {error}"))?;
    let mut found = false;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| format!("release archive entry is invalid: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("release archive path is invalid: {error}"))?;
        if found {
            return Err("release archive contains extra entries".into());
        }
        if path.as_ref() != Path::new("stitch") {
            return Err(format!(
                "release archive contains unexpected entry `{}`",
                path.display()
            ));
        }
        if !entry.header().entry_type().is_file() {
            return Err("release archive's `stitch` entry is not a regular file".into());
        }
        let size = entry
            .header()
            .size()
            .map_err(|error| format!("release archive has an invalid size: {error}"))?;
        if size > BINARY_LIMIT {
            return Err(format!(
                "release binary is too large ({size} bytes; limit is {BINARY_LIMIT})"
            ));
        }
        let copied = std::io::copy(&mut entry, output)
            .map_err(|error| format!("could not read release binary: {error}"))?;
        if copied != size {
            return Err(format!(
                "release binary is truncated (expected {size} bytes, read {copied})"
            ));
        }
        found = true;
    }
    if !found {
        return Err("release archive does not contain `stitch`".into());
    }
    Ok(())
}

fn validate_elf(file: &mut File, target: &str) -> Result<(), String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot inspect staged binary: {error}"))?;
    let mut header = [0_u8; 20];
    file.read_exact(&mut header)
        .map_err(|error| format!("staged binary has no complete ELF header: {error}"))?;
    if &header[0..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err("staged binary is not a 64-bit little-endian ELF executable".into());
    }
    let machine = u16::from_le_bytes([header[18], header[19]]);
    let expected_machine = match target {
        "x86_64-unknown-linux-gnu" => 62,
        "aarch64-unknown-linux-gnu" => 183,
        _ => return Err(format!("unsupported release target `{target}`")),
    };
    if machine != expected_machine {
        return Err(format!(
            "staged binary has ELF machine {machine}, expected {expected_machine} for {target}"
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot rewind staged binary: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;

    #[derive(Default)]
    struct FakeTransport {
        responses: BTreeMap<String, Vec<u8>>,
    }

    impl Transport for FakeTransport {
        fn get(&self, url: &str, _limit: usize) -> Result<Vec<u8>, String> {
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| format!("unexpected request: {url}"))
        }
    }

    struct AcceptProbe;

    impl CandidateProbe for AcceptProbe {
        fn probe(&self, _path: &Path, _expected_version: &Version) -> Result<(), String> {
            Ok(())
        }
    }

    fn release_json(version: &str, target: &str) -> Vec<u8> {
        let artifact = format!("stitch-v{version}-{target}.tar.gz");
        serde_json::to_vec(&serde_json::json!({
            "tag_name": format!("v{version}"),
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": artifact,
                    "browser_download_url": format!("{RELEASE_BASE_URL}/v{version}/{artifact}")
                },
                {
                    "name": format!("{artifact}.sha256"),
                    "browser_download_url": format!("{RELEASE_BASE_URL}/v{version}/{artifact}.sha256")
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn published_target_mapping_is_exact() {
        assert_eq!(
            target_for("linux", "x86_64", "gnu").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            target_for("linux", "aarch64", "gnu").unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        assert!(target_for("linux", "x86_64", "musl").is_err());
        assert!(target_for("macos", "x86_64", "gnu").is_err());
        assert!(target_for("linux", "arm", "gnu").is_err());
    }

    #[test]
    fn check_reports_available_without_downloading_assets() {
        let target = "x86_64-unknown-linux-gnu";
        let mut transport = FakeTransport::default();
        transport
            .responses
            .insert(LATEST_RELEASE_URL.into(), release_json("9.0.0", target));
        let result =
            run_with(&transport, true, false, "1.0.0", target, None, &AcceptProbe).unwrap();
        assert_eq!(result.status, "update-available");
        assert_eq!(result.latest_version, "9.0.0");
    }

    #[test]
    fn equal_and_older_releases_never_download_or_install() {
        let target = "x86_64-unknown-linux-gnu";
        for (latest, status) in [("1.0.0", "up-to-date"), ("0.9.0", "newer")] {
            let mut transport = FakeTransport::default();
            transport
                .responses
                .insert(LATEST_RELEASE_URL.into(), release_json(latest, target));
            let result = run_with(
                &transport,
                false,
                false,
                "1.0.0",
                target,
                None,
                &AcceptProbe,
            )
            .unwrap();
            assert_eq!(result.status, status);
        }
    }

    #[test]
    fn release_requires_stable_canonical_tag_and_exact_assets() {
        let target = "x86_64-unknown-linux-gnu";
        for metadata in [
            serde_json::json!({"tag_name":"1.2.3","draft":false,"prerelease":false,"assets":[]}),
            serde_json::json!({"tag_name":"v1.2.3-rc.1","draft":false,"prerelease":true,"assets":[]}),
            serde_json::json!({"tag_name":"v1.2.3+build","draft":false,"prerelease":false,"assets":[]}),
            serde_json::json!({"tag_name":"v1.2.3","draft":true,"prerelease":false,"assets":[]}),
        ] {
            let mut transport = FakeTransport::default();
            transport.responses.insert(
                LATEST_RELEASE_URL.into(),
                serde_json::to_vec(&metadata).unwrap(),
            );
            assert!(resolve_release(&transport, target).is_err());
        }

        let mut transport = FakeTransport::default();
        transport.responses.insert(
            LATEST_RELEASE_URL.into(),
            serde_json::to_vec(&serde_json::json!({
                "tag_name":"v1.2.3",
                "draft":false,
                "prerelease":false,
                "assets":[]
            }))
            .unwrap(),
        );
        assert!(resolve_release(&transport, target).is_err());
    }

    #[test]
    fn checksum_parser_is_strict() {
        let name = "stitch-v1.2.3-x86_64-unknown-linux-gnu.tar.gz";
        let hash = "a".repeat(64);
        assert_eq!(
            parse_checksum(format!("{hash}  {name}\n").as_bytes(), name).unwrap(),
            hash
        );
        assert!(parse_checksum(format!("{hash} *{name}\n").as_bytes(), name).is_err());
        assert!(parse_checksum(format!("{hash}  other\n").as_bytes(), name).is_err());
        assert!(parse_checksum(format!("{}  {name}\n", "A".repeat(64)).as_bytes(), name).is_err());
        assert!(parse_checksum(format!("{hash}  {name}\nextra\n").as_bytes(), name).is_err());
    }

    fn tar_gz(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut compressed = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            for (path, kind, contents) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(*kind);
                header.set_mode(0o755);
                header.set_size(contents.len() as u64);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, *contents)
                    .expect("append archive entry");
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        compressed
    }

    fn elf(machine: u16, marker: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0_u8; 20];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes.extend_from_slice(marker);
        bytes
    }

    #[test]
    fn extraction_accepts_only_one_root_regular_file() {
        let good = tar_gz(&[("stitch", tar::EntryType::Regular, b"binary")]);
        let temp = tempfile::tempfile().unwrap();
        extract_binary(&good, &mut temp.try_clone().unwrap()).unwrap();

        for bad in [
            tar_gz(&[("dir/stitch", tar::EntryType::Regular, b"x")]),
            tar_gz(&[("stitch", tar::EntryType::Symlink, b"")]),
            tar_gz(&[
                ("stitch", tar::EntryType::Regular, b"x"),
                ("extra", tar::EntryType::Regular, b"y"),
            ]),
        ] {
            let mut output = tempfile::tempfile().unwrap();
            assert!(extract_binary(&bad, &mut output).is_err());
        }
    }

    #[test]
    fn elf_validation_checks_target_machine() {
        for (target, machine) in [
            ("x86_64-unknown-linux-gnu", 62_u16),
            ("aarch64-unknown-linux-gnu", 183_u16),
        ] {
            let mut header = [0_u8; 20];
            header[0..4].copy_from_slice(b"\x7fELF");
            header[4] = 2;
            header[5] = 1;
            header[18..20].copy_from_slice(&machine.to_le_bytes());
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(&header).unwrap();
            validate_elf(&mut file, target).unwrap();
        }
    }

    #[test]
    fn verified_update_atomically_replaces_test_executable() {
        let target = "x86_64-unknown-linux-gnu";
        let version = "2.0.0";
        let artifact = format!("stitch-v{version}-{target}.tar.gz");
        let candidate = elf(62, b"new");
        let archive = tar_gz(&[("stitch", tar::EntryType::Regular, &candidate)]);
        let hash = hex_digest(&archive);
        let mut transport = FakeTransport::default();
        transport
            .responses
            .insert(LATEST_RELEASE_URL.into(), release_json(version, target));
        transport.responses.insert(
            format!("{RELEASE_BASE_URL}/v{version}/{artifact}.sha256"),
            format!("{hash}  {artifact}\n").into_bytes(),
        );
        transport
            .responses
            .insert(format!("{RELEASE_BASE_URL}/v{version}/{artifact}"), archive);

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("stitch");
        fs::write(&executable, b"old").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let original_gid = fs::metadata(&executable).unwrap().gid();

        let result = run_with(
            &transport,
            false,
            false,
            "1.0.0",
            target,
            Some(&executable),
            &AcceptProbe,
        )
        .unwrap();
        assert_eq!(result.status, "updated");
        assert_eq!(fs::read(&executable).unwrap(), candidate);
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::metadata(&executable).unwrap().gid(), original_gid);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn executable_with_extended_attributes_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("stitch");
        fs::write(&executable, b"old").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        xattr::set(&executable, "user.stitch-test", b"preserve-me").unwrap();

        let error = InstallTarget::preflight(&executable)
            .err()
            .expect("extended attributes must be refused");
        assert!(error.contains("extended attributes"));
        assert_eq!(
            xattr::get(&executable, "user.stitch-test").unwrap(),
            Some(b"preserve-me".to_vec())
        );
    }

    #[test]
    fn checksum_failure_leaves_test_executable_untouched() {
        let target = "x86_64-unknown-linux-gnu";
        let version = "2.0.0";
        let artifact = format!("stitch-v{version}-{target}.tar.gz");
        let archive = tar_gz(&[("stitch", tar::EntryType::Regular, &elf(62, b"untrusted"))]);
        let mut transport = FakeTransport::default();
        transport
            .responses
            .insert(LATEST_RELEASE_URL.into(), release_json(version, target));
        transport.responses.insert(
            format!("{RELEASE_BASE_URL}/v{version}/{artifact}.sha256"),
            format!("{}  {artifact}\n", "0".repeat(64)).into_bytes(),
        );
        transport
            .responses
            .insert(format!("{RELEASE_BASE_URL}/v{version}/{artifact}"), archive);

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("stitch");
        fs::write(&executable, b"old").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let error = run_with(
            &transport,
            false,
            false,
            "1.0.0",
            target,
            Some(&executable),
            &AcceptProbe,
        )
        .unwrap_err();
        assert!(error.contains("checksum mismatch"));
        assert_eq!(fs::read(&executable).unwrap(), b"old");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn header_only_elf_failing_version_probe_leaves_executable_untouched() {
        let target = "x86_64-unknown-linux-gnu";
        let version = "2.0.0";
        let artifact = format!("stitch-v{version}-{target}.tar.gz");
        let archive = tar_gz(&[(
            "stitch",
            tar::EntryType::Regular,
            &elf(62, b"not really executable"),
        )]);
        let hash = hex_digest(&archive);
        let mut transport = FakeTransport::default();
        transport
            .responses
            .insert(LATEST_RELEASE_URL.into(), release_json(version, target));
        transport.responses.insert(
            format!("{RELEASE_BASE_URL}/v{version}/{artifact}.sha256"),
            format!("{hash}  {artifact}\n").into_bytes(),
        );
        transport
            .responses
            .insert(format!("{RELEASE_BASE_URL}/v{version}/{artifact}"), archive);

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("stitch");
        fs::write(&executable, b"old").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let error = run_with(
            &transport,
            false,
            false,
            "1.0.0",
            target,
            Some(&executable),
            &VersionProbe,
        )
        .unwrap_err();
        assert!(error.contains("cannot start") || error.contains("failed its `--version` probe"));
        assert_eq!(fs::read(&executable).unwrap(), b"old");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
