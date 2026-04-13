use crate::launcher;
use crate::ToolPaths;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    Welcome,
    ExternalTools,
    SystemDependencies,
    BuildHelper,
    GameFolder,
    Ready,
}

impl SetupStep {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Welcome => Some(Self::ExternalTools),
            Self::ExternalTools => Some(Self::SystemDependencies),
            Self::SystemDependencies => Some(Self::BuildHelper),
            Self::BuildHelper => Some(Self::GameFolder),
            Self::GameFolder => Some(Self::Ready),
            Self::Ready => None,
        }
    }

    pub fn previous(self) -> Option<Self> {
        match self {
            Self::Welcome => None,
            Self::ExternalTools => Some(Self::Welcome),
            Self::SystemDependencies => Some(Self::ExternalTools),
            Self::BuildHelper => Some(Self::SystemDependencies),
            Self::GameFolder => Some(Self::BuildHelper),
            Self::Ready => Some(Self::GameFolder),
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::ExternalTools => "External Tools",
            Self::SystemDependencies => "System Dependencies",
            Self::BuildHelper => "Build Helper",
            Self::GameFolder => "Game Folder",
            Self::Ready => "Ready",
        }
    }
}

pub struct SetupStatus {
    pub cwassettool_source: bool,
    pub codewalker_source: bool,
    pub cwassettool_binary: bool,
    pub git_available: bool,
    pub dotnet_available: bool,
    pub magick_available: bool,
    pub wine_available: bool,
    pub wineboot_available: bool,
    pub wineserver_available: bool,
    pub winetricks_available: bool,
}

impl SetupStatus {
    pub fn detect(tool_paths: &ToolPaths) -> Self {
        let launcher_dependencies = launcher::LauncherDependencyStatus::detect();
        Self {
            cwassettool_source: tool_paths.cwassettool_present(),
            codewalker_source: tool_paths.codewalker_present(),
            cwassettool_binary: tool_paths.cwassettool_bin.is_file(),
            git_available: tool_paths.ensure_git().is_ok(),
            dotnet_available: tool_paths.ensure_dotnet().is_ok(),
            magick_available: tool_paths.ensure_magick().is_ok(),
            wine_available: launcher_dependencies.wine_available,
            wineboot_available: launcher_dependencies.wineboot_available,
            wineserver_available: launcher_dependencies.wineserver_available,
            winetricks_available: launcher_dependencies.winetricks_available,
        }
    }

    pub fn setup_ready(&self) -> bool {
        self.cwassettool_source
            && self.codewalker_source
            && self.cwassettool_binary
            && self.git_available
            && self.dotnet_available
            && self.magick_available
            && self.wine_available
            && self.wineboot_available
            && self.wineserver_available
            && self.winetricks_available
    }
}
