//! GitHub releases access for the self-updater, behind a trait so the
//! updater's decision logic is unit-testable without the network.

use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub download_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

pub trait GithubReleases: Send {
    fn latest_release(&self, repo: &str) -> Result<Release, String>;
    fn download(&self, url: &str, dest: &Path) -> Result<(), String>;
}

pub struct UreqGithubReleases {
    agent: ureq::Agent,
}

impl UreqGithubReleases {
    pub fn new() -> Self {
        UreqGithubReleases {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .build(),
        }
    }
}

impl Default for UreqGithubReleases {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubReleases for UreqGithubReleases {
    fn latest_release(&self, repo: &str) -> Result<Release, String> {
        let response: serde_json::Value = self
            .agent
            .get(&format!("https://api.github.com/repos/{repo}/releases/latest"))
            .set("User-Agent", "soulrust-updater")
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| format!("release lookup failed: {e}"))?
            .into_json()
            .map_err(|e| format!("release response is not json: {e}"))?;

        let tag = response["tag_name"]
            .as_str()
            .ok_or("release response missing tag_name")?
            .to_owned();
        let assets = response["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|asset| {
                Some(Asset {
                    name: asset["name"].as_str()?.to_owned(),
                    download_url: asset["browser_download_url"].as_str()?.to_owned(),
                })
            })
            .collect();
        Ok(Release { tag, assets })
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        let response = self
            .agent
            .get(url)
            .set("User-Agent", "soulrust-updater")
            .call()
            .map_err(|e| format!("download failed: {e}"))?;
        let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
        std::io::copy(&mut response.into_reader(), &mut file).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The release asset name expected for this platform, e.g.
/// `soulrust-x86_64-linux`.
pub fn expected_asset_name() -> String {
    format!("soulrust-{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

pub fn pick_asset<'a>(release: &'a Release, expected: &str) -> Option<&'a Asset> {
    release.assets.iter().find(|a| a.name == expected)
}

/// Compares a release tag (with or without a leading `v`) to the running
/// version, both as semver.
pub fn is_newer(tag: &str, current: &str) -> Result<bool, String> {
    let tag_version = semver::Version::parse(tag.trim_start_matches('v'))
        .map_err(|e| format!("release tag {tag:?} is not semver: {e}"))?;
    let current_version = semver::Version::parse(current)
        .map_err(|e| format!("current version {current:?} is not semver: {e}"))?;
    Ok(tag_version > current_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_strips_v_prefix() {
        assert!(is_newer("v0.2.0", "0.1.0").unwrap());
        assert!(is_newer("0.1.1", "0.1.0").unwrap());
        assert!(!is_newer("v0.1.0", "0.1.0").unwrap());
        assert!(!is_newer("v0.0.9", "0.1.0").unwrap());
        assert!(is_newer("not-a-version", "0.1.0").is_err());
    }

    #[test]
    fn version_comparison_orders_prereleases_below_their_release() {
        // Semver: a pre-release is LOWER than the same release, so we must not
        // "update" from a stable build to its own pre-release.
        assert!(!is_newer("v0.1.0-beta", "0.1.0").unwrap(), "prerelease is not newer than release");
        assert!(is_newer("v0.1.0", "0.1.0-beta").unwrap(), "release is newer than its prerelease");
        // Ordering among pre-releases, and a newer pre-release vs an older release.
        assert!(is_newer("v0.2.0-beta", "0.2.0-alpha").unwrap());
        assert!(is_newer("v0.2.0-alpha", "0.1.0").unwrap(), "newer prerelease beats older release");
        assert!(!is_newer("v0.1.0-alpha", "0.1.0-beta").unwrap());
    }

    #[test]
    fn picks_only_the_exact_platform_asset() {
        let release = Release {
            tag: "v0.2.0".into(),
            assets: vec![
                Asset { name: "soulrust-x86_64-linux".into(), download_url: "u1".into() },
                Asset { name: "soulrust-aarch64-macos".into(), download_url: "u2".into() },
                Asset { name: "checksums.txt".into(), download_url: "u3".into() },
            ],
        };
        assert_eq!(
            pick_asset(&release, "soulrust-aarch64-macos").unwrap().download_url,
            "u2"
        );
        assert!(pick_asset(&release, "soulrust-riscv64-linux").is_none());
    }
}
