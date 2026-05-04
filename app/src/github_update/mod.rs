//! Update checker + installer against the fork's GitHub Releases.
//!
//! Independent from `crate::autoupdate` (which talks to warp.dev's release
//! infrastructure and is unreachable on a fork). Drives the Settings page
//! Version row:
//!   * Shows current version.
//!   * Polls the latest release on `Heartcoolman/warp-cn`.
//!   * Lets the user trigger an in-place download/verify/extract/replace
//!     pipeline (see [`install`]) when the release publishes a signed
//!     tarball + `.minisig`. Falls back to "Open on GitHub" otherwise.
//!
//! Auto-update path is macOS-only; on other targets the install module is
//! not compiled.

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use warp_core::channel::ChannelState;
use warp_core::paths;
use warpui::{AppContext, Entity, ModelContext, SingletonEntity, Timer};

mod auto_check;
#[cfg(target_os = "macos")]
mod install;

pub(crate) use auto_check::register as register_auto_check;

const REPO_API_URL: &str = "https://api.github.com/repos/Heartcoolman/warp-cn/releases/latest";
const REPO_RELEASES_URL: &str = "https://github.com/Heartcoolman/warp-cn/releases";
const REPO_TAGS_API: &str =
    "https://api.github.com/repos/Heartcoolman/warp-cn/git/matching-refs/tags/";
const REPO_GIT_TAGS_API: &str = "https://api.github.com/repos/Heartcoolman/warp-cn/git/tags/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "warp-cn-update-check/0.1";
const ACCEPT_HEADER: &str = "application/vnd.github+json";
/// Cache lifetime for SHA→tag resolution. SHA→tag is stable unless the user
/// retags the same commit; 24h re-validation is the right balance for honoring
/// retags without burning the GitHub unauthenticated rate budget (60/h/IP).
const VERSION_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const VERSION_CACHE_FILE: &str = "version_resolution.json";

/// Carrier for a release that has both an installable tarball and its
/// minisign signature. Surfacing this as `Some(_)` from a check tells the
/// UI it can offer "Download & Install"; `None` means fall back to the
/// browser link.
#[derive(Clone, Debug)]
pub struct InstallableRelease {
    pub tag: String,
    pub asset_url: String,
    pub sig_url: String,
}

#[derive(Clone, Debug)]
pub enum GithubUpdateState {
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable {
        tag: String,
        html_url: String,
        installable: Option<InstallableRelease>,
    },
    /// Tarball is being fetched + verified. `downloaded_bytes` is updated
    /// every ~250ms by a ticker that polls a shared atomic written by the
    /// install future's stream loop. `total_bytes` is `None` until the HTTP
    /// response's `Content-Length` is known (almost always present for
    /// GitHub release-asset redirects).
    Downloading {
        tag: String,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    /// Verification done; about to swap and relaunch. The current process
    /// will `exit(0)` very shortly after entering this state, so the user
    /// rarely sees this frame.
    Installing {
        tag: String,
    },
    Error,
}

impl GithubUpdateState {
    pub fn new() -> Self {
        Self::Idle
    }

    pub fn register(ctx: &mut AppContext) {
        ctx.add_singleton_model(|_ctx| Self::new());
    }

    /// True iff the binary was built with a baked update public key. UI uses
    /// this to decide whether to surface the install button at all; the
    /// install module enforces the same check defensively before applying.
    pub fn install_supported() -> bool {
        cfg!(target_os = "macos")
            && option_env!("WARP_UPDATE_PUBKEY").is_some_and(|s| !s.is_empty())
    }

    pub fn trigger_check(ctx: &mut AppContext) {
        Self::handle(ctx).update(ctx, |state, ctx| {
            if matches!(
                state,
                Self::Checking | Self::Downloading { .. } | Self::Installing { .. }
            ) {
                return;
            }

            *state = Self::Checking;
            ctx.notify();

            ctx.spawn(async { check_for_update().await }, |state, result, ctx| {
                *state = match result {
                    Ok(CheckResult::UpToDate) => Self::UpToDate,
                    Ok(CheckResult::UpdateAvailable {
                        tag,
                        html_url,
                        installable,
                    }) => Self::UpdateAvailable {
                        tag,
                        html_url,
                        installable,
                    },
                    Err(err) => {
                        log::warn!("GitHub update check failed: {err:#}");
                        Self::Error
                    }
                };
                ctx.notify();
            });
        });
    }

    /// Kick off the download/verify/extract/replace pipeline. macOS-only;
    /// no-ops on other platforms (and the install button is hidden).
    #[cfg(target_os = "macos")]
    pub fn trigger_install(ctx: &mut AppContext, target: InstallableRelease) {
        Self::handle(ctx).update(ctx, |state, ctx| {
            if matches!(state, Self::Downloading { .. } | Self::Installing { .. }) {
                return;
            }
            let tag = target.tag.clone();
            let progress = Arc::new(AtomicU64::new(0));
            let total = Arc::new(AtomicU64::new(0));
            let done = Arc::new(AtomicBool::new(false));

            *state = Self::Downloading {
                tag: tag.clone(),
                downloaded_bytes: 0,
                total_bytes: None,
            };
            ctx.notify();

            let install_target = target.clone();
            let install_tag = tag.clone();
            let progress_w = progress.clone();
            let total_w = total.clone();
            let done_w = done.clone();
            ctx.spawn(
                async move {
                    let r = install::run_install(install_target, progress_w, total_w).await;
                    done_w.store(true, AtomicOrdering::SeqCst);
                    r
                },
                move |state, result, ctx| {
                    match result {
                        Ok(()) => {
                            *state = Self::Installing { tag: install_tag };
                        }
                        Err(err) => {
                            log::error!("GitHub update install failed: {err:#}");
                            *state = Self::Error;
                        }
                    }
                    ctx.notify();
                },
            );

            // Re-entrant ticker that reflects atomic counters into model
            // state every ~250ms. Stops when `done` flips true (set by the
            // install future just before its callback fires) or when state
            // moves out of Downloading{tag}.
            Self::schedule_progress_tick(ctx, tag, progress, total, done);
        });
    }

    #[cfg(target_os = "macos")]
    fn schedule_progress_tick(
        ctx: &mut ModelContext<Self>,
        tag: String,
        progress: Arc<AtomicU64>,
        total: Arc<AtomicU64>,
        done: Arc<AtomicBool>,
    ) {
        if done.load(AtomicOrdering::SeqCst) {
            return;
        }
        ctx.spawn(
            async move {
                Timer::after(Duration::from_millis(250)).await;
            },
            move |state, _, ctx| {
                if let Self::Downloading {
                    tag: cur_tag,
                    downloaded_bytes,
                    total_bytes,
                } = state
                {
                    if cur_tag == &tag {
                        *downloaded_bytes = progress.load(AtomicOrdering::SeqCst);
                        let t = total.load(AtomicOrdering::SeqCst);
                        *total_bytes = if t > 0 { Some(t) } else { None };
                        ctx.notify();
                    }
                }
                Self::schedule_progress_tick(ctx, tag, progress, total, done);
            },
        );
    }

    #[cfg(not(target_os = "macos"))]
    pub fn trigger_install(_ctx: &mut AppContext, _target: InstallableRelease) {
        // No-op outside macOS; the UI does not surface the install button
        // on other platforms.
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
    UpdateAvailable {
        tag: String,
        html_url: String,
        installable: Option<InstallableRelease>,
    },
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Picks the tarball + matching `.minisig` from a release's asset list.
/// Returns `None` (and the UI falls back to the browser link) if either
/// piece is missing — older releases predate the auto-update pipeline,
/// and a release with the tarball but no signature is treated identically:
/// without the signature we cannot verify integrity, so we refuse to
/// auto-install.
fn select_installable(tag: &str, assets: &[ReleaseAsset]) -> Option<InstallableRelease> {
    if !GithubUpdateState::install_supported() {
        return None;
    }
    let tarball = assets
        .iter()
        .find(|a| a.name.ends_with(".tar.gz") && !a.name.ends_with(".tar.gz.minisig"))?;
    let sig = assets
        .iter()
        .find(|a| a.name == format!("{}.minisig", tarball.name))?;
    Some(InstallableRelease {
        tag: tag.to_string(),
        asset_url: tarball.browser_download_url.clone(),
        sig_url: sig.browser_download_url.clone(),
    })
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

    let installable = select_installable(&tag, &release.assets);

    let current_version = ChannelState::app_version().or(option_env!("GIT_RELEASE_TAG"));
    match current_version {
        None => Ok(CheckResult::UpdateAvailable {
            tag,
            html_url,
            installable,
        }),
        Some(current_tag) => {
            let current = ParsedGithubVersion::parse(current_tag)
                .with_context(|| format!("Failed to parse current version tag {current_tag}"))?;
            if latest.cmp_numeric(&current) == Ordering::Greater {
                Ok(CheckResult::UpdateAvailable {
                    tag,
                    html_url,
                    installable,
                })
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
    // gh CLI stores its config at `~/.config/gh/hosts.yml` on every platform
    // (XDG-style), not at `dirs::config_dir()/gh/...` — which on macOS resolves
    // to `~/Library/Application Support/gh/`, where gh never writes. Try the
    // XDG path first (covers the gh-on-macOS case), then $XDG_CONFIG_HOME if
    // set, then fall back to `dirs::config_dir()` for any platform where gh
    // does honor it.
    let mut candidates: Vec<std::path::PathBuf> = Vec::with_capacity(3);
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".config").join("gh").join("hosts.yml"));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            candidates.push(std::path::PathBuf::from(xdg).join("gh").join("hosts.yml"));
        }
    }
    if let Some(cfg) = dirs::config_dir() {
        candidates.push(cfg.join("gh").join("hosts.yml"));
    }

    for path in candidates {
        if let Some(token) = parse_gh_hosts_yaml(&path) {
            return Some(token);
        }
    }
    None
}

fn parse_gh_hosts_yaml(path: &std::path::Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
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

// =============================================================================
// SHA → tag reverse resolution (lets a retag fix a stale version without a
// rebuild or repackage). Triggered once at app startup; falls back silently to
// whatever ChannelState::app_version() already holds (plist or option_env).
// =============================================================================

#[derive(Deserialize)]
struct GitRef {
    #[serde(rename = "ref")]
    ref_name: String,
    object: GitRefObject,
}

#[derive(Deserialize)]
struct GitRefObject {
    sha: String,
    #[serde(rename = "type")]
    obj_type: String,
}

#[derive(Deserialize)]
struct GitTagObject {
    object: GitTagInner,
}

#[derive(Deserialize)]
struct GitTagInner {
    sha: String,
}

#[derive(Serialize, Deserialize)]
struct VersionCache {
    sha: String,
    tag: String,
    ts: u64,
}

/// Triggers a one-shot SHA→tag resolution against the fork's GitHub Releases.
/// Runs as a spawn on the GithubUpdateState entity (no state mutation; only
/// `ctx.notify()` on success so the version row re-renders). Pure
/// best-effort: any error (no SHA baked in, network down, rate limit, no
/// matching tag) leaves the existing app_version untouched.
/// Best-effort cleanup of the rollback `.previous` bundle left behind by a
/// successful auto-update swap. Reaching this code path means the new
/// version booted far enough to register singletons — i.e. the upgrade
/// looks healthy — so the rollback copy is no longer needed.
pub fn cleanup_previous_install() {
    #[cfg(target_os = "macos")]
    install::cleanup_previous();
}

pub fn trigger_app_version_resolve(ctx: &mut AppContext) {
    let Some(sha) = option_env!("GIT_COMMIT_SHA") else {
        return;
    };
    if sha.is_empty() {
        return;
    }
    let sha = sha.to_string();
    GithubUpdateState::handle(ctx).update(ctx, |_state, ctx| {
        let _ = ctx.spawn(
            async move { resolve_app_version_from_sha(&sha).await },
            |_state, result, ctx| match result {
                Ok(()) => ctx.notify(),
                Err(err) => {
                    log::debug!("github_update: app version resolve skipped: {err:#}");
                }
            },
        );
    });
}

async fn resolve_app_version_from_sha(sha: &str) -> Result<()> {
    if let Some(cache) = read_version_cache() {
        if cache.sha == sha && now_secs().saturating_sub(cache.ts) < VERSION_CACHE_TTL.as_secs() {
            apply_resolved_tag(&cache.tag);
            return Ok(());
        }
    }

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("Failed to construct reqwest client")?;

    let refs: Vec<GitRef> = github_get(&client, REPO_TAGS_API)
        .await
        .context("Failed to list git refs")?;

    for git_ref in &refs {
        let commit_sha = match git_ref.object.obj_type.as_str() {
            "commit" => git_ref.object.sha.clone(),
            "tag" => match dereference_annotated_tag(&client, &git_ref.object.sha).await {
                Ok(s) => s,
                Err(err) => {
                    log::debug!("dereference annotated tag failed: {err:#}");
                    continue;
                }
            },
            _ => continue,
        };
        if commit_sha != sha {
            continue;
        }
        let tag = git_ref
            .ref_name
            .strip_prefix("refs/tags/")
            .unwrap_or(&git_ref.ref_name)
            .to_string();
        if ParsedGithubVersion::parse(&tag).is_err() {
            continue;
        }
        apply_resolved_tag(&tag);
        if let Err(err) = write_version_cache(&VersionCache {
            sha: sha.to_string(),
            tag,
            ts: now_secs(),
        }) {
            log::debug!("write version cache failed: {err:#}");
        }
        return Ok(());
    }

    bail!("no release-shaped tag points at {sha}");
}

async fn github_get<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T> {
    let mut request = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, ACCEPT_HEADER);
    if let Some(token) = resolve_github_token() {
        request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    Ok(request
        .send()
        .await
        .context("send")?
        .error_for_status()
        .context("status")?
        .json()
        .await
        .context("json")?)
}

async fn dereference_annotated_tag(client: &reqwest::Client, tag_sha: &str) -> Result<String> {
    let url = format!("{REPO_GIT_TAGS_API}{tag_sha}");
    let obj: GitTagObject = github_get(client, &url).await?;
    Ok(obj.object.sha)
}

fn apply_resolved_tag(tag: &str) {
    let leaked: &'static str = Box::leak(tag.to_string().into_boxed_str());
    ChannelState::set_app_version(Some(leaked));
    log::info!("github_update: app_version resolved to {leaked} via SHA reverse lookup");
}

fn version_cache_path() -> std::path::PathBuf {
    paths::cache_dir().join(VERSION_CACHE_FILE)
}

fn read_version_cache() -> Option<VersionCache> {
    let bytes = std::fs::read(version_cache_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_version_cache(cache: &VersionCache) -> Result<()> {
    let path = version_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create cache dir")?;
    }
    let bytes = serde_json::to_vec(cache).context("serialize cache")?;
    std::fs::write(&path, bytes).context("write cache")?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
