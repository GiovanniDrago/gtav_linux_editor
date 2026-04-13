use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const BASE_DLL_OVERRIDES: &str =
    "winemenubuilder.exe=d;mshtml=d;dinput8=n,b;mscoree=n,b;d3dx11_43=n,b;scripthookv=n,b;scripthookvdotnet2=n,b;scripthookvdotnet3=n,b";
const RUNTIME_DEPS_MARKER_FILE: &str = "runtime-deps.v1";

#[derive(Clone, Copy)]
pub struct LauncherDependencyStatus {
    pub wine_available: bool,
    pub wineboot_available: bool,
    pub wineserver_available: bool,
    pub winetricks_available: bool,
}

impl LauncherDependencyStatus {
    pub fn detect() -> Self {
        Self {
            wine_available: command_exists(&wine_bin()),
            wineboot_available: command_exists(&wineboot_bin()),
            wineserver_available: command_exists(&wineserver_bin()),
            winetricks_available: command_exists(&winetricks_bin()),
        }
    }
}

pub struct PrepareResult {
    pub prefix: PathBuf,
}

#[derive(Clone, Copy)]
pub enum DependencyStatus {
    AlreadyInstalled,
    InstalledNow,
}

impl DependencyStatus {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::AlreadyInstalled => "already installed",
            Self::InstalledNow => "installed",
        }
    }
}

#[derive(Debug)]
pub enum LauncherError {
    InvalidGameDirectory(PathBuf),
    MissingGameExecutable(PathBuf),
    MissingBinary(&'static str),
    Io {
        context: String,
        source: io::Error,
    },
    Spawn {
        context: String,
        source: io::Error,
    },
    CommandFailed {
        context: String,
        code: Option<i32>,
        stderr: String,
    },
}

impl fmt::Display for LauncherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGameDirectory(path) => {
                write!(f, "Game directory does not exist: {}", path.display())
            }
            Self::MissingGameExecutable(path) => {
                write!(f, "PlayGTAV.exe not found in: {}", path.display())
            }
            Self::MissingBinary(binary) => {
                write!(f, "Required launcher binary is missing: {binary}")
            }
            Self::Io { context, source } => write!(f, "{}: {}", context, source),
            Self::Spawn { context, source } => {
                write!(f, "Failed to launch game process ({}): {}", context, source)
            }
            Self::CommandFailed {
                context,
                code,
                stderr,
            } => {
                if stderr.is_empty() {
                    write!(f, "Command failed ({}), exit code {:?}", context, code)
                } else {
                    write!(
                        f,
                        "Command failed ({}), exit code {:?}: {}",
                        context, code, stderr
                    )
                }
            }
        }
    }
}

impl std::error::Error for LauncherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Spawn { source, .. } => Some(source),
            Self::InvalidGameDirectory(_)
            | Self::MissingGameExecutable(_)
            | Self::MissingBinary(_)
            | Self::CommandFailed { .. } => None,
        }
    }
}

fn command_exists(binary: &str) -> bool {
    Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {} >/dev/null 2>&1", binary))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn launcher_root(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("launcher")
}

fn runtime_dependencies_marker(prefix: &Path) -> PathBuf {
    prefix.join(RUNTIME_DEPS_MARKER_FILE)
}

pub fn wine_prefix(workspace_dir: &Path) -> PathBuf {
    launcher_root(workspace_dir).join("prefix")
}

fn wine_bin() -> String {
    env::var("SYSWINE").unwrap_or_else(|_| "wine".to_owned())
}

fn wineboot_bin() -> String {
    env::var("RUSTY_GTA_WINEBOOT").unwrap_or_else(|_| "wineboot".to_owned())
}

fn wineserver_bin() -> String {
    env::var("RUSTY_GTA_WINESERVER").unwrap_or_else(|_| "wineserver".to_owned())
}

fn winetricks_bin() -> String {
    env::var("RUSTY_GTA_WINETRICKS").unwrap_or_else(|_| "winetricks".to_owned())
}

fn wine_environment(prefix: &Path) -> Vec<(String, String)> {
    vec![
        (
            "WINEPREFIX".to_owned(),
            prefix.as_os_str().to_string_lossy().to_string(),
        ),
        (
            "WINEARCH".to_owned(),
            env::var("WINEARCH").unwrap_or_else(|_| "win64".to_owned()),
        ),
        (
            "WINE_LARGE_ADDRESS_AWARE".to_owned(),
            env::var("WINE_LARGE_ADDRESS_AWARE").unwrap_or_else(|_| "1".to_owned()),
        ),
        (
            "WINEDEBUG".to_owned(),
            env::var("WINEDEBUG").unwrap_or_else(|_| "fixme-all".to_owned()),
        ),
        (
            "DXVK_ENABLE_NVAPI".to_owned(),
            env::var("DXVK_ENABLE_NVAPI").unwrap_or_else(|_| "1".to_owned()),
        ),
        (
            "LC_NUMERIC".to_owned(),
            env::var("LC_NUMERIC").unwrap_or_else(|_| "C".to_owned()),
        ),
        (
            "WINEDLLOVERRIDES".to_owned(),
            env::var("WINEDLLOVERRIDES").unwrap_or_else(|_| BASE_DLL_OVERRIDES.to_owned()),
        ),
    ]
}

fn run_command_wait(
    binary: &str,
    args: &[String],
    envs: &[(String, String)],
    context: &str,
) -> Result<(), LauncherError> {
    let mut command = Command::new(binary);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .args(args)
        .output()
        .map_err(|source| LauncherError::Spawn {
            context: context.to_owned(),
            source,
        })?;

    if output.status.success() {
        return Ok(());
    }

    Err(LauncherError::CommandFailed {
        context: context.to_owned(),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn spawn_command(
    binary: &str,
    args: &[String],
    envs: &[(String, String)],
    context: &str,
    current_dir: &Path,
) -> Result<(), LauncherError> {
    let mut command = Command::new(binary);
    command.current_dir(current_dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|source| LauncherError::Spawn {
            context: context.to_owned(),
            source,
        })
}

fn ensure_binary_available(name: &'static str, binary: &str) -> Result<(), LauncherError> {
    if command_exists(binary) {
        Ok(())
    } else {
        Err(LauncherError::MissingBinary(name))
    }
}

pub fn validate_game_directory(game_dir: &Path) -> Result<(), LauncherError> {
    if !game_dir.is_dir() {
        return Err(LauncherError::InvalidGameDirectory(game_dir.to_path_buf()));
    }

    if !game_dir.join("PlayGTAV.exe").is_file() {
        return Err(LauncherError::MissingGameExecutable(game_dir.to_path_buf()));
    }

    Ok(())
}

pub fn prepare_environment(workspace_dir: &Path) -> Result<PrepareResult, LauncherError> {
    let prefix = wine_prefix(workspace_dir);
    ensure_binary_available("wine", &wine_bin())?;
    ensure_binary_available("wineboot", &wineboot_bin())?;
    ensure_binary_available("wineserver", &wineserver_bin())?;

    let envs = wine_environment(&prefix);
    if !prefix.is_dir() {
        fs::create_dir_all(&prefix).map_err(|source| LauncherError::Io {
            context: format!(
                "Failed to create Wine prefix directory {}",
                prefix.display()
            ),
            source,
        })?;

        run_command_wait(
            &wineboot_bin(),
            &["-i".to_owned()],
            &envs,
            "initialize Wine prefix",
        )?;
        run_command_wait(
            &wineserver_bin(),
            &["-w".to_owned()],
            &envs,
            "wait for wineserver",
        )?;
    }

    Ok(PrepareResult { prefix })
}

fn run_winetricks(
    winetricks: &str,
    envs: &[(String, String)],
    verbs: &[&str],
    context: &str,
) -> Result<(), LauncherError> {
    let mut args = Vec::with_capacity(1 + verbs.len());
    args.push("-q".to_owned());
    args.extend(verbs.iter().map(|verb| (*verb).to_owned()));
    run_command_wait(winetricks, &args, envs, context)
}

pub fn ensure_runtime_dependencies(
    workspace_dir: &Path,
    prepare_result: &PrepareResult,
) -> Result<DependencyStatus, LauncherError> {
    ensure_binary_available("winetricks", &winetricks_bin())?;

    let marker = runtime_dependencies_marker(&prepare_result.prefix);
    if marker.is_file() {
        return Ok(DependencyStatus::AlreadyInstalled);
    }

    let envs = wine_environment(&prepare_result.prefix);
    let winetricks = winetricks_bin();
    let _ = workspace_dir;

    run_winetricks(&winetricks, &envs, &["remove_mono"], "remove wine mono")?;
    run_winetricks(
        &winetricks,
        &envs,
        &["winxp", "dotnet40", "dotnet452"],
        "install dotnet452 runtime",
    )?;
    run_winetricks(
        &winetricks,
        &envs,
        &["winxp", "dotnet40", "dotnet48"],
        "install dotnet48 runtime",
    )?;
    run_winetricks(&winetricks, &envs, &["win10"], "restore win10 mode")?;
    run_winetricks(
        &winetricks,
        &envs,
        &["ucrtbase2019", "vcrun2019"],
        "install VC runtime",
    )?;
    run_winetricks(&winetricks, &envs, &["d3dx11_43"], "install d3dx11_43")?;

    fs::write(&marker, b"runtime dependencies installed").map_err(|source| LauncherError::Io {
        context: format!("Failed to write runtime marker {}", marker.display()),
        source,
    })?;

    Ok(DependencyStatus::InstalledNow)
}

pub fn launch_game_prepared(
    game_dir: &Path,
    prepare_result: &PrepareResult,
) -> Result<(), LauncherError> {
    validate_game_directory(game_dir)?;
    ensure_binary_available("wine", &wine_bin())?;

    let envs = wine_environment(&prepare_result.prefix);
    let wine = wine_bin();
    let context = format!("{} PlayGTAV.exe (cwd: {})", wine, game_dir.display());
    spawn_command(
        &wine,
        &["PlayGTAV.exe".to_owned()],
        &envs,
        &context,
        game_dir,
    )
}
