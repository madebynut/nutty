//! Optional check for a newer Nutty release on GitHub.
//!
//! One anonymous GET to the public releases API per launch. Nothing about the user,
//! their connections or configuration is sent; the request carries only the
//! `User-Agent` GitHub requires. Users turn it off in Settings → General.

use anyhow::{Result, ensure};
use serde::Deserialize;
use std::time::Duration;

pub const REPOSITORY: &str = "madebynut/nutty";
/// Thirty releases is far more than one response needs; the cap bounds memory.
const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: semver::Version,
    /// Release page built from the validated tag, never a URL taken from the response.
    pub url: String,
}

#[derive(Deserialize)]
struct Listed {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The newest published release worth offering to a user running `current`.
///
/// Stable builds are only offered stable releases. Pre-release builds are offered
/// both, so beta testers move on to the final release. Drafts, tags that are not
/// `v<semver>`, build metadata and packaging trials (`-test.N`) are ignored.
pub fn newest(current: &str, releases: &[(String, bool, bool)]) -> Option<Release> {
    let current = semver::Version::parse(current).ok()?;
    releases
        .iter()
        .filter(|(_, draft, _)| !draft)
        .filter_map(|(tag, _, prerelease)| {
            let version = semver::Version::parse(tag.strip_prefix('v')?).ok()?;
            let unstable = *prerelease || !version.pre.is_empty();
            if !version.build.is_empty()
                || version.pre.as_str().contains("test")
                || (unstable && current.pre.is_empty())
            {
                return None;
            }
            Some(version)
        })
        .filter(|version| *version > current)
        .max()
        .map(|version| Release {
            url: format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"),
            version,
        })
}

/// Asks GitHub for recent releases. Errors are for logging only; callers stay quiet.
pub async fn check(current: &str) -> Result<Option<Release>> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(3))
        .user_agent(format!("Nutty/{current}"))
        .build()?;
    let mut response = client
        .get(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases?per_page=30"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "GitHub releases returned HTTP {}",
        response.status().as_u16()
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= MAX_RESPONSE,
            "GitHub releases response is too large"
        );
        body.extend_from_slice(&chunk);
    }
    let listed: Vec<Listed> = serde_json::from_slice(&body)?;
    let releases: Vec<_> = listed
        .into_iter()
        .map(|release| (release.tag_name, release.draft, release.prerelease))
        .collect();
    Ok(newest(current, &releases))
}
