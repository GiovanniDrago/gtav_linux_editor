# V1 Development Notes

## Biggest Difficulties In V1

## 1. External Tool Strategy

The biggest non-UI decision was how to handle GTA asset read/write support.

- `CwAssetTool` is custom glue code and should stay versioned with the app.
- `CodeWalker` is upstream external code and should be downloaded by the app instead of committed.
- The project needed a first-run bootstrap flow before the main UI could be trusted.

This led to the current setup wizard and app-local `external/` layout.

## 2. Avoiding UI Freezes

Earlier iterations could appear unresponsive because expensive tasks happened too close to the UI thread.

The risky operations are:

- preview generation
- asset export/import
- helper builds
- external downloads
- DDS conversion

V1 moved these operations behind background jobs and a polling/result-handling path so the window stays responsive.

## 3. Moving From Prototype UI To GNOME UI

The first prototype used a different GUI approach. V1 moved to GTK4/libadwaita to get:

- GNOME-style client-side decorations
- a more native Linux desktop feel
- cleaner page navigation for browser vs editor vs setup

That migration changed a large part of the UI structure and event wiring.

## 4. Editor Layout Model

The editor started as a simpler split model but quickly hit limitations.

Problems discovered during V1:

- binary-only splits created awkward nested unequal regions
- always-visible controls could overlap badly
- hover-only controls were not reliable enough for deep nested sections
- nested sections could visually collapse if controls consumed too much layout space

The current model uses grouped same-axis siblings so repeated adds on the same area stay equal-sized.

## 5. Standalone Repo Goal

The app originally lived inside a larger workspace. V1 had to be cleaned up so this folder could stand alone as its own repo.

Main cleanup points:

- move externals under the app folder
- remove runtime dependency on a parent repo layout
- update paths in docs and runtime logic
- keep downloadable externals in `.gitignore`

## Known Tradeoffs / Rough Edges

- The whole app still lives in a single `src/main.rs`, so navigation is simple but long.
- The setup/update system is practical, not deeply abstracted.
- Theme selection uses libadwaita style management, which is the right default for GNOME, but custom theme extensibility is minimal.
- The editor layout system is functional but remains a likely hotspot for future visual tuning.
- `.rpf` support currently focuses on browsing archives and editing supported embedded texture-bearing assets; unsupported binaries and more complex 3D-only content still fall back to `Not supported.`
- Archive navigation, search, and inline expansion now add more stateful behavior to the middle pane, which increases complexity inside `src/main.rs`.

## Good Next Refactors

- split `src/main.rs` into modules
- separate setup/update logic from UI code
- separate editor layout/rendering logic from app shell code
- add tests around config/setup state and composition logic
