# GTAV texture importer

`gtav_texture_importer` is a GTK4/libadwaita desktop editor for replacing textures inside GTA V `.ydr` and `.yft` assets on Arch Linux.

The app bundles its own `CwAssetTool` source in `external/CwAssetTool/`, downloads `CodeWalker` into `external/CodeWalker/` when needed, and uses the system `magick` command for DDS preview/conversion.

## What it does

- Imports one or more `.ydr` and `.yft` packages without touching the originals.
- Lets you create fake folders in the package tree and move imported files between them.
- Uses a GNOME-style application window instead of the previous custom window chrome.
- Starts with a setup wizard the first time the app runs, and can rerun it later from the app menu.
- Shows package files, textures, and previews in three horizontally resizable panes.
- Switches the main frame into a full editor page instead of opening an in-app popup editor.
- Uses a recursive split editor where `Add Row` and `Add Column` apply only to the selected section, and repeated adds on the same axis create equal-sized siblings.
- Lets you remove rows and columns from the currently targeted section without rebuilding the whole layout.
- Keeps the last used folder for asset import, image import, and copy-destination picking while the app is running.
- Offers an app menu with external update checks and theme selection.
- Saves rebuilt assets into `builds/`.
- Copies all built assets into a user-defined destination while preserving the fake folder structure.

## Docs

- `docs/PROJECT_OVERVIEW.md` - high-level project purpose, dependency model, and runtime flow
- `docs/V1_DEVELOPMENT_NOTES.md` - main technical difficulties and v1 decisions
- `docs/UI_ARCHITECTURE.md` - screen layout and UI behavior summary
- `docs/AI_HANDOFF.md` - quick orientation notes for the next AI/code agent

## Runtime requirements

- Rust toolchain
- GTK4 development files
- libadwaita development files
- git
- .NET 10 SDK
- ImageMagick with the `magick` command available in `PATH`

The setup wizard checks for the required tools and can download CodeWalker from `https://github.com/dexyfex/CodeWalker` into the app-local `external/` folder before building the bundled helper.

## Run

```bash
cargo run
```

## Output folders

- Working files: `.workspace/`
- Rebuilt assets: `builds/`
- Bundled helper source: `external/CwAssetTool/`
- Downloaded external source: `external/CodeWalker/`

The original imported asset files are left unchanged.
