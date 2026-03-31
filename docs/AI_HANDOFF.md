# AI Handoff

## Read This First

For fast orientation, read in this order:

1. `README.md`
2. `docs/PROJECT_OVERVIEW.md`
3. `docs/UI_ARCHITECTURE.md`
4. `docs/V1_DEVELOPMENT_NOTES.md`
5. `src/main.rs`

## Most Important Concepts

### 1. This app is a coordinator, not a native GTA parser

The Rust app does not directly parse/write GTA assets.

It relies on:

- bundled `external/CwAssetTool/`
- downloaded `external/CodeWalker/`
- system `magick`

### 2. Setup state matters

The setup wizard is not optional fluff.

The app is intentionally blocked until:

- bundled helper source exists
- CodeWalker source exists
- `git` is available
- `dotnet` is available
- `magick` is available
- helper binary has been built

### 3. Config is persisted

Look at `AppConfig` for:

- setup completion
- theme
- copy destination
- last-used asset/image/folder paths

### 4. Editor layout is custom

The texture editor uses a custom recursive section tree.

Important consequences:

- repeated same-axis additions should stay equal-sized
- removal acts relative to the current section and parent axis
- UI changes here can easily break composition logic if rendering and output logic drift apart

## Likely Hotspots For Future Work

### Setup / bootstrap

Search for:

- `SetupStep`
- `SetupStatus`
- `handle_setup_action`
- `download_codewalker`
- `check_codewalker_updates`

### Main app state

Search for:

- `struct App`
- `refresh_all`
- `refresh_header`
- `handle_job_results`

### Editor behavior

Search for:

- `SectionNode`
- `EditorState`
- `build_section_widget`
- `build_leaf_section_widget`
- `add_section_controls`

### Asset pipeline

Search for:

- `import_asset_draft`
- `list_rpf_tree`
- `export_archive_entry_draft`
- `parse_textures_from_xml`
- `apply_texture_job`
- `save_asset_build_job`
- `save_rpf_build_job`

### Archive browsing / state

Search for:

- `RpfTreeNode`
- `ImportedArchiveEntry`
- `refresh_textures_list`
- `append_archive_rows`
- `select_archive_parent`
- `set_archive_search_query`

### Helper-side archive commands

Search for:

- `list-rpf`
- `export-rpf-entry`
- `build-rpf`
- `CreateRpfManager`
- `GetRpfEntryExportData`

## Things That Were Already Decided

- `CwAssetTool` is bundled and versioned.
- `CodeWalker` is external and downloaded.
- The app is GNOME-style and uses libadwaita.
- The editor is a full page, not a popup.
- The standalone repo root is this folder.

## Things To Double-Check Before Major Changes

- Does the change break standalone repo assumptions?
- Does the change accidentally require committing downloaded external code?
- Does the editor still preserve equal-sized siblings on repeated same-axis adds?
- Does the change move expensive work back onto the UI thread?

## Suggested Future Improvements

- split `src/main.rs` into modules
- add tests for setup/config/update logic
- add tests for image composition geometry
- improve update UX with explicit confirmation dialogs
- refine editor visuals in deep nested layouts
