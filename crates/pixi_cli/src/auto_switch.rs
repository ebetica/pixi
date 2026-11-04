//! Switch to a matching pixi when a workspace's `requires-pixi` is not
//! satisfied.
//!
//! This build is a fork (<https://github.com/ebetica/pixi>) with the
//! shared-cache patches. When a workspace pins a pixi version this binary does
//! not satisfy, pixi looks for a fork release tagged `v<version>.biohub` that
//! does, installs its binary into `$PIXI_HOME/versions/<version>/pixi` and
//! re-executes itself with that binary, the same arguments and environment.
//!
//! - An installed version that satisfies the requirement is used without any
//!   network access.
//! - Only releases of the fork are considered, never upstream pixi releases.
//!   Without a matching fork release, the usual `requires-pixi` error is shown.
//! - `PIXI_NO_AUTO_SWITCH=1` turns this off.
//! - [`SWITCHED_EXE_ENV`] marks the re-executed process, so a binary that still
//!   does not satisfy the requirement fails instead of switching again.

use std::{
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};

use miette::{Context, IntoDiagnostic};
use rattler_conda_types::{Version, VersionSpec};
use serde::Deserialize;

/// The fork whose releases are installed. Never upstream.
const REPO: &str = "ebetica/pixi";
/// Fork releases are tagged `v<version>.biohub`.
const TAG_PREFIX: &str = "v";
const TAG_SUFFIX: &str = ".biohub";
/// The release asset with the binary, and `<asset>.sha256` with its checksum.
const ASSET: &str = "pixi-x86_64-unknown-linux-musl.tar.gz";

/// Set to `1` or `true` to disable switching.
pub const NO_AUTO_SWITCH_ENV: &str = "PIXI_NO_AUTO_SWITCH";
/// Set on the re-executed process to the path of the binary it runs.
pub const SWITCHED_EXE_ENV: &str = "PIXI_AUTO_SWITCHED_EXE";

/// Registers [`handle_mismatch`] with the manifest discovery.
pub fn install() {
    pixi_manifest::set_version_mismatch_handler(handle_mismatch);
}

/// Re-executes a pixi that satisfies `spec`, or returns a hint for the
/// `requires-pixi` error.
fn handle_mismatch(spec: &VersionSpec) -> Option<String> {
    if !cfg!(all(unix, target_arch = "x86_64", target_os = "linux")) {
        return None;
    }
    if matches!(
        std::env::var(NO_AUTO_SWITCH_ENV).as_deref(),
        Ok("1" | "true")
    ) {
        return None;
    }
    let current_exe = std::env::current_exe().ok();
    if let Some(switched) = std::env::var_os(SWITCHED_EXE_ENV)
        && same_file(Path::new(&switched), current_exe.as_deref())
    {
        return Some(format!(
            "pixi switched to '{}' for this requirement, but that binary does not satisfy it either; remove it and try again",
            Path::new(&switched).display()
        ));
    }
    let versions_dir = pixi_config::pixi_home()?.join("versions");

    let (version, exe) = match newest_installed(&versions_dir, spec) {
        Some(found) => found,
        // Discovery runs inside the tokio runtime, so the download gets a
        // thread with its own runtime.
        None => match std::thread::scope(|s| {
            s.spawn(|| download_matching(&versions_dir, spec))
                .join()
                .expect("the pixi download thread panicked")
        }) {
            Ok(Ok(found)) => found,
            Ok(Err(hint)) => return Some(hint),
            Err(err) => {
                return Some(format!(
                    "could not switch to a pixi release that satisfies '{spec}': {err:?}"
                ));
            }
        },
    };
    if current_exe
        .as_deref()
        .is_some_and(|current| same_file(&exe, Some(current)))
    {
        return None;
    }
    tracing::info!(
        "this workspace requires pixi '{spec}', running pixi {version} from '{}'",
        exe.display()
    );
    let err = exec(&exe);
    Some(format!(
        "could not run pixi {version} from '{}': {err}",
        exe.display()
    ))
}

#[cfg(unix)]
fn exec(exe: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(SWITCHED_EXE_ENV, exe)
        .exec()
}

#[cfg(not(unix))]
fn exec(_exe: &Path) -> std::io::Error {
    std::io::Error::other("switching pixi versions is only supported on unix")
}

fn same_file(a: &Path, b: Option<&Path>) -> bool {
    let Some(b) = b else { return false };
    match (fs_err::canonicalize(a), fs_err::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The newest version installed under `versions_dir` that satisfies `spec`.
fn newest_installed(versions_dir: &Path, spec: &VersionSpec) -> Option<(Version, PathBuf)> {
    fs_err::read_dir(versions_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = Version::from_str(entry.file_name().to_str()?).ok()?;
            let exe = entry.path().join("pixi");
            (spec.matches(&version) && exe.is_file()).then_some((version, exe))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
}

/// The parts of a GitHub release that are needed here.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
}

/// The pixi version of a fork release tag, `v<version>.biohub`.
fn tag_version(tag: &str) -> Option<Version> {
    let version = tag.strip_prefix(TAG_PREFIX)?.strip_suffix(TAG_SUFFIX)?;
    Version::from_str(version).ok()
}

/// Published fork releases that have the binary, newest first.
fn usable_releases(releases: &[Release]) -> Vec<(Version, &Release)> {
    let mut usable: Vec<_> = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter(|r| {
            let has = |name: &str| r.assets.iter().any(|a| a.name == name);
            has(ASSET) && has(&format!("{ASSET}.sha256"))
        })
        .filter_map(|r| Some((tag_version(&r.tag_name)?, r)))
        .collect();
    usable.sort_by(|a, b| b.0.cmp(&a.0));
    usable
}

/// The newest usable release that satisfies `spec`, or a hint naming the
/// releases there are.
fn select_release<'a>(
    releases: &'a [Release],
    spec: &VersionSpec,
) -> Result<(Version, &'a Release), String> {
    let usable = usable_releases(releases);
    if let Some((version, release)) = usable.iter().find(|(v, _)| spec.matches(v)) {
        return Ok((version.clone(), release));
    }
    let available = usable
        .iter()
        .map(|(v, _)| v.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(if available.is_empty() {
        format!("no release of https://github.com/{REPO} satisfies '{spec}'")
    } else {
        format!(
            "no release of https://github.com/{REPO} satisfies '{spec}'; the releases there are pixi {available}"
        )
    })
}

/// Finds and installs the newest fork release that satisfies `spec`. The
/// inner error is a hint for the user: there is no matching release.
fn download_matching(
    versions_dir: &Path,
    spec: &VersionSpec,
) -> miette::Result<Result<(Version, PathBuf), String>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .into_diagnostic()?;
    runtime.block_on(async {
        let client = pixi_utils::reqwest::reqwest_client_builder(Some(
            &pixi_config::Config::load_global(),
        ))
        .into_diagnostic()?
        .build()
        .into_diagnostic()?;
        let releases = fetch_releases(&client).await?;
        let (version, release) = match select_release(&releases, spec) {
            Ok(found) => found,
            Err(hint) => return Ok(Err(hint)),
        };
        eprintln!(
            "This workspace requires pixi '{spec}'; downloading pixi {version} from https://github.com/{REPO}/releases/tag/{}",
            release.tag_name
        );
        let exe = install_release(&client, versions_dir, &version, &release.tag_name).await?;
        Ok(Ok((version, exe)))
    })
}

async fn fetch_releases(client: &reqwest::Client) -> miette::Result<Vec<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    let mut request = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(token) = ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|t| !t.is_empty()))
    {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to list the releases at {url}"))?;
    let status = response.status();
    if !status.is_success() {
        miette::bail!(
            "failed to list the releases at {url}: HTTP {status} (set GITHUB_TOKEN if you hit the GitHub API rate limit)"
        );
    }
    response.json().await.into_diagnostic()
}

async fn download(client: &reqwest::Client, url: &str) -> miette::Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to download {url}"))?;
    Ok(response
        .bytes()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to download {url}"))?
        .to_vec())
}

/// Downloads the release binary into `<versions_dir>/<version>/pixi`.
///
/// The file only appears once its checksum and version are verified, through
/// a rename, so concurrent pixi processes never run a partial binary.
async fn install_release(
    client: &reqwest::Client,
    versions_dir: &Path,
    version: &Version,
    tag: &str,
) -> miette::Result<PathBuf> {
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let checksum = download(client, &format!("{base}/{ASSET}.sha256")).await?;
    let archive = download(client, &format!("{base}/{ASSET}")).await?;
    verify_sha256(&archive, &String::from_utf8_lossy(&checksum))?;

    let dir = versions_dir.join(version.to_string());
    fs_err::create_dir_all(&dir).into_diagnostic()?;
    let mut temp = tempfile::Builder::new()
        .prefix(".pixi.")
        .tempfile_in(&dir)
        .into_diagnostic()?;
    std::io::Write::write_all(&mut temp, &extract_pixi(&archive)?).into_diagnostic()?;
    temp.as_file().sync_all().into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs_err::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))
            .into_diagnostic()?;
    }
    // Close the write handle, or exec-ing the file fails with ETXTBSY.
    let temp = temp.into_temp_path();
    check_version(&temp, version)?;
    let exe = dir.join("pixi");
    temp.persist(&exe).into_diagnostic()?;
    Ok(exe)
}

/// Checks `data` against a `sha256sum`-style line (`<hex>  <file name>`).
fn verify_sha256(data: &[u8], checksum_file: &str) -> miette::Result<()> {
    let expected = checksum_file
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let actual: String = rattler_digest::compute_bytes_digest::<rattler_digest::Sha256>(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if expected != actual {
        miette::bail!("checksum mismatch for {ASSET}: expected '{expected}', got '{actual}'");
    }
    Ok(())
}

/// The `pixi` file from a release tarball.
fn extract_pixi(archive: &[u8]) -> miette::Result<Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().into_diagnostic()? {
        let mut entry = entry.into_diagnostic()?;
        if entry.header().entry_type().is_file()
            && entry.path().into_diagnostic()?.file_name() == Some("pixi".as_ref())
        {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).into_diagnostic()?;
            return Ok(data);
        }
    }
    miette::bail!("{ASSET} has no 'pixi' binary")
}

/// Checks that `exe --version` reports `version`, so the switched process
/// satisfies the requirement and does not switch again.
fn check_version(exe: &Path, version: &Version) -> miette::Result<()> {
    let output = std::process::Command::new(exe)
        .arg("--version")
        .env(NO_AUTO_SWITCH_ENV, "1")
        .output()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to run '{}'", exe.display()))?;
    let reported = String::from_utf8_lossy(&output.stdout);
    let reported = reported.trim();
    if reported != format!("pixi {version}") {
        miette::bail!("the release binary for pixi {version} reports '{reported}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rattler_conda_types::ParseStrictness;

    use super::*;

    fn spec(s: &str) -> VersionSpec {
        VersionSpec::from_str(s, ParseStrictness::Strict).unwrap()
    }

    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.to_string(),
            draft: false,
            prerelease: false,
            assets: vec![
                ReleaseAsset {
                    name: ASSET.to_string(),
                },
                ReleaseAsset {
                    name: format!("{ASSET}.sha256"),
                },
                ReleaseAsset {
                    name: "install.sh".to_string(),
                },
            ],
        }
    }

    fn selected(releases: &[Release], s: &str) -> Result<String, String> {
        select_release(releases, &spec(s)).map(|(_, r)| r.tag_name.clone())
    }

    #[test]
    fn tag_version_only_accepts_fork_tags() {
        assert_eq!(
            tag_version("v0.83.0.biohub"),
            Some(Version::from_str("0.83.0").unwrap())
        );
        assert_eq!(
            tag_version("v0.70.2.biohub"),
            Some(Version::from_str("0.70.2").unwrap())
        );
        assert_eq!(tag_version("v0.83.0"), None);
        assert_eq!(tag_version("v0.83.0-shared-cache"), None);
        assert_eq!(tag_version("0.83.0.biohub"), None);
        assert_eq!(tag_version("v.biohub"), None);
    }

    #[test]
    fn selects_newest_matching_release() {
        let releases = [
            release("v0.70.2.biohub"),
            release("v0.83.0.biohub"),
            release("v0.83.1.biohub"),
            release("v0.84.0.biohub"),
        ];
        assert_eq!(
            selected(&releases, ">=0.83.0,<0.84.0").unwrap(),
            "v0.83.1.biohub"
        );
        assert_eq!(
            selected(&releases, ">=0.70.2,<0.71.0").unwrap(),
            "v0.70.2.biohub"
        );
        assert_eq!(selected(&releases, ">=0.80").unwrap(), "v0.84.0.biohub");
        assert_eq!(selected(&releases, "==0.83.0").unwrap(), "v0.83.0.biohub");
    }

    #[test]
    fn ignores_upstream_drafts_prereleases_and_incomplete_releases() {
        let mut draft = release("v0.83.2.biohub");
        draft.draft = true;
        let mut prerelease = release("v0.83.3.biohub");
        prerelease.prerelease = true;
        let mut no_checksum = release("v0.83.4.biohub");
        no_checksum.assets.retain(|a| a.name == ASSET);
        let releases = [
            release("v0.83.9"),
            release("v0.83.8-shared-cache"),
            draft,
            prerelease,
            no_checksum,
            release("v0.83.0.biohub"),
        ];
        assert_eq!(
            selected(&releases, ">=0.83.0,<0.84.0").unwrap(),
            "v0.83.0.biohub"
        );
    }

    #[test]
    fn no_match_names_the_available_releases() {
        let releases = [
            release("v0.70.2.biohub"),
            release("v0.83.0.biohub"),
            release("v0.60.0"),
        ];
        let hint = selected(&releases, ">=0.65,<0.66").unwrap_err();
        assert!(hint.contains("'>=0.65,<0.66'"), "{hint}");
        assert!(
            hint.ends_with("the releases there are pixi 0.83.0, 0.70.2"),
            "{hint}"
        );
        assert!(!hint.contains("0.60.0"), "{hint}");

        let hint = selected(&[], ">=0.65").unwrap_err();
        assert_eq!(
            hint,
            format!("no release of https://github.com/{REPO} satisfies '>=0.65'")
        );
    }

    #[test]
    fn newest_installed_needs_a_binary_and_a_match() {
        let dir = tempfile::tempdir().unwrap();
        for (version, with_binary) in [
            ("0.70.2", true),
            ("0.83.0", true),
            ("0.83.1", false),
            ("junk", true),
        ] {
            let version_dir = dir.path().join(version);
            fs_err::create_dir_all(&version_dir).unwrap();
            if with_binary {
                fs_err::write(version_dir.join("pixi"), "").unwrap();
            }
        }
        let found = |s: &str| newest_installed(dir.path(), &spec(s)).map(|(v, _)| v.to_string());
        assert_eq!(found(">=0.83.0,<0.84.0").as_deref(), Some("0.83.0"));
        assert_eq!(found(">=0.70.2,<0.71.0").as_deref(), Some("0.70.2"));
        assert_eq!(found(">=0.84"), None);
        assert_eq!(
            newest_installed(&dir.path().join("missing"), &spec(">=0")),
            None
        );
    }

    #[test]
    fn verifies_sha256() {
        // sha256 of "hello"
        let line =
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  pixi.tar.gz\n";
        verify_sha256(b"hello", line).unwrap();
        verify_sha256(b"hello", &line.to_uppercase()).unwrap();
        assert!(verify_sha256(b"hellO", line).is_err());
        assert!(verify_sha256(b"hello", "").is_err());
    }

    #[test]
    fn extracts_pixi_from_tarball() {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data) in [
            ("README", b"readme".as_slice()),
            ("pixi", b"binary".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        let archive = builder.into_inner().unwrap().finish().unwrap();
        assert_eq!(extract_pixi(&archive).unwrap(), b"binary");
    }
}
