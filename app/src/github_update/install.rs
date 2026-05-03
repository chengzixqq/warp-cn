//! macOS auto-update installer (no Apple Developer ID required).
//!
//! Pipeline, on the Tokio runtime owned by `ctx.spawn`:
//!   1. Stage to a per-update directory under `paths::cache_dir()/updates/`.
//!   2. Download tarball + `.minisig` via `reqwest`. Files written by this
//!      process do not get the `com.apple.quarantine` xattr — that flag is
//!      only attached by LaunchServices/Finder/AirDrop, never by raw POSIX
//!      writes. This is the entire reason we can self-update without
//!      notarization.
//!   3. Verify the tarball against the build-time-baked
//!      `WARP_UPDATE_PUBKEY` (minisign Ed25519). Refuses to proceed if the
//!      key is missing — see [`super::GithubUpdateState::install_supported`].
//!   4. Extract via `/usr/bin/tar -xzf`. Tar does not propagate quarantine
//!      xattrs through extraction, so the new `.app` is xattr-clean even
//!      if a previous CI step touched the tarball.
//!   5. Defensively `xattr -dr com.apple.quarantine` and re-sign ad-hoc —
//!      bundle integrity sealing on macOS 12+ requires re-signing after
//!      any file-level rewrite.
//!   6. Hand off to a detached `/bin/sh` helper that waits for this PID
//!      to exit, swaps `INSTALL_PATH` ↔ `INSTALL_PATH.previous`, drops in
//!      the new bundle, strips quarantine again as a belt-and-braces
//!      step, and `open`s the new app.
//!   7. `std::process::exit(0)`.
//!
//! Refusal modes (each surfaces as `Err` → `GithubUpdateState::Error`):
//!   * Pubkey not baked → "this build cannot self-update".
//!   * Current binary is not inside a `.app` (e.g. `cargo run`) → refuse to
//!     trash the working tree.
//!   * Signature verification failure → never extract, never write.
//!   * Tar/codesign failure → leaves the prior install intact.
//!
//! No granular progress reporting in this iteration — release tarballs are
//! ~50–150 MB and complete in seconds. Adding a stream-based progress
//! callback later is mechanical (replace `bytes()` with `bytes_stream` and
//! a counter that nudges entity state via `ctx.spawn` ticks).

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use minisign_verify::{PublicKey, Signature};
use warp_core::paths;

use super::InstallableRelease;

const REQUEST_TIMEOUT_SECS: u64 = 300; // tarballs ~ 50-150 MB; allow slow links
const USER_AGENT: &str = "warp-cn-update-install/0.1";

pub(super) async fn run_install(target: InstallableRelease) -> Result<()> {
    let pubkey_str = option_env!("WARP_UPDATE_PUBKEY")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!("this build was produced without WARP_UPDATE_PUBKEY; auto-update disabled")
        })?;
    let pubkey = PublicKey::from_base64(pubkey_str)
        .map_err(|e| anyhow!("invalid baked WARP_UPDATE_PUBKEY: {e}"))?;

    let install_path = current_app_bundle_path()
        .context("could not locate current .app bundle (running outside an .app?)")?;

    let staging = staging_dir(&target.tag)?;
    let tarball = staging.join("warp.tar.gz");
    let sig = staging.join("warp.tar.gz.minisig");

    download_to(&target.asset_url, &tarball)
        .await
        .context("download tarball")?;
    download_to(&target.sig_url, &sig)
        .await
        .context("download signature")?;

    verify_minisign(&pubkey, &tarball, &sig).context("minisign verification")?;

    let extracted = staging.join("ext");
    fs::create_dir_all(&extracted).context("create extract dir")?;
    run_cmd(
        "/usr/bin/tar",
        &[
            "-xzf",
            tarball.to_str().unwrap(),
            "-C",
            extracted.to_str().unwrap(),
        ],
    )
    .context("tar -xzf")?;

    let new_app = find_dot_app(&extracted).context("locate .app inside extracted tarball")?;

    // Defensive: tar should never carry com.apple.quarantine, but if a future
    // CI step pipes through a quarantining tool, this strips it. Errors are
    // ignored: missing xattr is the success path.
    let _ = run_cmd(
        "/usr/bin/xattr",
        &["-dr", "com.apple.quarantine", new_app.to_str().unwrap()],
    );

    // Re-seal ad-hoc signature. Without --deep on macOS 13+ this would warn,
    // but the warning is non-fatal and --deep is the only single-call form
    // that re-signs framework children too.
    run_cmd(
        "/usr/bin/codesign",
        &[
            "--force",
            "--deep",
            "--sign",
            "-",
            new_app.to_str().unwrap(),
        ],
    )
    .context("codesign --sign -")?;

    let helper = write_helper_script(&staging, std::process::id(), &new_app, &install_path)
        .context("write helper script")?;

    Command::new("/bin/sh")
        .arg(&helper)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .context("spawn helper")?;

    // Yield enough time for the helper to start its kill -0 polling loop
    // before our PID dies, but not so long that the user notices a hang.
    std::thread::sleep(std::time::Duration::from_millis(300));
    log::info!("github_update: helper spawned, exiting current process for relaunch");
    std::process::exit(0);
}

fn staging_dir(tag: &str) -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Per-attempt subdir avoids collision if a prior failed attempt left
    // partial files behind. The per-tag root is reused so retries cluster.
    let dir = paths::cache_dir()
        .join("updates")
        .join(sanitize_for_path(tag))
        .join(format!("{now}"));
    fs::create_dir_all(&dir).context("create staging dir")?;
    Ok(dir)
}

fn sanitize_for_path(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn download_to(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;
    let bytes = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("status {url}"))?
        .bytes()
        .await
        .with_context(|| format!("body {url}"))?;
    fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

fn verify_minisign(pubkey: &PublicKey, file: &Path, sig: &Path) -> Result<()> {
    let sig_text = fs::read_to_string(sig).context("read sig file")?;
    let sig = Signature::decode(&sig_text).map_err(|e| anyhow!("decode signature: {e}"))?;
    let bytes = fs::read(file).context("read file for verify")?;
    pubkey
        .verify(&bytes, &sig, false)
        .map_err(|e| anyhow!("signature verification failed: {e}"))?;
    Ok(())
}

/// Walks up from `current_exe` to find the enclosing `.app` directory.
/// Returns `Err` if the binary lives outside any bundle (e.g. `cargo run`),
/// which is the correct behavior — we refuse to overwrite a developer's
/// working tree as a "release".
fn current_app_bundle_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    for ancestor in exe.ancestors() {
        if ancestor
            .extension()
            .map(|e| e.eq_ignore_ascii_case("app"))
            .unwrap_or(false)
        {
            return Ok(ancestor.to_path_buf());
        }
    }
    bail!("current_exe {} is not inside a .app bundle", exe.display())
}

fn find_dot_app(dir: &Path) -> Result<PathBuf> {
    for entry in fs::read_dir(dir).context("read_dir extract dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir()
            && path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("app"))
                .unwrap_or(false)
        {
            return Ok(path);
        }
    }
    bail!("no .app bundle found in {}", dir.display())
}

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("spawn {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Writes the self-replace helper to disk. Crucial properties:
///   * Polls the parent PID with `kill -0` rather than sleeping a fixed
///     duration; works regardless of how long shutdown takes.
///   * Caps the wait at 50 × 0.2s = 10s; if the parent hangs, we don't
///     leave a zombie helper running forever.
///   * Backs up the prior install to `.previous` so a broken upgrade can
///     be rolled back manually; cleared by [`super::cleanup_previous`] on
///     the next successful boot.
///   * `xattr -dr` runs again post-move as belt-and-braces against any
///     OS-level quarantine attribution we missed.
fn write_helper_script(
    dir: &Path,
    parent_pid: u32,
    new_app: &Path,
    install_path: &Path,
) -> Result<PathBuf> {
    let helper = dir.join("apply_update.sh");
    let script = format!(
        r#"#!/bin/sh
set -u
PARENT={parent_pid}
NEW={new_app}
INSTALL={install_path}

# Wait up to 10s for the parent (running app) to exit.
i=0
while kill -0 "$PARENT" 2>/dev/null && [ "$i" -lt 50 ]; do
    sleep 0.2
    i=$((i+1))
done

PREV="${{INSTALL}}.previous"
rm -rf "$PREV" 2>/dev/null || true
if [ -d "$INSTALL" ]; then
    mv "$INSTALL" "$PREV" || exit 1
fi
mv "$NEW" "$INSTALL" || {{
    # Roll back if the swap failed (e.g. cross-device). Best-effort.
    if [ -d "$PREV" ]; then
        mv "$PREV" "$INSTALL" 2>/dev/null || true
    fi
    exit 1
}}

xattr -dr com.apple.quarantine "$INSTALL" 2>/dev/null || true
open "$INSTALL"
"#,
        parent_pid = parent_pid,
        new_app = shell_quote(new_app),
        install_path = shell_quote(install_path),
    );
    let mut f = fs::File::create(&helper).context("create helper script")?;
    f.write_all(script.as_bytes()).context("write helper")?;
    f.set_permissions(fs::Permissions::from_mode(0o755))
        .context("chmod +x helper")?;
    Ok(helper)
}

fn shell_quote(p: &Path) -> String {
    // POSIX-safe single-quote escaping: ' becomes '\''.
    let s = p.to_string_lossy();
    let escaped = s.replace('\'', r"'\''");
    format!("'{escaped}'")
}

/// Best-effort cleanup of the previous-install rollback copy, called from
/// app startup once the new version has booted successfully.
pub(crate) fn cleanup_previous() {
    let Ok(install_path) = current_app_bundle_path() else {
        return;
    };
    let mut previous = install_path.into_os_string();
    previous.push(".previous");
    let previous: PathBuf = previous.into();
    if previous.exists() {
        match fs::remove_dir_all(&previous) {
            Ok(()) => log::info!(
                "github_update: removed rollback copy {}",
                previous.display()
            ),
            Err(e) => log::debug!(
                "github_update: failed to remove rollback copy {}: {e}",
                previous.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_alnum_and_safe_punct() {
        assert_eq!(
            sanitize_for_path("v0.2026.05.02-cn.0"),
            "v0.2026.05.02-cn.0"
        );
        assert_eq!(sanitize_for_path("v 0/../etc"), "v_0_.._etc");
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        let p = Path::new("/tmp/it's a test/x.app");
        assert_eq!(shell_quote(p), r"'/tmp/it'\''s a test/x.app'");
    }
}
