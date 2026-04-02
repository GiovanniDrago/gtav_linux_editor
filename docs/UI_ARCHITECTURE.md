# UI Architecture

## Main Screens

The application currently has three major UI states managed through a stack.

- `setup`
  - first-run/setup wizard
  - also reachable later through the app menu
- `browser`
  - normal package browsing and preview workflow
- `editor`
  - full-page texture editing workflow

The editor is intentionally not an in-app popup.

## Header / Settings Drawer

The header bar contains:

- settings-drawer toggle in the top-left
- back button when in editor or setup flow
- overflow menu for secondary actions

The left settings drawer currently exposes:

- `Run Setup Wizard Again`
- `Check External Tool Updates`
- `Theme`
- GTA V mods folder path + chooser
- backup-before-save toggle
- copy-destination controls

## Browser Screen Layout

The browser screen is a 3-pane layout.

### Left pane: packages

- real filesystem tree rooted at the configured GTA V mods folder
- empty-state prompt when the mods folder is not configured yet
- save icon on supported file rows when there are unsaved changes
- saves apply back to the original selected file instead of writing only to `builds/`

### Middle pane: textures / archive explorer

- standalone `.ydr`, `.yft`, and `.ytd` imports still show the texture list directly
- `.rpf` imports first show an archive explorer rooted at the imported archive
- the archive explorer includes a current-section search bar, path label, and `Up` button
- folders and package files can be entered by clicking the row label
- folders and package files can also be expanded inline with `+/-` without changing the current section
- opening a supported embedded `.ydr`, `.yft`, or `.ytd` switches the pane to the texture list for that file
- unsupported embedded files currently show `Not supported.`
- texture rows show name, dimensions, format, and mip count

### Right pane: preview

- selected asset/title
- selected texture metadata
- preview image
- edit button

## Editor Screen Layout

The editor screen is split in two.

- left: original texture preview
- right: replacement layout editor

### Replacement layout model

The replacement side uses a recursive section tree.

- a section can be a leaf with one assigned image
- or a grouped split containing multiple equal siblings on one axis

Repeated adds on the same section and same axis produce equal-sized siblings rather than uneven nested halves.

### Section controls

Each section currently includes:

- image add/replace button in the top-right corner
- row/column add/remove controls using compact icons

The controls are always visible now because hover-only controls were not reliable enough for nested layouts.

## Setup Wizard Layout

The setup wizard is a linear sequence of steps.

- Welcome
- External Tools
- System Dependencies
- Build Helper
- Ready

Each step shows:

- a title
- explanatory text
- a list of status/info rows
- back/next/action controls

The wizard blocks normal app use until setup is complete.

## Theme Handling

Theme switching uses libadwaita `StyleManager`.

Supported options:

- System
- Light
- Dark

Theme preference is persisted in the app config.

## Key UI Code Anchors

Useful places in `src/main.rs`:

- `ToolPaths` - app-local path layout and external tool resolution
- `AppConfig` - persisted app state
- `SetupStep` / `SetupStatus` - setup wizard state
- `AppWidgets` - major widget handles
- `App` - application state and orchestration
- `build_widgets` - UI construction
- `connect_signals` - signal wiring
- `handle_job_results` - async result handling
- `refresh_textures_list` - archive explorer and texture-list mode switching
- `append_archive_rows` - middle-pane archive tree/list rendering
- `build_section_widget` - recursive editor rendering
- `add_section_controls` - per-section controls
