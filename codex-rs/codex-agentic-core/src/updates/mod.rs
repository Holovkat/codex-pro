use crate::settings::Settings;

#[derive(Debug, Default, Clone)]
pub struct UpdateConfig {
    pub repo: Option<String>,
    pub latest_url: Option<String>,
    pub upgrade_cmd: Option<String>,
    pub disable_check: bool,
}

pub fn default_config() -> UpdateConfig {
    UpdateConfig::default()
}

pub fn from_settings(settings: &Settings) -> UpdateConfig {
    let mut config = UpdateConfig::default();
    if let Some(updates) = &settings.updates {
        config.repo = updates.repo.clone();
        config.latest_url = updates.latest_url.clone();
        config.upgrade_cmd = updates.upgrade_cmd.clone();
        config.disable_check = updates.disable_check.unwrap_or(false);
    }
    config
}
