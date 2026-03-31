# Project Overview

## Purpose

`gtav_texture_importer` is a GTK4/libadwaita desktop app for editing embedded textures inside GTA V `.ydr` and `.yft` assets on Linux.

The app is focused on a safe workflow:

- import assets without changing the originals
- inspect textures contained inside the asset
- replace one texture with one or more user images
- rebuild the final asset into `builds/`
- optionally copy built outputs elsewhere while preserving the fake folder structure created in the UI

## Core Dependency Model

The project intentionally separates bundled custom code from downloadable external source.

- Bundled and versioned with the repo:
  - `external/CwAssetTool/`
- Downloaded by the setup wizard, not intended for commits:
  - `external/CodeWalker/`
- Required system tools:
  - `git`
  - `dotnet`
  - `magick` from ImageMagick

`CwAssetTool` is a small custom helper that wraps CodeWalker APIs for:

- asset inspection
- XML export
- XML import
- DDS inspection

The Rust app does not directly implement GTA V asset parsing/writing. It orchestrates the helper and presents the UI.

## Main Runtime Folders

- `src/` - main application code
- `assets/` - UI icons used by the editor
- `external/CwAssetTool/` - bundled helper source
- `external/CodeWalker/` - downloaded upstream source
- `.workspace/` - app config and temporary working files
- `builds/` - rebuilt assets produced by the app

## Main User Flow

1. First startup opens the setup wizard.
2. The wizard verifies bundled helper source, downloadable external source, and system dependencies.
3. If `CodeWalker` is missing, the app can download it from:
   - `https://github.com/dexyfex/CodeWalker`
4. The app builds `CwAssetTool` locally.
5. After setup, the user imports `.ydr` or `.yft` files.
6. The user selects a texture, edits it, applies changes, and saves rebuilt outputs into `builds/`.

## Important Design Assumptions

- The app is Linux-first.
- The UI is GNOME-style and uses libadwaita theming.
- The app should remain usable as a standalone repo rooted at this folder.
- Downloadable third-party source should stay out of commits.
