use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const REQUIRED_SCRIPT_HOOK_FILES: &[&str] = &[
    "dinput8.dll",
    "ScriptHookV.dll",
    "ScriptHookVDotNet.asi",
    "ScriptHookVDotNet2.dll",
    "ScriptHookVDotNet3.dll",
    "mscoree.dll",
];

pub fn set_directory_enabled(visible: &Path, hidden: &Path, enabled: bool) -> Result<()> {
    match enabled {
        true => {
            if hidden.exists() && !visible.exists() {
                fs::rename(hidden, visible).with_context(|| {
                    format!(
                        "Failed to unhide {} from {}",
                        visible.display(),
                        hidden.display()
                    )
                })?;
            }
            if !visible.exists() {
                fs::create_dir_all(visible)
                    .with_context(|| format!("Failed to create {}", visible.display()))?;
            }
        }
        false => {
            if visible.exists() && !hidden.exists() {
                fs::rename(visible, hidden).with_context(|| {
                    format!(
                        "Failed to hide {} as {}",
                        visible.display(),
                        hidden.display()
                    )
                })?;
            }
        }
    }

    Ok(())
}

pub fn missing_script_hook_files(game_root: &Path) -> Vec<String> {
    REQUIRED_SCRIPT_HOOK_FILES
        .iter()
        .filter_map(|name| (!game_root.join(name).is_file()).then_some((*name).to_owned()))
        .collect()
}

pub fn update_scripthook_ini(path: &Path, autoload_scripts: bool) -> Result<()> {
    let desired_line = format!(
        "AutoLoadScripts={}",
        if autoload_scripts { "true" } else { "false" }
    );

    let mut lines = if path.exists() {
        fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?
            .lines()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let mut updated = false;
    for line in &mut lines {
        if line
            .split_once('=')
            .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("AutoLoadScripts"))
        {
            *line = desired_line.clone();
            updated = true;
            break;
        }
    }

    if !updated {
        lines.push(desired_line);
    }

    fs::write(path, lines.join("\n") + "\n")
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}
