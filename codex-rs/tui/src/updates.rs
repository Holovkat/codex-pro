use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_agentic_core::updates::UpdateConfig;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::path::Path;
use std::path::PathBuf;

use codex_core::config::Config;
use codex_core::default_client::create_client;

use crate::version::CODEX_CLI_VERSION;

#[derive(Debug, Clone)]
pub struct UpgradeInfo {
    pub latest_version: String,
    pub release_url: Option<String>,
    pub upgrade_cmd: Option<String>,
}

pub fn get_upgrade_version(config: &Config, update_config: &UpdateConfig) -> Option<UpgradeInfo> {
    if update_config.disable_check {
        return None;
    }
    let version_file = version_filepath(config);
    let info = read_version_info(&version_file).ok();

    if match &info {
        None => true,
        Some(info) => info.last_checked_at < Utc::now() - Duration::hours(20),
    } {
        // Refresh the cached latest version in the background so TUI startup
        // isn’t blocked by a network call. The UI reads the previously cached
        // value (if any) for this run; the next run shows the banner if needed.
        let update_config = update_config.clone();
        tokio::spawn(async move {
            let _ = check_for_update(&version_file, update_config)
                .await
                .inspect_err(|e| tracing::error!("Failed to update version: {e}"));
        });
    }

    info.and_then(|info| {
        if is_newer(&info.latest_version, CODEX_CLI_VERSION).unwrap_or(false) {
            Some(UpgradeInfo {
                latest_version: info.latest_version,
                release_url: release_url(update_config),
                upgrade_cmd: update_config.upgrade_cmd.clone(),
            })
        } else {
            None
        }
    })
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct VersionInfo {
    latest_version: String,
    // ISO-8601 timestamp (RFC3339)
    last_checked_at: DateTime<Utc>,
}

#[derive(Deserialize, Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
}

const VERSION_FILENAME: &str = "version.json";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";

fn version_filepath(config: &Config) -> PathBuf {
    config.codex_home.join(VERSION_FILENAME)
}

fn resolve_latest_url(update_config: &UpdateConfig) -> Option<String> {
    if let Some(url) = &update_config.latest_url
        && !url.trim().is_empty()
    {
        return Some(url.clone());
    }
    if let Some(repo) = &update_config.repo
        && !repo.trim().is_empty()
    {
        return Some(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        ));
    }
    Some(LATEST_RELEASE_URL.to_string())
}

fn release_url(update_config: &UpdateConfig) -> Option<String> {
    if let Some(repo) = &update_config.repo
        && !repo.trim().is_empty()
    {
        return Some(format!("https://github.com/{repo}/releases/latest"));
    }
    update_config
        .latest_url
        .as_ref()
        .and_then(|url| {
            url.strip_prefix("https://api.github.com/repos/")
                .and_then(|rest| rest.strip_suffix("/releases/latest"))
                .map(|slug| format!("https://github.com/{slug}/releases/latest"))
        })
        .or_else(|| Some("https://github.com/openai/codex/releases/latest".to_string()))
}

fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

async fn check_for_update(version_file: &Path, update_config: UpdateConfig) -> anyhow::Result<()> {
    let Some(latest_url) = resolve_latest_url(&update_config) else {
        return Ok(());
    };
    let ReleaseInfo {
        tag_name: latest_tag_name,
    } = create_client()
        .get(&latest_url)
        .send()
        .await?
        .error_for_status()?
        .json::<ReleaseInfo>()
        .await?;

    let info = VersionInfo {
        latest_version: normalize_tag(&latest_tag_name),
        last_checked_at: Utc::now(),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

fn is_newer(latest: &str, current: &str) -> Option<bool> {
    let latest = parse_version(latest)?;
    let current = parse_version(current)?;
    if latest.unsupported_suffix || current.unsupported_suffix {
        return None;
    }
    match (latest.major, latest.minor, latest.patch).cmp(&(
        current.major,
        current.minor,
        current.patch,
    )) {
        Ordering::Greater => Some(true),
        Ordering::Less => Some(false),
        Ordering::Equal => {
            let latest_apc = latest.apc.unwrap_or(0);
            let current_apc = current.apc.unwrap_or(0);
            if latest_apc > current_apc {
                Some(true)
            } else {
                Some(false)
            }
        }
    }
}

#[derive(Debug)]
struct ParsedVersion {
    major: u64,
    minor: u64,
    patch: u64,
    apc: Option<u64>,
    unsupported_suffix: bool,
}

fn parse_version(v: &str) -> Option<ParsedVersion> {
    let trimmed = v.trim();
    let mut parts = trimmed.splitn(2, '-');
    let base = parts.next()?;
    let suffix = parts.next();

    let mut iter = base.split('.');
    let major = iter.next()?.parse::<u64>().ok()?;
    let minor = iter.next()?.parse::<u64>().ok()?;
    let patch = iter.next()?.parse::<u64>().ok()?;
    if iter.next().is_some() {
        return None;
    }

    let mut parsed = ParsedVersion {
        major,
        minor,
        patch,
        apc: None,
        unsupported_suffix: false,
    };

    if let Some(suffix) = suffix {
        if let Some(apc_suffix) = suffix.strip_prefix("apc.") {
            if let Ok(value) = apc_suffix.parse::<u64>() {
                parsed.apc = Some(value);
            } else {
                return None;
            }
        } else {
            parsed.unsupported_suffix = true;
        }
    }

    Some(parsed)
}

fn normalize_tag(tag: &str) -> String {
    let trimmed = tag.trim();
    if let Some(without_rust_v) = trimmed.strip_prefix("rust-v") {
        return without_rust_v.to_string();
    }
    if let Some(without_v) = trimmed.strip_prefix('v') {
        return without_v.to_string();
    }
    trimmed.to_string()
}

/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Update via `npm install -g @openai/codex@latest`.
    NpmGlobalLatest,
    /// Update via `bun install -g @openai/codex@latest`.
    BunGlobalLatest,
    /// Update via `brew upgrade codex`.
    BrewUpgrade,
}

#[cfg(any(not(debug_assertions), test))]
pub(crate) fn get_update_action() -> Option<UpdateAction> {
    let exe = std::env::current_exe().unwrap_or_default();
    let managed_by_npm = std::env::var_os("CODEX_MANAGED_BY_NPM").is_some();
    let managed_by_bun = std::env::var_os("CODEX_MANAGED_BY_BUN").is_some();
    if managed_by_npm {
        Some(UpdateAction::NpmGlobalLatest)
    } else if managed_by_bun {
        Some(UpdateAction::BunGlobalLatest)
    } else if cfg!(target_os = "macos")
        && (exe.starts_with("/opt/homebrew") || exe.starts_with("/usr/local"))
    {
        Some(UpdateAction::BrewUpgrade)
    } else {
        None
    }
}

impl UpdateAction {
    /// Returns the list of command-line arguments for invoking the update.
    pub fn command_args(self) -> (&'static str, &'static [&'static str]) {
        match self {
            UpdateAction::NpmGlobalLatest => ("npm", &["install", "-g", "@openai/codex@latest"]),
            UpdateAction::BunGlobalLatest => ("bun", &["install", "-g", "@openai/codex@latest"]),
            UpdateAction::BrewUpgrade => ("brew", &["upgrade", "--cask", "codex"]),
        }
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        let args_str = args.join(" ");
        format!("{command} {args_str}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), None);
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), None);
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(
            parse_version(" 1.2.3 \n").map(|p| (p.major, p.minor, p.patch, p.apc)),
            Some((1, 2, 3, None))
        );
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }

    #[test]
    fn apc_suffix_is_used_as_tiebreaker() {
        assert_eq!(is_newer("1.2.3-apc.2", "1.2.3-apc.1"), Some(true));
        assert_eq!(is_newer("1.2.3-apc.1", "1.2.3-apc.2"), Some(false));
        assert_eq!(is_newer("1.2.3-apc.1", "1.2.3"), Some(true));
        assert_eq!(is_newer("1.2.3", "1.2.3-apc.4"), Some(false));
    }

    #[test]
    fn test_get_update_action() {
        let prev = std::env::var_os("CODEX_MANAGED_BY_NPM");
        let prev_bun = std::env::var_os("CODEX_MANAGED_BY_BUN");

        unsafe { std::env::remove_var("CODEX_MANAGED_BY_NPM") };
        unsafe { std::env::remove_var("CODEX_MANAGED_BY_BUN") };
        assert_eq!(get_update_action(), None);

        unsafe { std::env::set_var("CODEX_MANAGED_BY_NPM", "1") };
        assert_eq!(get_update_action(), Some(UpdateAction::NpmGlobalLatest));

        if let Some(v) = prev {
            unsafe { std::env::set_var("CODEX_MANAGED_BY_NPM", v) };
        } else {
            unsafe { std::env::remove_var("CODEX_MANAGED_BY_NPM") };
        }

        if let Some(v) = prev_bun {
            unsafe { std::env::set_var("CODEX_MANAGED_BY_BUN", v) };
        } else {
            unsafe { std::env::remove_var("CODEX_MANAGED_BY_BUN") };
        }
    }
}
