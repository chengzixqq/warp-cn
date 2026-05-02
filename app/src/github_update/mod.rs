//! Lightweight update checker against the fork's GitHub Releases.
//!
//! Independent from `crate::autoupdate` (which talks to warp.dev's release
//! infrastructure and is unreachable on a fork). Powers only the Settings
//! page Version row: shows current version, lets the user query the
//! latest release on `Heartcoolman/warp-cn`, and routes the "open release"
//! click to the browser. No download, no install.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use std::cmp::Ordering;
use std::time::Duration;
use warp_core::channel::ChannelState;
use warpui::{AppContext, Entity, SingletonEntity};

const REPO_API_URL: &str =
    "https://api.github.com/repos/Heartcoolman/warp-cn/releases/latest";
const REPO_RELEASES_URL: &str = "https://github.com/Heartcoolman/warp-cn/releases";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "warp-cn-update-check/0.1";
const ACCEPT_HEADER: &str = "application/vnd.github+json";

#[derive(Clone, Debug)]
pub enum GithubUpdateState {
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable { tag: String, html_url: String },
    Error,
}

impl GithubUpdateState {
    pub fn new() -> Self {
        Self::Idle
    }

    pub fn register(ctx: &mut AppContext) {
        ctx.add_singleton_model(|_ctx| Self::new());
    }

    pub fn trigger_check(ctx: &mut AppContext) {
        Self::handle(ctx).update(ctx, |state, ctx| {
            if matches!(state, Self::Checking) {
                return;
            }

            *state = Self::Checking;
            ctx.notify();

            ctx.spawn(
                async { check_for_update().await },
                |state, result, ctx| {
                    *state = match result {
                        Ok(CheckResult::UpToDate) => Self::UpToDate,
                        Ok(CheckResult::UpdateAvailable { tag, html_url }) => {
                            Self::UpdateAvailable { tag, html_url }
                        }
                        Err(err) => {
                            log::warn!("GitHub update check failed: {err:#}");
                            Self::Error
                        }
                    };
                    ctx.notify();
                },
            );
        });
    }
}

impl Default for GithubUpdateState {
    fn default() -> Self {
        Self::new()
    }
}

impl Entity for GithubUpdateState {
    type Event = ();
}

impl SingletonEntity for GithubUpdateState {}

enum CheckResult {
    UpToDate,
    UpdateAvailable { tag: String, html_url: String },
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: Option<String>,
    #[serde(default)]
    prerelease: bool,
}

async fn check_for_update() -> Result<CheckResult> {
    // Use a fresh `reqwest::Client` rather than `http_client::Client`: the
    // shared client injects Warp telemetry headers (`X-Warp-Client-ID`, OS
    // info, etc.) on every native request, which we must not leak to a
    // third-party host like api.github.com.
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("Failed to construct reqwest client")?;
    let mut request = client
        .get(REPO_API_URL)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, ACCEPT_HEADER);
    if let Some(token) = resolve_github_token() {
        request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .context("Failed to fetch latest GitHub release")?
        .error_for_status()
        .context("GitHub latest release request failed")?;

    let release: GithubRelease = response
        .json()
        .await
        .context("Failed to parse latest GitHub release response")?;

    if release.prerelease {
        log::info!("Latest GitHub release is a prerelease; treating as up to date");
        return Ok(CheckResult::UpToDate);
    }

    let tag = release.tag_name;
    if tag.is_empty() {
        bail!("GitHub release missing tag_name");
    }

    let html_url = release
        .html_url
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| format!("{REPO_RELEASES_URL}/tag/{tag}"));

    let latest = ParsedGithubVersion::parse(&tag)
        .with_context(|| format!("Failed to parse latest GitHub release tag {tag}"))?;

    let current_version = ChannelState::app_version().or(option_env!("GIT_RELEASE_TAG"));
    match current_version {
        None => Ok(CheckResult::UpdateAvailable { tag, html_url }),
        Some(current_tag) => {
            let current = ParsedGithubVersion::parse(current_tag).with_context(|| {
                format!("Failed to parse current version tag {current_tag}")
            })?;
            if latest.cmp_numeric(&current) == Ordering::Greater {
                Ok(CheckResult::UpdateAvailable { tag, html_url })
            } else {
                Ok(CheckResult::UpToDate)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedGithubVersion {
    major: u32,
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    channel: String,
    patch: u32,
}

impl ParsedGithubVersion {
    /// Accepts two tag shapes:
    ///   - `v<major>.<YYYY>.<MM>.<DD>.<HH>.<mm>.<channel>_<patch>`  (7-segment, CI)
    ///   - `v<major>.<YYYY>.<MM>.<DD>-<channel>.<patch>`            (5-segment, GitHub Releases)
    fn parse(tag: &str) -> Result<Self> {
        let body = tag
            .strip_prefix('v')
            .ok_or_else(|| anyhow!("Version tag must start with 'v'"))?;
        let parts: Vec<&str> = body.split('.').collect();

        match parts.len() {
            5 => {
                let (day, channel) = parts[3]
                    .split_once('-')
                    .ok_or_else(|| anyhow!("4th component must be DD-channel"))?;
                Ok(Self {
                    major: parts[0].parse().context("Invalid major version")?,
                    year: parts[1].parse().context("Invalid year")?,
                    month: parts[2].parse().context("Invalid month")?,
                    day: day.parse().context("Invalid day")?,
                    hour: 0,
                    minute: 0,
                    channel: channel.to_string(),
                    patch: parts[4].parse().context("Invalid patch")?,
                })
            }
            7 => {
                let (channel, patch) = parts[6]
                    .split_once('_')
                    .ok_or_else(|| anyhow!("7th component must be <channel>_<patch>"))?;
                Ok(Self {
                    major: parts[0].parse().context("Invalid major version")?,
                    year: parts[1].parse().context("Invalid year")?,
                    month: parts[2].parse().context("Invalid month")?,
                    day: parts[3].parse().context("Invalid day")?,
                    hour: parts[4].parse().context("Invalid hour")?,
                    minute: parts[5].parse().context("Invalid minute")?,
                    channel: channel.to_string(),
                    patch: patch.parse().context("Invalid patch")?,
                })
            }
            _ => bail!("Version tag must have 5 or 7 dot-separated components"),
        }
    }

    fn cmp_numeric(&self, other: &Self) -> Ordering {
        (
            self.major,
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.patch,
        )
            .cmp(&(
                other.major,
                other.year,
                other.month,
                other.day,
                other.hour,
                other.minute,
                other.patch,
            ))
    }
}

fn resolve_github_token() -> Option<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            return Some(token);
        }
    }
    read_gh_cli_token()
}

fn read_gh_cli_token() -> Option<String> {
    let config_path = dirs::config_dir()?.join("gh").join("hosts.yml");
    let contents = std::fs::read_to_string(config_path).ok()?;
    let hosts: serde_yaml::Value = serde_yaml::from_str(&contents).ok()?;
    let github = hosts.get("github.com")?;
    if let Some(token) = github.get("oauth_token").and_then(|v| v.as_str()) {
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    let (_, first_user) = github.get("users")?.as_mapping()?.into_iter().next()?;
    first_user
        .get("oauth_token")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_7_segment() {
        let p = ParsedGithubVersion::parse("v0.2026.04.27.15.32.stable_03").unwrap();
        assert_eq!(p.major, 0);
        assert_eq!(p.year, 2026);
        assert_eq!(p.month, 4);
        assert_eq!(p.day, 27);
        assert_eq!(p.hour, 15);
        assert_eq!(p.minute, 32);
        assert_eq!(p.channel, "stable");
        assert_eq!(p.patch, 3);
    }

    #[test]
    fn parse_5_segment() {
        let p = ParsedGithubVersion::parse("v0.2026.05.02-cn.0").unwrap();
        assert_eq!(p.major, 0);
        assert_eq!(p.year, 2026);
        assert_eq!(p.month, 5);
        assert_eq!(p.day, 2);
        assert_eq!(p.hour, 0);
        assert_eq!(p.minute, 0);
        assert_eq!(p.channel, "cn");
        assert_eq!(p.patch, 0);
    }

    #[test]
    fn newer_patch_is_greater() {
        let a = ParsedGithubVersion::parse("v0.2026.04.27.15.32.stable_03").unwrap();
        let b = ParsedGithubVersion::parse("v0.2026.04.27.15.32.stable_05").unwrap();
        assert_eq!(b.cmp_numeric(&a), Ordering::Greater);
        assert_eq!(a.cmp_numeric(&b), Ordering::Less);
        assert_eq!(a.cmp_numeric(&a.clone()), Ordering::Equal);
    }

    #[test]
    fn newer_date_5_segment() {
        let a = ParsedGithubVersion::parse("v0.2026.05.01-cn.0").unwrap();
        let b = ParsedGithubVersion::parse("v0.2026.05.02-cn.0").unwrap();
        assert_eq!(b.cmp_numeric(&a), Ordering::Greater);
    }

    #[test]
    fn newer_date_is_greater() {
        let a = ParsedGithubVersion::parse("v0.2026.04.27.15.32.stable_03").unwrap();
        let b = ParsedGithubVersion::parse("v0.2026.04.28.10.00.stable_01").unwrap();
        assert_eq!(b.cmp_numeric(&a), Ordering::Greater);
    }

    #[test]
    fn missing_prefix_or_components_fails() {
        assert!(ParsedGithubVersion::parse("0.2026.04.27.15.32.stable_03").is_err());
        assert!(ParsedGithubVersion::parse("v0.2026.04.27.15.stable_03").is_err());
        assert!(ParsedGithubVersion::parse("v0.2026.04.27.15.32.stable").is_err());
        assert!(ParsedGithubVersion::parse("v0.2026.05.02cn.0").is_err());
    }
}
