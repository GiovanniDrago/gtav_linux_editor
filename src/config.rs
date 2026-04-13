use std::fs;
use std::path::{Path, PathBuf};

use adw::ColorScheme;
use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ToolPaths;

pub const CURRENT_SETUP_REVISION: u32 = 2;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn color_scheme(self) -> ColorScheme {
        match self {
            Self::System => ColorScheme::Default,
            Self::Light => ColorScheme::ForceLight,
            Self::Dark => ColorScheme::ForceDark,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub setup_complete: bool,
    pub setup_revision: u32,
    pub copy_destination: String,
    pub theme: ThemePreference,
    pub game_root_path: Option<PathBuf>,
    pub addons_enabled: bool,
    pub script_mods_enabled: bool,
    pub play_settings_expanded: bool,
    pub backup_before_save: bool,
    pub last_asset_dir: Option<PathBuf>,
    pub last_image_dir: Option<PathBuf>,
    pub last_copy_dir: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            setup_complete: false,
            setup_revision: 0,
            copy_destination: String::new(),
            theme: ThemePreference::System,
            game_root_path: None,
            addons_enabled: true,
            script_mods_enabled: false,
            play_settings_expanded: false,
            backup_before_save: true,
            last_asset_dir: None,
            last_image_dir: None,
            last_copy_dir: None,
        }
    }
}

#[derive(Default, Deserialize)]
struct AppConfigFile {
    setup_complete: bool,
    setup_revision: Option<u32>,
    copy_destination: String,
    theme: Option<ThemePreference>,
    game_root_path: Option<PathBuf>,
    mod_folder_path: Option<PathBuf>,
    addons_enabled: Option<bool>,
    script_mods_enabled: Option<bool>,
    play_settings_expanded: Option<bool>,
    backup_before_save: Option<bool>,
    last_asset_dir: Option<PathBuf>,
    last_image_dir: Option<PathBuf>,
    last_copy_dir: Option<PathBuf>,
}

impl From<AppConfigFile> for AppConfig {
    fn from(value: AppConfigFile) -> Self {
        let inferred_game_root = value.game_root_path.or_else(|| {
            value.mod_folder_path.as_ref().and_then(|mod_folder_path| {
                mod_folder_path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("mods"))
                    .then(|| mod_folder_path.parent().map(Path::to_path_buf))
                    .flatten()
            })
        });

        Self {
            setup_complete: value.setup_complete,
            setup_revision: value.setup_revision.unwrap_or_default(),
            copy_destination: value.copy_destination,
            theme: value.theme.unwrap_or(ThemePreference::System),
            game_root_path: inferred_game_root,
            addons_enabled: value.addons_enabled.unwrap_or(true),
            script_mods_enabled: value.script_mods_enabled.unwrap_or(false),
            play_settings_expanded: value.play_settings_expanded.unwrap_or(false),
            backup_before_save: value.backup_before_save.unwrap_or(true),
            last_asset_dir: value.last_asset_dir,
            last_image_dir: value.last_image_dir,
            last_copy_dir: value.last_copy_dir,
        }
    }
}

impl AppConfig {
    fn path(tool_paths: &ToolPaths) -> PathBuf {
        tool_paths.workspace_dir.join("config.json")
    }

    pub fn load(tool_paths: &ToolPaths) -> Self {
        let path = Self::path(tool_paths);
        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str::<AppConfigFile>(&content).ok())
            .map(Into::into)
            .unwrap_or_default()
    }

    pub fn save(&self, tool_paths: &ToolPaths) -> Result<()> {
        fs::create_dir_all(&tool_paths.workspace_dir)?;
        fs::write(Self::path(tool_paths), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
