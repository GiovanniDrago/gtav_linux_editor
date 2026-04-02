# Project Overview

## Purpose

`gtav_texture_importer` is a GTK4/libadwaita desktop app for editing embedded textures inside GTA V `.ydr`, `.yft`, `.ytd`, and `.rpf` assets on Linux.

The app is focused on a safe workflow:

- import assets without changing the originals
- inspect textures contained inside the asset or a supported file inside an archive
- replace one texture with one or more user images
- apply changes back into the original selected file
- rebuild the full original `.rpf` archive when edited files came from inside an archive
- optionally create a `.bak` backup before replacing the original file
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
- `.rpf` tree listing
- embedded archive entry export
- rebuilt archive patching/build output

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
5. After setup, the user configures the GTA V mods folder path.
6. The left pane browses the real mods-folder contents and the user opens a supported `.ydr`, `.yft`, `.ytd`, or `.rpf` file from there.
7. If the selection is an archive, the user browses folders/packages in the middle pane, searches within the current section, and opens a supported embedded file.
8. The user selects a texture, edits it, applies changes, and saves from the file row in the left pane.
9. For archive edits, the saved output is a rebuilt copy of the original `.rpf` that replaces the original file after the write succeeds.

## Important Design Assumptions

- The app is Linux-first.
- The UI is GNOME-style and uses libadwaita theming.
- The app should remain usable as a standalone repo rooted at this folder.
- Downloadable third-party source should stay out of commits.
