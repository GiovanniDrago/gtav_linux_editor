use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;
use std::rc::Weak;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use anyhow::{anyhow, bail, Context, Result};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use image::{imageops::FilterType, DynamicImage, ImageBuffer, Rgba, RgbaImage};
use roxmltree::Document;
use serde::{Deserialize, Serialize};

const APP_ID: &str = "lab.coding.gtav_texture_importer";
const ROOT_FOLDER_ID: u64 = 0;

fn main() -> glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();

    application.connect_activate(|app| {
        if let Err(error) = App::launch(app) {
            eprintln!("Failed to launch GTAV texture importer: {error:#}");
        }
    });

    application.run()
}

#[derive(Clone)]
struct ToolPaths {
    app_root: PathBuf,
    workspace_dir: PathBuf,
    builds_dir: PathBuf,
    external_dir: PathBuf,
    codewalker_dir: PathBuf,
    cwassettool_project: PathBuf,
    cwassettool_bin: PathBuf,
}

impl ToolPaths {
    fn discover() -> Result<Self> {
        let app_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_dir = app_root.join(".workspace");
        let builds_dir = app_root.join("builds");
        let external_dir = app_root.join("external");
        let codewalker_dir = external_dir.join("CodeWalker");
        let cwassettool_project = external_dir.join("CwAssetTool/CwAssetTool.csproj");
        let cwassettool_bin = external_dir.join("CwAssetTool/bin/Release/net10.0/CwAssetTool");

        let paths = Self {
            app_root,
            workspace_dir,
            builds_dir,
            external_dir,
            codewalker_dir,
            cwassettool_project,
            cwassettool_bin,
        };

        paths.ensure_directories()?;
        Ok(paths)
    }

    fn ensure_directories(&self) -> Result<()> {
        fs::create_dir_all(self.workspace_dir.join("imports"))?;
        fs::create_dir_all(&self.builds_dir)?;
        fs::create_dir_all(&self.external_dir)?;
        Ok(())
    }

    fn ensure_magick(&self) -> Result<()> {
        let output = run_command("magick", ["-version"])?;
        ensure_success("magick -version", output)
    }

    fn ensure_git(&self) -> Result<()> {
        let output = run_command("git", ["--version"])?;
        ensure_success("git --version", output)
    }

    fn ensure_dotnet(&self) -> Result<()> {
        let output = run_command("dotnet", ["--version"])?;
        ensure_success("dotnet --version", output)
    }

    fn codewalker_present(&self) -> bool {
        self.codewalker_dir
            .join("CodeWalker.Core/CodeWalker.Core.csproj")
            .is_file()
    }

    fn cwassettool_present(&self) -> bool {
        self.cwassettool_project.is_file()
    }

    fn build_cwassettool(&self) -> Result<()> {
        if !self.cwassettool_project.is_file() {
            bail!(
                "CwAssetTool source was not found at {}",
                self.cwassettool_project.display()
            );
        }

        let project_dir = self
            .cwassettool_project
            .parent()
            .context("Invalid CwAssetTool project path")?;

        let output = Command::new("dotnet")
            .arg("build")
            .arg("-c")
            .arg("Release")
            .arg(&self.cwassettool_project)
            .current_dir(project_dir)
            .output()
            .context("Failed to start dotnet build for CwAssetTool")?;

        ensure_success("dotnet build -c Release CwAssetTool", output)?;

        if !self.cwassettool_bin.is_file() {
            bail!(
                "CwAssetTool build completed but the binary was not found at {}",
                self.cwassettool_bin.display()
            );
        }

        Ok(())
    }

    fn codewalker_clone_url(&self) -> &'static str {
        "https://github.com/dexyfex/CodeWalker"
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum ThemePreference {
    System,
    Light,
    Dark,
}

impl ThemePreference {
    fn as_str(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    fn color_scheme(self) -> adw::ColorScheme {
        match self {
            Self::System => adw::ColorScheme::Default,
            Self::Light => adw::ColorScheme::ForceLight,
            Self::Dark => adw::ColorScheme::ForceDark,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct AppConfig {
    setup_complete: bool,
    copy_destination: String,
    theme: ThemePreference,
    mod_folder_path: Option<PathBuf>,
    backup_before_save: bool,
    last_asset_dir: Option<PathBuf>,
    last_image_dir: Option<PathBuf>,
    last_copy_dir: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            setup_complete: false,
            copy_destination: String::new(),
            theme: ThemePreference::System,
            mod_folder_path: None,
            backup_before_save: true,
            last_asset_dir: None,
            last_image_dir: None,
            last_copy_dir: None,
        }
    }
}

impl AppConfig {
    fn path(tool_paths: &ToolPaths) -> PathBuf {
        tool_paths.workspace_dir.join("config.json")
    }

    fn load(tool_paths: &ToolPaths) -> Self {
        let path = Self::path(tool_paths);
        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    fn save(&self, tool_paths: &ToolPaths) -> Result<()> {
        fs::create_dir_all(&tool_paths.workspace_dir)?;
        fs::write(Self::path(tool_paths), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Welcome,
    ExternalTools,
    SystemDependencies,
    BuildHelper,
    Ready,
}

impl SetupStep {
    fn next(self) -> Option<Self> {
        match self {
            Self::Welcome => Some(Self::ExternalTools),
            Self::ExternalTools => Some(Self::SystemDependencies),
            Self::SystemDependencies => Some(Self::BuildHelper),
            Self::BuildHelper => Some(Self::Ready),
            Self::Ready => None,
        }
    }

    fn previous(self) -> Option<Self> {
        match self {
            Self::Welcome => None,
            Self::ExternalTools => Some(Self::Welcome),
            Self::SystemDependencies => Some(Self::ExternalTools),
            Self::BuildHelper => Some(Self::SystemDependencies),
            Self::Ready => Some(Self::BuildHelper),
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::ExternalTools => "External Tools",
            Self::SystemDependencies => "System Dependencies",
            Self::BuildHelper => "Build Helper",
            Self::Ready => "Ready",
        }
    }
}

struct SetupStatus {
    cwassettool_source: bool,
    codewalker_source: bool,
    cwassettool_binary: bool,
    git_available: bool,
    dotnet_available: bool,
    magick_available: bool,
}

impl SetupStatus {
    fn detect(tool_paths: &ToolPaths) -> Self {
        Self {
            cwassettool_source: tool_paths.cwassettool_present(),
            codewalker_source: tool_paths.codewalker_present(),
            cwassettool_binary: tool_paths.cwassettool_bin.is_file(),
            git_available: tool_paths.ensure_git().is_ok(),
            dotnet_available: tool_paths.ensure_dotnet().is_ok(),
            magick_available: tool_paths.ensure_magick().is_ok(),
        }
    }

    fn setup_ready(&self) -> bool {
        self.cwassettool_source
            && self.codewalker_source
            && self.cwassettool_binary
            && self.git_available
            && self.dotnet_available
            && self.magick_available
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Ydr,
    Yft,
    Ytd,
    Rpf,
}

impl AssetKind {
    fn from_path(path: &Path) -> Option<Self> {
        match path
            .extension()?
            .to_string_lossy()
            .to_ascii_lowercase()
            .as_str()
        {
            "ydr" => Some(Self::Ydr),
            "yft" => Some(Self::Yft),
            "ytd" => Some(Self::Ytd),
            "rpf" => Some(Self::Rpf),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ydr => "YDR",
            Self::Yft => "YFT",
            Self::Ytd => "YTD",
            Self::Rpf => "RPF",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum TextureFormat {
    Dxt1,
    Dxt5,
    A8R8G8B8,
    X8R8G8B8,
    Unknown(String),
}

impl TextureFormat {
    fn from_label(label: &str) -> Self {
        match label {
            "D3DFMT_DXT1" => Self::Dxt1,
            "D3DFMT_DXT5" => Self::Dxt5,
            "D3DFMT_A8R8G8B8" => Self::A8R8G8B8,
            "D3DFMT_X8R8G8B8" => Self::X8R8G8B8,
            other => Self::Unknown(other.to_owned()),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Dxt1 => "D3DFMT_DXT1",
            Self::Dxt5 => "D3DFMT_DXT5",
            Self::A8R8G8B8 => "D3DFMT_A8R8G8B8",
            Self::X8R8G8B8 => "D3DFMT_X8R8G8B8",
            Self::Unknown(label) => label.as_str(),
        }
    }

    fn supports_alpha(&self) -> bool {
        matches!(self, Self::Dxt5 | Self::A8R8G8B8)
    }

    fn magick_compression(&self) -> Option<&'static str> {
        match self {
            Self::Dxt1 => Some("dxt1"),
            Self::Dxt5 => Some("dxt5"),
            Self::A8R8G8B8 | Self::X8R8G8B8 => Some("none"),
            Self::Unknown(_) => None,
        }
    }

    fn supported_for_write(&self) -> bool {
        self.magick_compression().is_some()
    }
}

struct FolderNode {
    id: u64,
    parent_id: u64,
    name: String,
}

struct TextureEntry {
    name: String,
    file_name: String,
    width: u32,
    height: u32,
    mips: u32,
    format: TextureFormat,
    usage: String,
    dds_path: PathBuf,
    preview_png_path: PathBuf,
    preview_texture: Option<gdk::Texture>,
    preview_loading: bool,
    modified: bool,
}

struct ImportedAsset {
    id: String,
    source_path: PathBuf,
    kind: AssetKind,
    folder_id: u64,
    xml_path: Option<PathBuf>,
    textures: Vec<TextureEntry>,
    archive_tree: Option<RpfTreeNode>,
    archive_entries: Vec<ImportedArchiveEntry>,
    archive_current_path: Option<String>,
    archive_expanded_paths: HashSet<String>,
    archive_selected_file: Option<String>,
    archive_file_notice: Option<String>,
    archive_file_loading_path: Option<String>,
    archive_search_query: String,
    dirty: bool,
    last_saved_path: Option<PathBuf>,
}

impl ImportedAsset {
    fn title(&self) -> String {
        self.source_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.source_path.display().to_string())
    }

    fn is_archive(&self) -> bool {
        self.kind == AssetKind::Rpf
    }

    fn archive_root_path(&self) -> Option<&str> {
        self.archive_tree.as_ref().map(|tree| tree.path.as_str())
    }

    fn archive_current_display_path(&self) -> Option<String> {
        let path = self
            .archive_selected_file
            .as_deref()
            .or(self.archive_current_path.as_deref())?;
        self.find_archive_node(path)
            .map(|node| node.display_path.clone())
    }

    fn find_archive_node(&self, path: &str) -> Option<&RpfTreeNode> {
        self.archive_tree.as_ref()?.find(path)
    }

    fn find_archive_entry(&self, entry_path: &str) -> Option<&ImportedArchiveEntry> {
        self.archive_entries
            .iter()
            .find(|entry| entry.entry_path == entry_path)
    }

    fn find_archive_entry_mut(&mut self, entry_path: &str) -> Option<&mut ImportedArchiveEntry> {
        self.archive_entries
            .iter_mut()
            .find(|entry| entry.entry_path == entry_path)
    }

    fn sync_archive_dirty(&mut self) {
        if self.is_archive() {
            self.dirty = self.archive_entries.iter().any(|entry| entry.dirty);
        }
    }
}

#[derive(Clone)]
struct TextureEntryDraft {
    name: String,
    file_name: String,
    width: u32,
    height: u32,
    mips: u32,
    format: TextureFormat,
    usage: String,
    dds_path: PathBuf,
    preview_png_path: PathBuf,
}

struct ImportedAssetDraft {
    id: String,
    source_path: PathBuf,
    kind: AssetKind,
    folder_id: u64,
    xml_path: Option<PathBuf>,
    textures: Vec<TextureEntryDraft>,
    archive_tree: Option<RpfTreeNode>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpfTreeNode {
    name: String,
    path: String,
    display_path: String,
    kind: RpfTreeNodeKind,
    supported_asset: bool,
    children: Vec<RpfTreeNode>,
}

impl RpfTreeNode {
    fn find(&self, path: &str) -> Option<&Self> {
        if self.path == path {
            return Some(self);
        }

        self.children.iter().find_map(|child| child.find(path))
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RpfTreeNodeKind {
    Folder,
    Package,
    File,
}

struct ImportedArchiveEntry {
    entry_path: String,
    title: String,
    xml_path: PathBuf,
    textures: Vec<TextureEntry>,
    dirty: bool,
}

#[derive(Clone)]
struct ImportedArchiveEntryDraft {
    entry_path: String,
    title: String,
    xml_path: PathBuf,
    textures: Vec<TextureEntryDraft>,
}

enum ArchiveEntryOpenOutcome {
    Supported(ImportedArchiveEntryDraft),
    Unsupported(String),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RpfBuildManifest {
    changes: Vec<RpfBuildChange>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RpfBuildChange {
    entry_path: String,
    xml_path: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitAxis {
    Horizontal,
    Vertical,
}

struct LeafSection {
    id: u64,
    image_path: Option<PathBuf>,
    preview_texture: Option<gdk::Texture>,
}

struct GroupSection {
    id: u64,
    axis: SplitAxis,
    children: Vec<SectionNode>,
}

enum SectionNode {
    Leaf(LeafSection),
    Group(GroupSection),
}

impl SectionNode {
    fn empty_leaf(id: u64) -> Self {
        Self::Leaf(LeafSection {
            id,
            image_path: None,
            preview_texture: None,
        })
    }

    fn id(&self) -> u64 {
        match self {
            SectionNode::Leaf(leaf) => leaf.id,
            SectionNode::Group(group) => group.id,
        }
    }

    fn add_section(&mut self, section_id: u64, axis: SplitAxis, next_id: &mut u64) -> bool {
        match self {
            SectionNode::Leaf(leaf) if leaf.id == section_id => {
                let existing = std::mem::replace(self, SectionNode::empty_leaf(0));
                let group_id = *next_id;
                *next_id += 1;
                let new_leaf_id = *next_id;
                *next_id += 1;
                *self = SectionNode::Group(GroupSection {
                    id: group_id,
                    axis,
                    children: vec![existing, SectionNode::empty_leaf(new_leaf_id)],
                });
                true
            }
            SectionNode::Group(group) if group.id == section_id => {
                if group.axis == axis {
                    let new_leaf_id = *next_id;
                    *next_id += 1;
                    group.children.push(SectionNode::empty_leaf(new_leaf_id));
                } else {
                    let existing = std::mem::replace(self, SectionNode::empty_leaf(0));
                    let group_id = *next_id;
                    *next_id += 1;
                    let new_leaf_id = *next_id;
                    *next_id += 1;
                    *self = SectionNode::Group(GroupSection {
                        id: group_id,
                        axis,
                        children: vec![existing, SectionNode::empty_leaf(new_leaf_id)],
                    });
                }
                true
            }
            SectionNode::Group(group) => {
                for child in &mut group.children {
                    if child.add_section(section_id, axis, next_id) {
                        return true;
                    }
                }
                false
            }
            SectionNode::Leaf(_) => false,
        }
    }

    fn set_leaf_image(&mut self, leaf_id: u64, image_path: PathBuf, texture: gdk::Texture) -> bool {
        match self {
            SectionNode::Leaf(leaf) if leaf.id == leaf_id => {
                leaf.image_path = Some(image_path);
                leaf.preview_texture = Some(texture);
                true
            }
            SectionNode::Group(group) => {
                for child in &mut group.children {
                    if child.set_leaf_image(leaf_id, image_path.clone(), texture.clone()) {
                        return true;
                    }
                }
                false
            }
            SectionNode::Leaf(_) => false,
        }
    }

    fn remove_section(&mut self, section_id: u64, axis: SplitAxis) -> bool {
        match self {
            SectionNode::Group(group) => {
                if group.axis == axis {
                    if let Some(position) = group
                        .children
                        .iter()
                        .position(|child| child.id() == section_id)
                    {
                        if group.children.len() <= 1 {
                            return false;
                        }
                        group.children.remove(position);
                        if group.children.len() == 1 {
                            let remaining = group.children.remove(0);
                            *self = remaining;
                        }
                        return true;
                    }
                }

                let mut index = 0;
                while let SectionNode::Group(current_group) = self {
                    if index >= current_group.children.len() {
                        break;
                    }

                    if current_group.children[index].remove_section(section_id, axis) {
                        if current_group.children.len() == 1 {
                            let remaining = current_group.children.remove(0);
                            *self = remaining;
                        }
                        return true;
                    }

                    index += 1;
                }

                false
            }
            SectionNode::Leaf(_) => false,
        }
    }

    fn count_missing_images(&self) -> usize {
        match self {
            SectionNode::Leaf(leaf) => usize::from(leaf.image_path.is_none()),
            SectionNode::Group(group) => group
                .children
                .iter()
                .map(SectionNode::count_missing_images)
                .sum(),
        }
    }

    fn collect_composition_cells(
        &self,
        rect: PixelRect,
        keep_alpha: bool,
        cells: &mut Vec<CompositionCell>,
    ) -> Result<()> {
        match self {
            SectionNode::Leaf(leaf) => {
                let image_path = leaf
                    .image_path
                    .clone()
                    .context("Every section needs an image before applying changes")?;
                cells.push(CompositionCell {
                    rect,
                    image_path,
                    keep_alpha,
                });
                Ok(())
            }
            SectionNode::Group(group) => {
                let child_count = group.children.len().max(1) as u32;
                for (index, child) in group.children.iter().enumerate() {
                    let index = index as u32;
                    let child_rect = match group.axis {
                        SplitAxis::Horizontal => {
                            let start = rect.height * index / child_count;
                            let end = rect.height * (index + 1) / child_count;
                            PixelRect {
                                x: rect.x,
                                y: rect.y + start,
                                width: rect.width,
                                height: end.saturating_sub(start),
                            }
                        }
                        SplitAxis::Vertical => {
                            let start = rect.width * index / child_count;
                            let end = rect.width * (index + 1) / child_count;
                            PixelRect {
                                x: rect.x + start,
                                y: rect.y,
                                width: end.saturating_sub(start),
                                height: rect.height,
                            }
                        }
                    };
                    child.collect_composition_cells(child_rect, keep_alpha, cells)?;
                }
                Ok(())
            }
        }
    }
}

struct EditorState {
    asset_index: usize,
    entry_path: Option<String>,
    texture_index: usize,
    root: SectionNode,
    next_section_id: u64,
}

impl EditorState {
    fn new(asset_index: usize, entry_path: Option<String>, texture_index: usize) -> Self {
        Self {
            asset_index,
            entry_path,
            texture_index,
            root: SectionNode::empty_leaf(1),
            next_section_id: 2,
        }
    }

    fn is_complete(&self) -> bool {
        self.root.count_missing_images() == 0
    }

    fn add_section(&mut self, section_id: u64, axis: SplitAxis) {
        self.root
            .add_section(section_id, axis, &mut self.next_section_id);
    }

    fn remove_section(&mut self, section_id: u64, axis: SplitAxis) {
        self.root.remove_section(section_id, axis);
    }

    fn set_leaf_image(&mut self, leaf_id: u64, image_path: PathBuf, texture: gdk::Texture) {
        self.root.set_leaf_image(leaf_id, image_path, texture);
    }

    fn collect_composition_cells(
        &self,
        width: u32,
        height: u32,
        keep_alpha: bool,
    ) -> Result<Vec<CompositionCell>> {
        let mut cells = Vec::new();
        self.root.collect_composition_cells(
            PixelRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            keep_alpha,
            &mut cells,
        )?;
        Ok(cells)
    }
}

struct CompositionCell {
    rect: PixelRect,
    image_path: PathBuf,
    keep_alpha: bool,
}

#[derive(Clone, Copy)]
struct PixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

enum JobResult {
    ImportFinished {
        source_path: PathBuf,
        result: std::result::Result<ImportedAssetDraft, String>,
    },
    OpenArchiveEntryFinished {
        asset_id: String,
        entry_path: String,
        result: std::result::Result<ArchiveEntryOpenOutcome, String>,
    },
    DownloadCodeWalkerFinished(std::result::Result<(), String>),
    BuildHelperFinished(std::result::Result<(), String>),
    UpdateCodeWalkerFinished(std::result::Result<String, String>),
    PreviewFinished {
        asset_id: String,
        entry_path: Option<String>,
        texture_index: usize,
        result: std::result::Result<PathBuf, String>,
    },
    SaveFinished {
        asset_id: String,
        result: std::result::Result<PathBuf, String>,
    },
    ApplyFinished {
        asset_id: String,
        entry_path: Option<String>,
        texture_index: usize,
        result: std::result::Result<(), String>,
    },
    CopyAllFinished(std::result::Result<usize, String>),
}

struct AppWidgets {
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    app_menu_button: gtk::ToggleButton,
    settings_backdrop: gtk::Button,
    settings_revealer: gtk::Revealer,
    rerun_setup_button: gtk::Button,
    check_updates_button: gtk::Button,
    theme_dropdown: gtk::DropDown,
    mod_folder_path_label: gtk::Label,
    open_mod_folder_button: gtk::Button,
    change_mod_folder_button: gtk::Button,
    backup_before_save_check: gtk::CheckButton,
    back_button: gtk::Button,
    import_button: gtk::Button,
    save_button: gtk::Button,
    open_build_folder_button: gtk::Button,
    copy_all_button: gtk::Button,
    settings_button: gtk::Button,
    status_label: gtk::Label,
    stack: gtk::Stack,
    browser_main_paned: gtk::Paned,
    packages_panel: gtk::Box,
    package_target_label: gtk::Label,
    new_folder_entry: gtk::Entry,
    create_folder_button: gtk::Button,
    import_here_button: gtk::Button,
    move_here_button: gtk::Button,
    package_list_box: gtk::Box,
    textures_title_label: gtk::Label,
    textures_search_entry: gtk::SearchEntry,
    textures_path_label: gtk::Label,
    textures_back_button: gtk::Button,
    textures_notice_label: gtk::Label,
    texture_list_box: gtk::Box,
    preview_asset_label: gtk::Label,
    preview_texture_label: gtk::Label,
    preview_meta_label: gtk::Label,
    preview_picture: gtk::Picture,
    preview_notice_label: gtk::Label,
    edit_button: gtk::Button,
    editor_title_label: gtk::Label,
    editor_meta_label: gtk::Label,
    editor_original_picture: gtk::Picture,
    editor_canvas_box: gtk::Box,
    editor_notice_label: gtk::Label,
    editor_apply_button: gtk::Button,
    copy_destination_window: gtk::Window,
    copy_destination_entry: gtk::Entry,
    browse_copy_destination_button: gtk::Button,
    setup_step_label: gtk::Label,
    setup_title_label: gtk::Label,
    setup_body_label: gtk::Label,
    setup_list_box: gtk::Box,
    setup_back_button: gtk::Button,
    setup_next_button: gtk::Button,
    setup_action_button: gtk::Button,
}

struct App {
    tool_paths: ToolPaths,
    config: AppConfig,
    setup_step: SetupStep,
    setup_status: SetupStatus,
    folders: Vec<FolderNode>,
    next_folder_id: u64,
    selected_folder_id: u64,
    mod_tree_expanded_paths: HashSet<PathBuf>,
    selected_mod_path: Option<PathBuf>,
    pending_import_paths: HashSet<PathBuf>,
    assets: Vec<ImportedAsset>,
    selected_asset: Option<usize>,
    selected_texture: Option<usize>,
    editor: Option<EditorState>,
    last_asset_dir: Option<PathBuf>,
    last_image_dir: Option<PathBuf>,
    last_copy_dir: Option<PathBuf>,
    pending_jobs: usize,
    status: String,
    job_tx: Sender<JobResult>,
    job_rx: Receiver<JobResult>,
    widgets: AppWidgets,
}

impl App {
    fn launch(application: &adw::Application) -> Result<Rc<RefCell<Self>>> {
        let tool_paths = ToolPaths::discover()?;
        let mut config = AppConfig::load(&tool_paths);
        if config.copy_destination.is_empty() {
            config.copy_destination = tool_paths.builds_dir.display().to_string();
        }
        let (job_tx, job_rx) = mpsc::channel();
        let widgets = build_widgets(application, &tool_paths, &config);
        apply_theme(config.theme);
        let setup_status = SetupStatus::detect(&tool_paths);

        let app = Rc::new(RefCell::new(Self {
            tool_paths,
            config,
            setup_step: SetupStep::Welcome,
            setup_status,
            folders: Vec::new(),
            next_folder_id: 1,
            selected_folder_id: ROOT_FOLDER_ID,
            mod_tree_expanded_paths: HashSet::new(),
            selected_mod_path: None,
            pending_import_paths: HashSet::new(),
            assets: Vec::new(),
            selected_asset: None,
            selected_texture: None,
            editor: None,
            last_asset_dir: None,
            last_image_dir: None,
            last_copy_dir: None,
            pending_jobs: 0,
            status: "Ready. Choose the GTA V mods folder to begin.".to_owned(),
            job_tx,
            job_rx,
            widgets,
        }));

        {
            let mut borrowed = app.borrow_mut();
            borrowed.last_asset_dir = borrowed.config.last_asset_dir.clone();
            borrowed.last_image_dir = borrowed.config.last_image_dir.clone();
            borrowed.last_copy_dir = borrowed.config.last_copy_dir.clone();
            if let Some(path) = borrowed.config.mod_folder_path.clone() {
                borrowed.mod_tree_expanded_paths.insert(path);
            }
        }

        connect_signals(&app);
        attach_job_poller(&app);
        app.borrow_mut().refresh_all();
        app.borrow().widgets.window.present();
        Ok(app)
    }

    fn refresh_all(&mut self) {
        self.setup_status = SetupStatus::detect(&self.tool_paths);
        self.refresh_header();
        self.refresh_status();
        self.refresh_setup_page();
        self.refresh_package_tree();
        self.refresh_textures_list();
        self.refresh_preview_pane();
        self.refresh_editor_page();
    }

    fn setup_required(&self) -> bool {
        !self.config.setup_complete || !self.setup_status.setup_ready()
    }

    fn persist_config(&self) {
        if let Err(error) = self.config.save(&self.tool_paths) {
            eprintln!("Failed to save config: {error:#}");
        }
    }

    fn show_toast(&self, message: impl AsRef<str>) {
        self.widgets
            .toast_overlay
            .add_toast(adw::Toast::new(message.as_ref()));
    }

    fn mods_root_path(&self) -> Option<&Path> {
        self.config
            .mod_folder_path
            .as_deref()
            .filter(|path| path.is_dir())
    }

    fn set_mod_folder_path(&mut self, path: PathBuf) {
        self.config.mod_folder_path = Some(path.clone());
        self.mod_tree_expanded_paths.clear();
        self.mod_tree_expanded_paths.insert(path.clone());
        self.selected_mod_path = None;
        self.pending_import_paths.clear();
        self.assets.clear();
        self.selected_asset = None;
        self.selected_texture = None;
        self.editor = None;
        self.close_settings_panel();
        self.persist_config();
        self.set_status(format!("Using GTA V mods folder at {}", path.display()));
        self.show_toast("GTA V mods folder updated.");
        self.refresh_all();
    }

    fn close_settings_panel(&self) {
        sync_settings_panel_visibility(&self.widgets, false);
        self.widgets.app_menu_button.set_active(false);
    }

    fn open_mods_root_folder(&mut self) {
        let Some(directory) = self.mods_root_path() else {
            self.set_status("Choose a GTA V mods folder first.");
            return;
        };

        if let Err(error) = open_directory(directory) {
            self.set_status(format!("Failed to open folder: {error:#}"));
        }
    }

    fn rerun_setup_wizard(&mut self) {
        self.setup_step = SetupStep::Welcome;
        self.widgets.stack.set_visible_child_name("setup");
        self.refresh_all();
    }

    fn refresh_header(&self) {
        let in_editor = self.editor.is_some();
        let setup_required = self.setup_required();

        self.widgets
            .back_button
            .set_visible(in_editor || setup_required);
        self.widgets.app_menu_button.set_sensitive(!in_editor);
        let mod_folder_text = self
            .config
            .mod_folder_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Not configured".to_owned());
        self.widgets
            .mod_folder_path_label
            .set_text(&mod_folder_text);
        self.widgets
            .open_mod_folder_button
            .set_sensitive(self.mods_root_path().is_some());
        self.widgets
            .backup_before_save_check
            .set_active(self.config.backup_before_save);
        self.widgets.save_button.set_visible(false);
        self.widgets.open_build_folder_button.set_visible(false);
        self.widgets.copy_all_button.set_visible(false);
        self.widgets.import_button.set_visible(false);
        self.widgets.settings_button.set_visible(false);
        self.widgets.edit_button.set_sensitive(
            !in_editor
                && !setup_required
                && self
                    .current_texture_entry()
                    .is_some_and(|texture| texture.format.supported_for_write()),
        );
        self.widgets.editor_apply_button.set_sensitive(
            !setup_required
                && self.editor.as_ref().is_some_and(|editor| {
                    let texture = self.assets.get(editor.asset_index).and_then(|asset| {
                        if let Some(entry_path) = editor.entry_path.as_deref() {
                            asset
                                .find_archive_entry(entry_path)
                                .and_then(|entry| entry.textures.get(editor.texture_index))
                        } else {
                            asset.textures.get(editor.texture_index)
                        }
                    });
                    texture.is_some_and(|texture| {
                        texture.format.supported_for_write() && editor.is_complete()
                    })
                }),
        );

        self.widgets
            .setup_back_button
            .set_sensitive(self.setup_step.previous().is_some());
        self.widgets
            .setup_next_button
            .set_sensitive(match self.setup_step {
                SetupStep::Welcome => true,
                SetupStep::ExternalTools => {
                    self.setup_status.cwassettool_source && self.setup_status.codewalker_source
                }
                SetupStep::SystemDependencies => {
                    self.setup_status.git_available
                        && self.setup_status.dotnet_available
                        && self.setup_status.magick_available
                }
                SetupStep::BuildHelper => self.setup_status.cwassettool_binary,
                SetupStep::Ready => false,
            });
        self.widgets
            .setup_action_button
            .set_sensitive(match self.setup_step {
                SetupStep::Welcome => true,
                SetupStep::ExternalTools => !self.setup_status.codewalker_source,
                SetupStep::SystemDependencies => false,
                SetupStep::BuildHelper => !self.setup_status.cwassettool_binary,
                SetupStep::Ready => self.setup_status.setup_ready(),
            });
    }

    fn refresh_status(&self) {
        let prefix = if self.pending_jobs > 0 {
            format!("Working: {} job(s) | ", self.pending_jobs)
        } else {
            String::new()
        };
        self.widgets
            .status_label
            .set_text(&format!("{}{}", prefix, self.status));
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.refresh_status();
    }

    fn refresh_setup_page(&self) {
        if self.setup_required() {
            self.widgets.stack.set_visible_child_name("setup");
        }

        self.widgets
            .setup_step_label
            .set_text(&format!("Step: {}", self.setup_step.title()));

        let (body, action_label) = match self.setup_step {
            SetupStep::Welcome => (
                "GTAV texture importer helps you inspect GTA V YDR and YFT packages, preview embedded textures, replace them with your own images, and rebuild safe output files without modifying the originals.",
                Some("Start Setup"),
            ),
            SetupStep::ExternalTools => (
                "The app needs bundled helper code and CodeWalker source in the app folder. If CodeWalker is missing, the wizard can download it into the local external tools directory before the app starts.",
                if !self.setup_status.codewalker_source {
                    Some("Download CodeWalker")
                } else {
                    None
                },
            ),
            SetupStep::SystemDependencies => (
                "The app also needs git, dotnet, and ImageMagick available on the system. Install any missing dependency before continuing.",
                None,
            ),
            SetupStep::BuildHelper => (
                "CwAssetTool is bundled with the app, but it still needs to be built against the local CodeWalker source before texture import/export can work.",
                if !self.setup_status.cwassettool_binary {
                    Some("Build Helper")
                } else {
                    None
                },
            ),
            SetupStep::Ready => (
                "Setup is complete. You can continue into the main application. You can run the wizard again later from the app menu.",
                Some("Continue"),
            ),
        };

        self.widgets
            .setup_title_label
            .set_text(self.setup_step.title());
        self.widgets.setup_body_label.set_text(body);
        self.widgets
            .setup_action_button
            .set_visible(action_label.is_some());
        if let Some(label) = action_label {
            self.widgets.setup_action_button.set_label(label);
        }

        clear_box(&self.widgets.setup_list_box);
        match self.setup_step {
            SetupStep::Welcome => {
                self.widgets.setup_list_box.append(&setup_info_row(
                    "App folder",
                    &self.tool_paths.app_root.display().to_string(),
                ));
                self.widgets.setup_list_box.append(&setup_info_row(
                    "External tool folder",
                    &self.tool_paths.external_dir.display().to_string(),
                ));
            }
            SetupStep::ExternalTools => {
                self.widgets.setup_list_box.append(&setup_status_row(
                    "Bundled CwAssetTool source",
                    self.setup_status.cwassettool_source,
                    &self.tool_paths.cwassettool_project.display().to_string(),
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "CodeWalker source",
                    self.setup_status.codewalker_source,
                    &self.tool_paths.codewalker_dir.display().to_string(),
                ));
                self.widgets.setup_list_box.append(&setup_info_row(
                    "CodeWalker download URL",
                    self.tool_paths.codewalker_clone_url(),
                ));
            }
            SetupStep::SystemDependencies => {
                self.widgets.setup_list_box.append(&setup_status_row(
                    "git",
                    self.setup_status.git_available,
                    "Required to download CodeWalker and check updates",
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "dotnet",
                    self.setup_status.dotnet_available,
                    "Required to build the bundled CwAssetTool helper",
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "magick",
                    self.setup_status.magick_available,
                    "Required for DDS preview generation and texture conversion",
                ));
            }
            SetupStep::BuildHelper => {
                self.widgets.setup_list_box.append(&setup_status_row(
                    "CwAssetTool binary",
                    self.setup_status.cwassettool_binary,
                    &self.tool_paths.cwassettool_bin.display().to_string(),
                ));
                self.widgets
                    .setup_list_box
                    .append(&setup_info_row("Build target", ".NET Release / net10.0"));
            }
            SetupStep::Ready => {
                self.widgets.setup_list_box.append(&setup_status_row(
                    "Setup complete",
                    self.setup_status.setup_ready(),
                    "All required tools are present and verified",
                ));
            }
        }
    }

    fn folder_path_components(&self, folder_id: u64) -> Vec<String> {
        let mut current = folder_id;
        let mut parts = Vec::new();
        while current != ROOT_FOLDER_ID {
            let Some(folder) = self.folders.iter().find(|folder| folder.id == current) else {
                break;
            };
            parts.push(folder.name.clone());
            current = folder.parent_id;
        }
        parts.reverse();
        parts
    }

    fn folder_path_buf(&self, folder_id: u64) -> PathBuf {
        let mut path = PathBuf::new();
        for part in self.folder_path_components(folder_id) {
            path.push(part);
        }
        path
    }

    fn folder_path_string(&self, folder_id: u64) -> String {
        let parts = self.folder_path_components(folder_id);
        if parts.is_empty() {
            "Workspace".to_owned()
        } else {
            format!("Workspace / {}", parts.join(" / "))
        }
    }

    fn asset_index_for_source_path(&self, path: &Path) -> Option<usize> {
        self.assets
            .iter()
            .position(|asset| asset.source_path == path)
    }

    fn toggle_mod_tree_path(&mut self, path: &Path) {
        let path = path.to_path_buf();
        if self.mod_tree_expanded_paths.contains(&path) {
            self.mod_tree_expanded_paths.remove(&path);
        } else {
            self.mod_tree_expanded_paths.insert(path);
        }
        self.refresh_package_tree();
    }

    fn open_mod_asset_path(&mut self, path: PathBuf) {
        self.selected_mod_path = Some(path.clone());

        if let Some(asset_index) = self.asset_index_for_source_path(&path) {
            self.select_asset(asset_index);
            return;
        }

        if self.pending_import_paths.contains(&path) {
            self.set_status(format!("Opening {}...", path.display()));
            self.refresh_package_tree();
            return;
        }

        self.pending_import_paths.insert(path.clone());
        self.pending_jobs += 1;
        self.set_status(format!("Opening {}...", path.display()));

        let tool_paths = self.tool_paths.clone();
        let tx = self.job_tx.clone();
        thread::spawn(move || {
            let result = import_asset_draft(&tool_paths, &path, ROOT_FOLDER_ID)
                .map_err(|error| format!("{}", error));
            let _ = tx.send(JobResult::ImportFinished {
                source_path: path,
                result,
            });
        });

        self.refresh_package_tree();
    }

    fn active_archive_entry_path(&self, asset_index: usize) -> Option<String> {
        let asset = self.assets.get(asset_index)?;
        if !asset.is_archive() {
            return None;
        }
        let entry_path = asset.archive_selected_file.as_ref()?;
        asset.find_archive_entry(entry_path)?;
        Some(entry_path.clone())
    }

    fn current_texture_entry(&self) -> Option<&TextureEntry> {
        let asset_index = self.selected_asset?;
        let texture_index = self.selected_texture?;
        let asset = self.assets.get(asset_index)?;

        if let Some(entry_path) = self.active_archive_entry_path(asset_index) {
            asset
                .find_archive_entry(&entry_path)?
                .textures
                .get(texture_index)
        } else {
            asset.textures.get(texture_index)
        }
    }

    fn current_texture_entry_mut(&mut self) -> Option<&mut TextureEntry> {
        let asset_index = self.selected_asset?;
        let texture_index = self.selected_texture?;
        let entry_path = self.active_archive_entry_path(asset_index);
        let asset = self.assets.get_mut(asset_index)?;

        if let Some(entry_path) = entry_path {
            asset
                .find_archive_entry_mut(&entry_path)?
                .textures
                .get_mut(texture_index)
        } else {
            asset.textures.get_mut(texture_index)
        }
    }

    fn current_archive_entry(&self) -> Option<&ImportedArchiveEntry> {
        let asset_index = self.selected_asset?;
        let entry_path = self.active_archive_entry_path(asset_index)?;
        self.assets
            .get(asset_index)?
            .find_archive_entry(&entry_path)
    }

    fn archive_current_node<'a>(&self, asset: &'a ImportedAsset) -> Option<&'a RpfTreeNode> {
        let current_path = asset
            .archive_current_path
            .as_deref()
            .or(asset.archive_root_path())?;
        asset.find_archive_node(current_path)
    }

    fn archive_parent_path_for(&self, asset: &ImportedAsset, path: &str) -> Option<String> {
        let root_path = asset.archive_root_path()?;
        if path == root_path {
            return None;
        }

        let mut parent_path = None;
        if let Some(tree) = &asset.archive_tree {
            find_archive_parent_path(tree, path, &mut parent_path);
        }
        parent_path
    }

    fn archive_parent_path(&self, asset: &ImportedAsset) -> Option<String> {
        let current_path = asset.archive_current_path.as_deref()?;
        self.archive_parent_path_for(asset, current_path)
    }

    fn browse_archive_path(&mut self, asset_index: usize, path: String) {
        let Some(asset) = self.assets.get_mut(asset_index) else {
            return;
        };
        asset.archive_current_path = Some(path);
        asset.archive_selected_file = None;
        asset.archive_file_notice = None;
        asset.archive_file_loading_path = None;
        self.selected_texture = None;
        self.refresh_all();
    }

    fn toggle_archive_path_expanded(&mut self, asset_index: usize, path: &str) {
        let Some(asset) = self.assets.get_mut(asset_index) else {
            return;
        };

        if asset.archive_expanded_paths.contains(path) {
            asset.archive_expanded_paths.remove(path);
        } else {
            asset.archive_expanded_paths.insert(path.to_owned());
        }

        self.refresh_textures_list();
    }

    fn set_archive_search_query(&mut self, query: String) {
        let Some(asset_index) = self.selected_asset else {
            return;
        };
        let Some(asset) = self.assets.get_mut(asset_index) else {
            return;
        };
        if !asset.is_archive() {
            return;
        }
        if asset.archive_search_query == query {
            return;
        }

        asset.archive_search_query = query;
        self.refresh_textures_list();
    }

    fn select_archive_parent(&mut self) {
        let Some(asset_index) = self.selected_asset else {
            return;
        };
        if let Some(asset) = self.assets.get_mut(asset_index) {
            if asset.archive_selected_file.is_some() {
                asset.archive_selected_file = None;
                asset.archive_file_notice = None;
                asset.archive_file_loading_path = None;
                self.selected_texture = None;
                self.refresh_all();
                return;
            }
        }
        let Some(parent_path) = self
            .assets
            .get(asset_index)
            .and_then(|asset| self.archive_parent_path(asset))
        else {
            return;
        };
        self.browse_archive_path(asset_index, parent_path);
    }

    fn open_archive_file(&mut self, asset_index: usize, entry_path: String) {
        let Some(node) = self
            .assets
            .get(asset_index)
            .and_then(|asset| asset.find_archive_node(&entry_path))
            .cloned()
        else {
            self.set_status("Archive entry could not be found.");
            return;
        };
        let parent_path = self
            .assets
            .get(asset_index)
            .and_then(|asset| self.archive_parent_path_for(asset, &entry_path));
        let Some(asset) = self.assets.get_mut(asset_index) else {
            return;
        };

        asset.archive_selected_file = Some(entry_path.clone());
        if let Some(parent_path) = parent_path {
            asset.archive_current_path = Some(parent_path);
        }
        asset.archive_file_notice = None;
        self.selected_texture = None;

        if !node.supported_asset {
            asset.archive_file_loading_path = None;
            asset.archive_file_notice = Some("Not supported.".to_owned());
            let _ = asset;
            self.refresh_all();
            return;
        }

        if let Some(existing_textures_len) = asset
            .find_archive_entry(&entry_path)
            .map(|entry| entry.textures.len())
        {
            asset.archive_file_loading_path = None;
            asset.archive_file_notice = if existing_textures_len == 0 {
                Some("Not supported.".to_owned())
            } else {
                None
            };
            self.selected_texture = if existing_textures_len == 0 {
                None
            } else {
                Some(0)
            };
            let _ = asset;
            self.request_preview_for_selected_texture();
            self.refresh_all();
            return;
        }

        let asset_id = asset.id.clone();
        let archive_path = asset.source_path.clone();
        let file_title = node.name.clone();
        let Some(_) = AssetKind::from_path(Path::new(&file_title)) else {
            asset.archive_file_loading_path = None;
            asset.archive_file_notice = Some("Not supported.".to_owned());
            let _ = asset;
            self.refresh_all();
            return;
        };
        asset.archive_file_loading_path = Some(entry_path.clone());
        let tool_paths = self.tool_paths.clone();
        let tx = self.job_tx.clone();

        self.pending_jobs += 1;
        self.set_status(format!("Opening {}...", file_title));

        thread::spawn(move || {
            let result = export_archive_entry_draft(&tool_paths, &archive_path, &entry_path)
                .map(ArchiveEntryOpenOutcome::Supported)
                .or_else(|error| {
                    if error.to_string().contains("No DDS textures were found") {
                        Ok(ArchiveEntryOpenOutcome::Unsupported(
                            "Not supported.".to_owned(),
                        ))
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| format!("{}", error));

            let _ = tx.send(JobResult::OpenArchiveEntryFinished {
                asset_id,
                entry_path,
                result,
            });
        });

        self.refresh_all();
    }

    fn select_asset(&mut self, asset_index: usize) {
        self.selected_asset = Some(asset_index);
        if let Some(asset) = self.assets.get(asset_index) {
            self.selected_mod_path = Some(asset.source_path.clone());
        }
        if self.assets[asset_index].is_archive() {
            if self.assets[asset_index].archive_current_path.is_none() {
                self.assets[asset_index].archive_current_path = self.assets[asset_index]
                    .archive_root_path()
                    .map(ToOwned::to_owned);
            }
            self.assets[asset_index].archive_selected_file = None;
            self.assets[asset_index].archive_file_notice = None;
            self.assets[asset_index].archive_file_loading_path = None;
            self.selected_texture = None;
        } else {
            self.selected_texture = if self.assets[asset_index].textures.is_empty() {
                None
            } else {
                Some(0)
            };
        }
        self.request_preview_for_selected_texture();
        self.refresh_all();
    }

    fn select_texture(&mut self, texture_index: usize) {
        self.selected_texture = Some(texture_index);
        self.request_preview_for_selected_texture();
        self.refresh_all();
    }

    fn request_preview_for_selected_texture(&mut self) {
        let Some(asset_index) = self.selected_asset else {
            return;
        };
        let Some(texture_index) = self.selected_texture else {
            return;
        };

        let entry_path = self.active_archive_entry_path(asset_index);

        let asset_id = match self.assets.get(asset_index) {
            Some(asset) => asset.id.clone(),
            None => return,
        };

        let (dds_path, preview_png_path) = {
            let Some(texture) = self.current_texture_entry_mut() else {
                return;
            };

            if texture.preview_texture.is_some() || texture.preview_loading {
                return;
            }

            texture.preview_loading = true;
            (texture.dds_path.clone(), texture.preview_png_path.clone())
        };

        self.pending_jobs += 1;
        self.refresh_status();
        let tx = self.job_tx.clone();

        thread::spawn(move || {
            let result = (|| -> Result<PathBuf> {
                if !preview_png_path.is_file() {
                    generate_preview_png(&dds_path, &preview_png_path)?;
                }
                Ok(preview_png_path)
            })()
            .map_err(|error| format!("{}", error));

            let _ = tx.send(JobResult::PreviewFinished {
                asset_id,
                entry_path,
                texture_index,
                result,
            });
        });
    }

    fn handle_job_results(&mut self) {
        while let Ok(job) = self.job_rx.try_recv() {
            self.pending_jobs = self.pending_jobs.saturating_sub(1);

            match job {
                JobResult::ImportFinished {
                    source_path,
                    result,
                } => {
                    self.pending_import_paths.remove(&source_path);
                    match result {
                        Ok(draft) => {
                            if let Some(existing_index) =
                                self.asset_index_for_source_path(&draft.source_path)
                            {
                                self.select_asset(existing_index);
                                self.set_status(format!(
                                    "Opened {}",
                                    self.assets[existing_index].title()
                                ));
                                continue;
                            }

                            let asset = ImportedAsset {
                                id: draft.id,
                                source_path: draft.source_path,
                                kind: draft.kind,
                                folder_id: draft.folder_id,
                                xml_path: draft.xml_path,
                                textures: draft
                                    .textures
                                    .into_iter()
                                    .map(|texture| TextureEntry {
                                        name: texture.name,
                                        file_name: texture.file_name,
                                        width: texture.width,
                                        height: texture.height,
                                        mips: texture.mips,
                                        format: texture.format,
                                        usage: texture.usage,
                                        dds_path: texture.dds_path,
                                        preview_png_path: texture.preview_png_path,
                                        preview_texture: None,
                                        preview_loading: false,
                                        modified: false,
                                    })
                                    .collect(),
                                archive_tree: draft.archive_tree,
                                archive_entries: Vec::new(),
                                archive_current_path: None,
                                archive_expanded_paths: HashSet::new(),
                                archive_selected_file: None,
                                archive_file_notice: None,
                                archive_file_loading_path: None,
                                archive_search_query: String::new(),
                                dirty: false,
                                last_saved_path: None,
                            };

                            self.assets.push(asset);
                            let new_index = self.assets.len() - 1;
                            self.select_asset(new_index);
                            self.set_status(format!("Opened {}", self.assets[new_index].title()));
                        }
                        Err(error) => {
                            self.set_status(format!("Import failed: {error}"));
                        }
                    }
                }
                JobResult::OpenArchiveEntryFinished {
                    asset_id,
                    entry_path,
                    result,
                } => {
                    if let Some(asset) = self.assets.iter_mut().find(|asset| asset.id == asset_id) {
                        asset.archive_file_loading_path = None;
                        match result {
                            Ok(ArchiveEntryOpenOutcome::Supported(draft)) => {
                                let opened_title = draft.title.clone();
                                let imported_entry = ImportedArchiveEntry {
                                    entry_path: draft.entry_path.clone(),
                                    title: draft.title,
                                    xml_path: draft.xml_path,
                                    textures: draft
                                        .textures
                                        .into_iter()
                                        .map(|texture| TextureEntry {
                                            name: texture.name,
                                            file_name: texture.file_name,
                                            width: texture.width,
                                            height: texture.height,
                                            mips: texture.mips,
                                            format: texture.format,
                                            usage: texture.usage,
                                            dds_path: texture.dds_path,
                                            preview_png_path: texture.preview_png_path,
                                            preview_texture: None,
                                            preview_loading: false,
                                            modified: false,
                                        })
                                        .collect(),
                                    dirty: false,
                                };

                                if let Some(existing) =
                                    asset.find_archive_entry_mut(&draft.entry_path)
                                {
                                    *existing = imported_entry;
                                } else {
                                    asset.archive_entries.push(imported_entry);
                                }

                                asset.archive_selected_file = Some(entry_path.clone());
                                asset.archive_file_notice = None;
                                self.selected_texture = Some(0);
                                self.request_preview_for_selected_texture();
                                self.set_status(format!("Opened {}", opened_title));
                            }
                            Ok(ArchiveEntryOpenOutcome::Unsupported(message)) => {
                                asset.archive_selected_file = Some(entry_path);
                                asset.archive_file_notice = Some(message.clone());
                                self.selected_texture = None;
                                self.set_status(message);
                            }
                            Err(error) => {
                                asset.archive_file_notice = Some("Not supported.".to_owned());
                                self.selected_texture = None;
                                self.set_status(format!("Open failed: {error}"));
                            }
                        }
                    }
                }
                JobResult::DownloadCodeWalkerFinished(result) => match result {
                    Ok(()) => {
                        self.setup_status = SetupStatus::detect(&self.tool_paths);
                        if self.setup_step == SetupStep::ExternalTools {
                            self.setup_step = SetupStep::SystemDependencies;
                        }
                        self.set_status("Downloaded CodeWalker into the app external folder.");
                    }
                    Err(error) => {
                        self.set_status(format!("CodeWalker download failed: {error}"));
                    }
                },
                JobResult::BuildHelperFinished(result) => match result {
                    Ok(()) => {
                        self.setup_status = SetupStatus::detect(&self.tool_paths);
                        if self.setup_step == SetupStep::BuildHelper {
                            self.setup_step = SetupStep::Ready;
                        }
                        self.set_status("Built the CwAssetTool helper successfully.");
                    }
                    Err(error) => {
                        self.set_status(format!("Helper build failed: {error}"));
                    }
                },
                JobResult::UpdateCodeWalkerFinished(result) => match result {
                    Ok(message) => {
                        self.setup_status = SetupStatus::detect(&self.tool_paths);
                        self.set_status(message);
                    }
                    Err(error) => {
                        self.set_status(format!("External update check failed: {error}"));
                    }
                },
                JobResult::PreviewFinished {
                    asset_id,
                    entry_path,
                    texture_index,
                    result,
                } => {
                    let mut status_message = None;
                    if let Some(asset) = self.assets.iter_mut().find(|asset| asset.id == asset_id) {
                        let texture = if let Some(entry_path) = entry_path.as_deref() {
                            asset
                                .find_archive_entry_mut(entry_path)
                                .and_then(|entry| entry.textures.get_mut(texture_index))
                        } else {
                            asset.textures.get_mut(texture_index)
                        };
                        if let Some(texture) = texture {
                            texture.preview_loading = false;
                            match result {
                                Ok(path) => match texture_from_path(&path) {
                                    Ok(preview) => {
                                        let texture_name = texture.name.clone();
                                        texture.preview_texture = Some(preview);
                                        status_message =
                                            Some(format!("Loaded preview for {}", texture_name));
                                    }
                                    Err(error) => {
                                        status_message =
                                            Some(format!("Preview load failed: {error:#}"));
                                    }
                                },
                                Err(error) => {
                                    status_message =
                                        Some(format!("Preview generation failed: {error}"));
                                }
                            }
                        }
                    }
                    if let Some(message) = status_message {
                        self.set_status(message);
                    }
                    self.refresh_preview_pane();
                    self.refresh_editor_page();
                }
                JobResult::SaveFinished { asset_id, result } => {
                    if let Some(asset) = self.assets.iter_mut().find(|asset| asset.id == asset_id) {
                        match result {
                            Ok(path) => {
                                asset.last_saved_path = Some(path.clone());
                                asset.dirty = false;
                                for texture in &mut asset.textures {
                                    texture.modified = false;
                                }
                                for entry in &mut asset.archive_entries {
                                    entry.dirty = false;
                                    for texture in &mut entry.textures {
                                        texture.modified = false;
                                    }
                                }
                                let asset_title = asset.title();
                                self.set_status(format!("Applied changes to {}", path.display()));
                                self.show_toast(format!("Changes applied to {}.", asset_title));
                            }
                            Err(error) => {
                                self.set_status(format!("Save failed: {error}"));
                            }
                        }
                    }
                    self.refresh_header();
                    self.refresh_package_tree();
                    self.refresh_textures_list();
                    self.refresh_preview_pane();
                }
                JobResult::ApplyFinished {
                    asset_id,
                    entry_path,
                    texture_index,
                    result,
                } => {
                    let mut apply_success = false;
                    let mut status_message = None;
                    if let Some(asset_index) =
                        self.assets.iter().position(|asset| asset.id == asset_id)
                    {
                        let texture = if let Some(entry_path) = entry_path.as_deref() {
                            self.assets[asset_index]
                                .find_archive_entry_mut(entry_path)
                                .and_then(|entry| entry.textures.get_mut(texture_index))
                        } else {
                            self.assets[asset_index].textures.get_mut(texture_index)
                        };
                        if let Some(texture) = texture {
                            match result {
                                Ok(()) => {
                                    let texture_name = texture.name.clone();
                                    texture.preview_texture = None;
                                    texture.preview_loading = false;
                                    texture.modified = true;
                                    status_message =
                                        Some(format!("Applied changes to {}", texture_name));
                                    apply_success = true;
                                }
                                Err(error) => {
                                    status_message = Some(format!("Apply failed: {error}"));
                                }
                            }
                        }

                        if apply_success {
                            if let Some(entry_path) = entry_path.as_deref() {
                                if let Some(entry) =
                                    self.assets[asset_index].find_archive_entry_mut(entry_path)
                                {
                                    entry.dirty = true;
                                }
                                self.assets[asset_index].sync_archive_dirty();
                            } else {
                                self.assets[asset_index].dirty = true;
                            }
                            self.editor = None;
                            self.selected_asset = Some(asset_index);
                            self.selected_texture = Some(texture_index);
                            self.request_preview_for_selected_texture();
                        }
                    }

                    if let Some(message) = status_message {
                        self.set_status(message);
                    }
                    self.refresh_all();
                }
                JobResult::CopyAllFinished(result) => match result {
                    Ok(count) => self.set_status(format!("Copied {count} built file(s).")),
                    Err(error) => self.set_status(format!("Copy All failed: {error}")),
                },
            }

            self.refresh_all();
        }
    }

    fn refresh_package_tree(&self) {
        clear_box(&self.widgets.package_list_box);

        let Some(mods_root) = self.mods_root_path() else {
            self.widgets.packages_panel.set_size_request(250, -1);
            self.widgets.browser_main_paned.set_position(250);
            self.widgets
                .package_target_label
                .set_text("GTA V mods folder not configured");
            let empty_state = gtk::Box::new(gtk::Orientation::Vertical, 12);
            empty_state.set_margin_top(20);
            empty_state.set_margin_bottom(12);
            empty_state.set_margin_start(6);
            empty_state.set_margin_end(6);
            empty_state.add_css_class("card");
            let choose_button = gtk::Button::from_icon_name("folder-open-symbolic");
            choose_button.add_css_class("suggested-action");
            choose_button.set_tooltip_text(Some("Choose GTA V mods folder"));
            choose_button.connect_clicked(move |_| {
                with_app_ref(|app| {
                    present_mod_folder_dialog(&app);
                });
            });
            choose_button.set_halign(gtk::Align::Start);
            let message = gtk::Label::new(Some(
                "The GTA V mods folder is required. It is important to make a backup before changing files.",
            ));
            message.set_xalign(0.0);
            message.set_wrap(true);
            empty_state.append(&choose_button);
            empty_state.append(&message);
            self.widgets.package_list_box.append(&empty_state);
            return;
        };

        self.widgets.packages_panel.set_size_request(320, -1);
        self.widgets.browser_main_paned.set_position(360);
        self.widgets
            .package_target_label
            .set_text(&format!("Mods folder: {}", mods_root.display()));
        append_mod_folder_rows(self, &self.widgets.package_list_box, mods_root, 0);
    }

    fn refresh_textures_list(&self) {
        clear_box(&self.widgets.texture_list_box);
        self.widgets.textures_notice_label.set_text("");
        self.widgets.textures_path_label.set_text("");

        let Some(asset_index) = self.selected_asset else {
            self.widgets.textures_search_entry.set_visible(false);
            self.widgets.textures_back_button.set_visible(false);
            self.widgets
                .textures_title_label
                .set_text("Select a package from the left pane.");
            self.widgets.textures_path_label.set_text("");
            return;
        };

        let Some(asset) = self.assets.get(asset_index) else {
            self.widgets.textures_search_entry.set_visible(false);
            self.widgets.textures_back_button.set_visible(false);
            self.widgets
                .textures_title_label
                .set_text("Select a package from the left pane.");
            self.widgets.textures_path_label.set_text("");
            return;
        };

        if asset.is_archive() {
            self.widgets.textures_search_entry.set_visible(true);
            if self.widgets.textures_search_entry.text().as_str() != asset.archive_search_query {
                self.widgets
                    .textures_search_entry
                    .set_text(&asset.archive_search_query);
            }
            self.widgets
                .textures_title_label
                .set_text("Archive Explorer");
            self.widgets.textures_path_label.set_text(
                &asset
                    .archive_current_display_path()
                    .unwrap_or_else(|| asset.title()),
            );
            self.widgets.textures_back_button.set_visible(
                asset.archive_selected_file.is_some() || self.archive_parent_path(asset).is_some(),
            );

            if let Some(entry_path) = asset.archive_selected_file.as_deref() {
                self.widgets.textures_search_entry.set_visible(false);
                let node = asset.find_archive_node(entry_path);
                self.widgets.textures_path_label.set_text(
                    &node
                        .map(|node| node.display_path.clone())
                        .unwrap_or_else(|| asset.title()),
                );

                if let Some(loading_path) = asset.archive_file_loading_path.as_deref() {
                    if loading_path == entry_path {
                        self.widgets
                            .textures_notice_label
                            .set_text("Loading file...");
                        return;
                    }
                }

                if let Some(entry) = asset.find_archive_entry(entry_path) {
                    self.widgets.textures_title_label.set_text(&format!(
                        "{} ({} textures)",
                        entry.title,
                        entry.textures.len()
                    ));
                    if entry.textures.is_empty() {
                        self.widgets
                            .textures_notice_label
                            .set_text("Not supported.");
                    } else {
                        append_texture_rows(
                            &self.widgets.texture_list_box,
                            &entry.textures,
                            self.selected_texture,
                        );
                    }
                    return;
                }

                self.widgets.textures_notice_label.set_text(
                    asset
                        .archive_file_notice
                        .as_deref()
                        .unwrap_or("Not supported."),
                );
                return;
            }

            if let Some(node) = self.archive_current_node(asset) {
                let trimmed_query = asset.archive_search_query.trim();
                let filter_query =
                    (!trimmed_query.is_empty()).then(|| trimmed_query.to_ascii_lowercase());

                if node.children.is_empty() {
                    self.widgets
                        .textures_notice_label
                        .set_text("This folder is empty.");
                    return;
                }

                let rendered_rows = append_archive_rows(
                    &self.widgets.texture_list_box,
                    asset_index,
                    node,
                    &asset.archive_expanded_paths,
                    filter_query.as_deref(),
                    0,
                );
                if rendered_rows == 0 {
                    self.widgets.textures_notice_label.set_text(&format!(
                        "No items match \"{}\" in this section.",
                        trimmed_query
                    ));
                }
            }
            return;
        }

        self.widgets.textures_search_entry.set_visible(false);
        self.widgets.textures_back_button.set_visible(false);
        self.widgets.textures_title_label.set_text(&format!(
            "{} ({} textures)",
            asset.title(),
            asset.textures.len()
        ));
        self.widgets.textures_path_label.set_text(&asset.title());
        append_texture_rows(
            &self.widgets.texture_list_box,
            &asset.textures,
            self.selected_texture,
        );
    }

    fn refresh_preview_pane(&self) {
        let Some(asset_index) = self.selected_asset else {
            self.widgets
                .preview_asset_label
                .set_text("Select a package first.");
            self.widgets.preview_texture_label.set_text("");
            self.widgets.preview_meta_label.set_text("");
            self.widgets.preview_notice_label.set_text("");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
            return;
        };
        let asset = &self.assets[asset_index];

        if !asset.is_archive() && self.selected_texture.is_none() {
            self.widgets
                .preview_asset_label
                .set_text("Select a texture to preview.");
            self.widgets.preview_texture_label.set_text("");
            self.widgets.preview_meta_label.set_text("");
            self.widgets.preview_notice_label.set_text("");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
            return;
        }

        if asset.is_archive() && asset.archive_selected_file.is_none() {
            self.widgets.preview_asset_label.set_text(&asset.title());
            self.widgets
                .preview_texture_label
                .set_text("Archive browser");
            self.widgets
                .preview_meta_label
                .set_text("Select a file in the middle pane to inspect its textures.");
            self.widgets.preview_notice_label.set_text("");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
            return;
        }

        let Some(texture) = self.current_texture_entry() else {
            self.widgets.preview_asset_label.set_text(&asset.title());
            self.widgets.preview_texture_label.set_text(
                asset
                    .archive_selected_file
                    .as_deref()
                    .and_then(|entry_path| {
                        asset
                            .find_archive_node(entry_path)
                            .map(|node| node.name.as_str())
                    })
                    .unwrap_or("Select a texture to preview."),
            );
            self.widgets.preview_meta_label.set_text("");
            self.widgets.preview_notice_label.set_text(
                asset
                    .archive_file_notice
                    .as_deref()
                    .unwrap_or("Not supported."),
            );
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
            return;
        };

        let entry_title = self
            .current_archive_entry()
            .map(|entry| entry.title.clone());

        self.widgets.preview_asset_label.set_text(&asset.title());
        if let Some(entry_title) = entry_title {
            self.widgets.preview_asset_label.set_text(&format!(
                "{} / {}",
                asset.title(),
                entry_title
            ));
        }
        self.widgets.preview_texture_label.set_text(&texture.name);
        self.widgets.preview_meta_label.set_text(&format!(
            "{}x{} | {} | {} mips | {}",
            texture.width,
            texture.height,
            texture.format.label(),
            texture.mips,
            texture.usage
        ));

        if texture.preview_loading {
            self.widgets
                .preview_notice_label
                .set_text("Loading preview...");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
        } else if let Some(preview) = &texture.preview_texture {
            self.widgets.preview_notice_label.set_text("");
            self.widgets.preview_picture.set_paintable(Some(preview));
        } else {
            self.widgets
                .preview_notice_label
                .set_text("Preview not available yet.");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
        }
    }

    fn open_editor_page(&mut self) {
        let Some(asset_index) = self.selected_asset else {
            self.set_status("Select a package first.");
            return;
        };
        let Some(texture_index) = self.selected_texture else {
            self.set_status("Select a texture first.");
            return;
        };
        let entry_path = self.active_archive_entry_path(asset_index);
        let Some(texture) = self.current_texture_entry() else {
            return;
        };

        if !texture.format.supported_for_write() {
            self.set_status(format!(
                "{} is not yet writable by the app.",
                texture.format.label()
            ));
            return;
        }

        self.editor = Some(EditorState::new(asset_index, entry_path, texture_index));
        self.widgets.stack.set_visible_child_name("editor");
        self.refresh_all();
    }

    fn close_editor_page(&mut self) {
        self.editor = None;
        self.widgets.stack.set_visible_child_name("browser");
        self.refresh_all();
    }

    fn refresh_editor_page(&self) {
        if self.setup_required() {
            self.widgets.stack.set_visible_child_name("setup");
            return;
        }

        let Some(editor) = &self.editor else {
            self.widgets.stack.set_visible_child_name("browser");
            return;
        };

        let asset = &self.assets[editor.asset_index];
        let texture = if let Some(entry_path) = editor.entry_path.as_deref() {
            let Some(entry) = asset.find_archive_entry(entry_path) else {
                self.widgets.stack.set_visible_child_name("browser");
                return;
            };
            let Some(texture) = entry.textures.get(editor.texture_index) else {
                self.widgets.stack.set_visible_child_name("browser");
                return;
            };
            self.widgets.editor_title_label.set_text(&format!(
                "Editing {} / {} / {}",
                asset.title(),
                entry.title,
                texture.name
            ));
            texture
        } else {
            let Some(texture) = asset.textures.get(editor.texture_index) else {
                self.widgets.stack.set_visible_child_name("browser");
                return;
            };
            self.widgets.editor_title_label.set_text(&format!(
                "Editing {} / {}",
                asset.title(),
                texture.name
            ));
            texture
        };
        self.widgets.stack.set_visible_child_name("editor");
        self.widgets.editor_meta_label.set_text(&format!(
            "Target: {}x{} | {} | {} mips",
            texture.width,
            texture.height,
            texture.format.label(),
            texture.mips
        ));

        if texture.preview_loading {
            self.widgets
                .editor_notice_label
                .set_text("Loading original preview...");
            self.widgets
                .editor_original_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
        } else if let Some(preview) = &texture.preview_texture {
            self.widgets.editor_notice_label.set_text("");
            self.widgets
                .editor_original_picture
                .set_paintable(Some(preview));
        } else {
            self.widgets
                .editor_notice_label
                .set_text("Original preview not available yet.");
            self.widgets
                .editor_original_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
        }

        clear_box(&self.widgets.editor_canvas_box);
        let canvas = build_section_widget(&editor.root, None);
        let aspect = gtk::AspectFrame::builder()
            .ratio(texture.width as f32 / texture.height as f32)
            .hexpand(true)
            .vexpand(true)
            .child(&canvas)
            .build();
        self.widgets.editor_canvas_box.append(&aspect);
        self.widgets
            .editor_apply_button
            .set_sensitive(editor.is_complete());
    }

    fn create_folder(&mut self) {
        let name = self.widgets.new_folder_entry.text().trim().to_owned();
        if name.is_empty() {
            self.set_status("Enter a folder name first.");
            return;
        }

        self.folders.push(FolderNode {
            id: self.next_folder_id,
            parent_id: self.selected_folder_id,
            name,
        });
        self.next_folder_id += 1;
        self.widgets.new_folder_entry.set_text("");
        self.set_status("Folder created.");
        self.refresh_package_tree();
    }

    fn move_selected_asset_to_selected_folder(&mut self) {
        let Some(asset_index) = self.selected_asset else {
            self.set_status("Select a package to move first.");
            return;
        };
        if let Some(asset) = self.assets.get_mut(asset_index) {
            asset.folder_id = self.selected_folder_id;
            let asset_title = asset.title();
            self.set_status(format!(
                "Moved {} to {}",
                asset_title,
                self.folder_path_string(self.selected_folder_id)
            ));
            self.refresh_package_tree();
        }
    }

    fn queue_import_files(&mut self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }

        let file_count = files.len();
        for file in files {
            let tool_paths = self.tool_paths.clone();
            let tx = self.job_tx.clone();
            let folder_id = self.selected_folder_id;
            self.pending_jobs += 1;

            thread::spawn(move || {
                let result = import_asset_draft(&tool_paths, &file, folder_id)
                    .map_err(|error| format!("{}", error));
                let _ = tx.send(JobResult::ImportFinished {
                    source_path: file,
                    result,
                });
            });
        }

        self.set_status(format!("Importing {} file(s)...", file_count));
        self.refresh_header();
    }

    fn queue_save_selected_asset(&mut self) {
        let Some(asset_index) = self.selected_asset else {
            self.set_status("Select a package first.");
            return;
        };
        self.queue_save_asset_by_index(asset_index);
    }

    fn queue_save_asset_by_index(&mut self, asset_index: usize) {
        let Some(asset) = self.assets.get(asset_index) else {
            return;
        };
        if !asset.dirty {
            self.set_status("There are no unsaved changes for this file.");
            return;
        }

        let asset_id = asset.id.clone();
        let tx = self.job_tx.clone();
        let tool_paths = self.tool_paths.clone();
        let source_path = asset.source_path.clone();
        let xml_path = asset.xml_path.clone();
        let backup_before_save = self.config.backup_before_save;
        let archive_changes: Vec<_> = asset
            .archive_entries
            .iter()
            .filter(|entry| entry.dirty)
            .map(|entry| RpfBuildChange {
                entry_path: entry.entry_path.clone(),
                xml_path: entry.xml_path.clone(),
            })
            .collect();

        self.pending_jobs += 1;
        self.set_status(format!("Applying changes to {}...", asset.title()));

        thread::spawn(move || {
            let result = save_asset_in_place_job(
                &tool_paths,
                &source_path,
                xml_path.as_deref(),
                archive_changes,
                backup_before_save,
            )
            .map_err(|error| format!("{}", error));
            let _ = tx.send(JobResult::SaveFinished { asset_id, result });
        });
    }

    fn queue_apply_editor(&mut self) {
        let Some(editor) = &self.editor else {
            return;
        };
        let asset = &self.assets[editor.asset_index];
        let texture = if let Some(entry_path) = editor.entry_path.as_deref() {
            let Some(entry) = asset.find_archive_entry(entry_path) else {
                self.set_status("The selected archive entry is no longer available.");
                return;
            };
            let Some(texture) = entry.textures.get(editor.texture_index) else {
                return;
            };
            texture
        } else {
            let Some(texture) = asset.textures.get(editor.texture_index) else {
                return;
            };
            texture
        };

        if !texture.format.supported_for_write() {
            self.set_status(format!(
                "{} is not yet writable by the app.",
                texture.format.label()
            ));
            return;
        }

        if !editor.is_complete() {
            self.set_status("Every section needs an image before you apply changes.");
            return;
        }

        let cells = match editor.collect_composition_cells(
            texture.width,
            texture.height,
            texture.format.supports_alpha(),
        ) {
            Ok(cells) => cells,
            Err(error) => {
                self.set_status(format!("Apply failed: {error:#}"));
                return;
            }
        };

        let asset_id = asset.id.clone();
        let entry_path = editor.entry_path.clone();
        let texture_index = editor.texture_index;
        let dds_path = texture.dds_path.clone();
        let preview_png_path = texture.preview_png_path.clone();
        let format = texture.format.clone();
        let mips = texture.mips;
        let width = texture.width;
        let height = texture.height;
        let tx = self.job_tx.clone();

        self.pending_jobs += 1;
        self.set_status(format!("Applying changes to {}...", texture.name));

        thread::spawn(move || {
            let result = apply_texture_job(
                &dds_path,
                &preview_png_path,
                &format,
                mips,
                width,
                height,
                cells,
            )
            .map_err(|error| format!("{}", error));

            let _ = tx.send(JobResult::ApplyFinished {
                asset_id,
                entry_path,
                texture_index,
                result,
            });
        });
    }

    fn queue_copy_all(&mut self) {
        let destination = self.widgets.copy_destination_entry.text();
        let destination = destination.trim();
        if destination.is_empty() {
            self.set_status("Choose a copy destination first.");
            return;
        }

        let destination_root = PathBuf::from(destination);
        let copy_jobs: Vec<_> = self
            .assets
            .iter()
            .filter_map(|asset| {
                let source = asset.last_saved_path.as_ref()?.clone();
                let destination = destination_root
                    .join(self.folder_path_buf(asset.folder_id))
                    .join(asset.source_path.file_name().unwrap_or_default());
                Some((source, destination))
            })
            .collect();

        if copy_jobs.is_empty() {
            self.set_status("There are no built files to copy yet.");
            return;
        }

        let tx = self.job_tx.clone();
        self.pending_jobs += 1;
        self.set_status(format!("Copying {} built file(s)...", copy_jobs.len()));

        thread::spawn(move || {
            let result = copy_all_builds(copy_jobs).map_err(|error| format!("{}", error));
            let _ = tx.send(JobResult::CopyAllFinished(result));
        });
    }

    fn handle_setup_action(&mut self) {
        match self.setup_step {
            SetupStep::Welcome => {
                self.setup_step = SetupStep::ExternalTools;
                self.refresh_all();
            }
            SetupStep::ExternalTools => {
                if !self.setup_status.codewalker_source {
                    self.queue_download_codewalker();
                }
            }
            SetupStep::BuildHelper => {
                if !self.setup_status.cwassettool_binary {
                    self.queue_build_helper();
                }
            }
            SetupStep::Ready => {
                if !self.setup_status.setup_ready() {
                    self.set_status("Setup is not complete yet.");
                    return;
                }
                self.config.setup_complete = true;
                self.persist_config();
                self.widgets.stack.set_visible_child_name("browser");
                self.refresh_all();
            }
            SetupStep::SystemDependencies => {}
        }
    }

    fn queue_download_codewalker(&mut self) {
        let tx = self.job_tx.clone();
        let tool_paths = self.tool_paths.clone();
        self.pending_jobs += 1;
        self.set_status("Downloading CodeWalker into the app external folder...");

        thread::spawn(move || {
            let result = download_codewalker(&tool_paths).map_err(|error| format!("{}", error));
            let _ = tx.send(JobResult::DownloadCodeWalkerFinished(result));
        });
    }

    fn queue_build_helper(&mut self) {
        let tx = self.job_tx.clone();
        let tool_paths = self.tool_paths.clone();
        self.pending_jobs += 1;
        self.set_status("Building CwAssetTool helper...");

        thread::spawn(move || {
            let result = tool_paths
                .build_cwassettool()
                .map_err(|error| format!("{}", error));
            let _ = tx.send(JobResult::BuildHelperFinished(result));
        });
    }

    fn queue_check_external_updates(&mut self) {
        let tx = self.job_tx.clone();
        let tool_paths = self.tool_paths.clone();
        self.pending_jobs += 1;
        self.set_status("Checking CodeWalker for updates...");

        thread::spawn(move || {
            let result =
                check_codewalker_updates(&tool_paths).map_err(|error| format!("{}", error));
            let _ = tx.send(JobResult::UpdateCodeWalkerFinished(result));
        });
    }

    fn open_selected_build_folder(&mut self) {
        let Some(asset_index) = self.selected_asset else {
            self.set_status("Select a package first.");
            return;
        };
        let Some(asset) = self.assets.get(asset_index) else {
            return;
        };
        let Some(path) = asset.last_saved_path.as_ref() else {
            self.set_status("This package has not been built yet.");
            return;
        };
        let Some(directory) = path.parent() else {
            self.set_status("The last build directory could not be determined.");
            return;
        };

        if let Err(error) = open_directory(directory) {
            self.set_status(format!("Failed to open folder: {error:#}"));
        }
    }
}

fn sync_settings_panel_visibility(widgets: &AppWidgets, reveal: bool) {
    widgets.settings_backdrop.set_visible(reveal);
    widgets.settings_revealer.set_reveal_child(reveal);
}

thread_local! {
    static APP: RefCell<Option<Weak<RefCell<App>>>> = const { RefCell::new(None) };
}

fn connect_signals(app: &Rc<RefCell<App>>) {
    APP.with(|slot| {
        *slot.borrow_mut() = Some(Rc::downgrade(app));
    });

    let widgets = &app.borrow().widgets;

    {
        let settings_backdrop = widgets.settings_backdrop.clone();
        let settings_revealer = widgets.settings_revealer.clone();
        widgets.app_menu_button.connect_toggled(move |button| {
            let reveal = button.is_active();
            settings_backdrop.set_visible(reveal);
            settings_revealer.set_reveal_child(reveal);
        });
    }
    {
        let app_menu_button = widgets.app_menu_button.clone();
        widgets.settings_backdrop.connect_clicked(move |_| {
            app_menu_button.set_active(false);
        });
    }
    {
        let app = Rc::clone(app);
        widgets.rerun_setup_button.connect_clicked(move |_| {
            app.borrow_mut().rerun_setup_wizard();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.check_updates_button.connect_clicked(move |_| {
            app.borrow_mut().queue_check_external_updates();
        });
    }
    {
        let app = Rc::clone(app);
        widgets
            .theme_dropdown
            .connect_selected_notify(move |dropdown| {
                let theme = match dropdown.selected() {
                    1 => ThemePreference::Light,
                    2 => ThemePreference::Dark,
                    _ => ThemePreference::System,
                };
                let mut app = app.borrow_mut();
                app.config.theme = theme;
                apply_theme(app.config.theme);
                app.persist_config();
            });
    }
    {
        let app = Rc::clone(app);
        widgets.change_mod_folder_button.connect_clicked(move |_| {
            present_mod_folder_dialog(&app);
        });
    }
    {
        let app = Rc::clone(app);
        widgets.open_mod_folder_button.connect_clicked(move |_| {
            app.borrow_mut().open_mods_root_folder();
        });
    }
    {
        let app = Rc::clone(app);
        widgets
            .backup_before_save_check
            .connect_toggled(move |check| {
                let mut app = app.borrow_mut();
                let active = check.is_active();
                if app.config.backup_before_save != active {
                    app.config.backup_before_save = active;
                    app.persist_config();
                }
            });
    }
    {
        let app = Rc::clone(app);
        widgets.import_button.connect_clicked(move |_| {
            present_mod_folder_dialog(&app);
        });
    }
    {
        let app = Rc::clone(app);
        widgets.import_here_button.connect_clicked(move |_| {
            present_asset_file_dialog(&app);
        });
    }
    {
        let app = Rc::clone(app);
        widgets.create_folder_button.connect_clicked(move |_| {
            app.borrow_mut().create_folder();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.new_folder_entry.connect_activate(move |_| {
            app.borrow_mut().create_folder();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.move_here_button.connect_clicked(move |_| {
            app.borrow_mut().move_selected_asset_to_selected_folder();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.save_button.connect_clicked(move |_| {
            app.borrow_mut().queue_save_selected_asset();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.open_build_folder_button.connect_clicked(move |_| {
            app.borrow_mut().open_selected_build_folder();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.copy_all_button.connect_clicked(move |_| {
            app.borrow_mut().queue_copy_all();
        });
    }
    {
        let app_menu_button = widgets.app_menu_button.clone();
        widgets.settings_button.connect_clicked(move |_| {
            app_menu_button.set_active(true);
        });
    }
    {
        let app = Rc::clone(app);
        widgets
            .browse_copy_destination_button
            .connect_clicked(move |_| {
                present_copy_destination_dialog(&app);
            });
    }
    {
        let app = Rc::clone(app);
        widgets.copy_destination_entry.connect_changed(move |_| {
            let mut app = app.borrow_mut();
            app.config.copy_destination = app.widgets.copy_destination_entry.text().to_string();
            app.persist_config();
            app.refresh_header();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.edit_button.connect_clicked(move |_| {
            app.borrow_mut().open_editor_page();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.textures_back_button.connect_clicked(move |_| {
            app.borrow_mut().select_archive_parent();
        });
    }
    {
        let app = Rc::clone(app);
        widgets
            .textures_search_entry
            .connect_search_changed(move |entry| {
                app.borrow_mut()
                    .set_archive_search_query(entry.text().to_string());
            });
    }
    {
        let app = Rc::clone(app);
        widgets.back_button.connect_clicked(move |_| {
            let mut app = app.borrow_mut();
            if app.editor.is_some() {
                app.close_editor_page();
            } else if app.setup_required() {
                if let Some(previous) = app.setup_step.previous() {
                    app.setup_step = previous;
                    app.refresh_all();
                }
            }
        });
    }
    {
        let app = Rc::clone(app);
        widgets.editor_apply_button.connect_clicked(move |_| {
            app.borrow_mut().queue_apply_editor();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.setup_back_button.connect_clicked(move |_| {
            let mut app = app.borrow_mut();
            if let Some(previous) = app.setup_step.previous() {
                app.setup_step = previous;
                app.refresh_all();
            }
        });
    }
    {
        let app = Rc::clone(app);
        widgets.setup_next_button.connect_clicked(move |_| {
            let mut app = app.borrow_mut();
            if let Some(next) = app.setup_step.next() {
                app.setup_step = next;
                app.refresh_all();
            }
        });
    }
    {
        let app = Rc::clone(app);
        widgets.setup_action_button.connect_clicked(move |_| {
            app.borrow_mut().handle_setup_action();
        });
    }
}

fn attach_job_poller(app: &Rc<RefCell<App>>) {
    let app = Rc::clone(app);
    glib::timeout_add_local(Duration::from_millis(50), move || {
        app.borrow_mut().handle_job_results();
        app.borrow().refresh_header();
        glib::ControlFlow::Continue
    });
}

fn build_widgets(
    application: &adw::Application,
    tool_paths: &ToolPaths,
    config: &AppConfig,
) -> AppWidgets {
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("GTAV texture importer")
        .default_width(1680)
        .default_height(980)
        .build();

    let header_bar = adw::HeaderBar::new();
    let title_label = gtk::Label::new(Some("GTAV texture importer"));
    title_label.add_css_class("title-2");
    title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    header_bar.set_title_widget(Some(&title_label));

    let app_menu_button = gtk::ToggleButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Settings")
        .build();
    let rerun_setup_button = gtk::Button::with_label("Run Setup Wizard Again");
    let check_updates_button = gtk::Button::with_label("Check External Tool Updates");
    let theme_label = gtk::Label::new(Some("Theme"));
    theme_label.set_xalign(0.0);
    let theme_dropdown = gtk::DropDown::from_strings(&[
        ThemePreference::System.as_str(),
        ThemePreference::Light.as_str(),
        ThemePreference::Dark.as_str(),
    ]);
    theme_dropdown.set_selected(match config.theme {
        ThemePreference::System => 0,
        ThemePreference::Light => 1,
        ThemePreference::Dark => 2,
    });
    header_bar.pack_start(&app_menu_button);

    let back_button = gtk::Button::from_icon_name("go-previous-symbolic");
    back_button.set_tooltip_text(Some("Back to browser"));
    header_bar.pack_start(&back_button);

    let import_button = gtk::Button::from_icon_name("document-open-symbolic");
    import_button.set_tooltip_text(Some("Import files"));
    let save_button = gtk::Button::from_icon_name("document-save-symbolic");
    save_button.set_tooltip_text(Some("Save changes"));
    let open_build_folder_button = gtk::Button::with_label("Open Build Folder");
    let copy_all_button = gtk::Button::with_label("Copy All");
    let settings_button = gtk::Button::with_label("Settings");

    header_bar.pack_end(&copy_all_button);
    header_bar.pack_end(&save_button);
    header_bar.pack_end(&import_button);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);

    let setup_page = gtk::Box::new(gtk::Orientation::Vertical, 16);
    setup_page.set_margin_top(28);
    setup_page.set_margin_bottom(28);
    setup_page.set_margin_start(28);
    setup_page.set_margin_end(28);
    let setup_step_label = gtk::Label::new(None);
    setup_step_label.set_xalign(0.0);
    setup_step_label.add_css_class("caption");
    let setup_title_label = gtk::Label::new(None);
    setup_title_label.set_xalign(0.0);
    setup_title_label.add_css_class("title-2");
    let setup_body_label = gtk::Label::new(None);
    setup_body_label.set_xalign(0.0);
    setup_body_label.set_wrap(true);
    let setup_list_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    setup_list_box.set_hexpand(true);
    setup_list_box.set_vexpand(true);
    let setup_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&setup_list_box)
        .build();
    let setup_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let setup_back_button = gtk::Button::with_label("Back");
    let setup_next_button = gtk::Button::with_label("Next");
    let setup_action_button = gtk::Button::with_label("Action");
    setup_action_button.add_css_class("suggested-action");
    setup_actions.append(&setup_back_button);
    setup_actions.append(&setup_next_button);
    setup_actions.append(&setup_action_button);
    setup_page.append(&setup_step_label);
    setup_page.append(&setup_title_label);
    setup_page.append(&setup_body_label);
    setup_page.append(&setup_scroll);
    setup_page.append(&setup_actions);

    let status_label = gtk::Label::new(None);
    status_label.set_xalign(0.0);
    status_label.set_margin_top(8);
    status_label.set_margin_bottom(8);
    status_label.set_margin_start(12);
    status_label.set_margin_end(12);
    status_label.add_css_class("caption");
    let status_button = gtk::Button::new();
    status_button.add_css_class("flat");
    status_button.set_halign(gtk::Align::Fill);
    status_button.set_hexpand(true);
    status_button.set_tooltip_text(Some("Click to copy the current status"));
    status_button.set_child(Some(&status_label));

    let browser_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let browser_overlay = gtk::Overlay::new();
    browser_overlay.set_hexpand(true);
    browser_overlay.set_vexpand(true);
    let main_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    main_paned.set_wide_handle(true);
    main_paned.set_position(360);
    main_paned.set_shrink_start_child(false);
    main_paned.set_shrink_end_child(false);

    let right_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    right_paned.set_wide_handle(true);
    right_paned.set_position(420);
    right_paned.set_shrink_start_child(false);
    right_paned.set_shrink_end_child(false);

    let settings_revealer = gtk::Revealer::new();
    settings_revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
    settings_revealer.set_reveal_child(false);
    settings_revealer.set_halign(gtk::Align::Start);
    settings_revealer.set_valign(gtk::Align::Fill);
    settings_revealer.set_vexpand(true);
    let settings_backdrop = gtk::Button::new();
    settings_backdrop.set_visible(false);
    settings_backdrop.set_can_focus(false);
    settings_backdrop.set_halign(gtk::Align::Fill);
    settings_backdrop.set_valign(gtk::Align::Fill);
    settings_backdrop.set_hexpand(true);
    settings_backdrop.set_vexpand(true);
    settings_backdrop.add_css_class("flat");
    settings_backdrop.add_css_class("background");
    settings_backdrop.set_opacity(0.35);
    let settings_panel = build_panel_box("Settings");
    settings_panel.set_size_request(320, -1);
    let mod_folder_label = gtk::Label::new(Some("GTA V mods folder"));
    mod_folder_label.set_xalign(0.0);
    let mod_folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let mod_folder_path_label = gtk::Label::new(Some("Not configured"));
    mod_folder_path_label.set_xalign(0.0);
    mod_folder_path_label.set_hexpand(true);
    mod_folder_path_label.set_wrap(true);
    mod_folder_path_label.add_css_class("caption");
    let open_mod_folder_button = gtk::Button::from_icon_name("folder-open-symbolic");
    open_mod_folder_button.add_css_class("flat");
    open_mod_folder_button.set_tooltip_text(Some("Open GTA V mods folder"));
    open_mod_folder_button.set_valign(gtk::Align::Start);
    let change_mod_folder_button = gtk::Button::with_label("Choose Mods Folder");
    mod_folder_row.append(&mod_folder_path_label);
    mod_folder_row.append(&open_mod_folder_button);
    let backup_before_save_check =
        gtk::CheckButton::with_label("Create backup before saving changes");
    backup_before_save_check.set_active(config.backup_before_save);
    let destination_label = gtk::Label::new(Some("Copy all destination"));
    destination_label.set_xalign(0.0);
    let destination_controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let copy_destination_entry = gtk::Entry::new();
    copy_destination_entry.set_hexpand(true);
    let browse_copy_destination_button = gtk::Button::with_label("Browse");
    let copy_hint = gtk::Label::new(Some(
        "Optional: use Copy All to mirror built files somewhere else while preserving relative structure.",
    ));
    copy_hint.set_wrap(true);
    copy_hint.set_xalign(0.0);
    copy_hint.add_css_class("caption");
    destination_controls.append(&copy_destination_entry);
    destination_controls.append(&browse_copy_destination_button);
    settings_panel.append(&rerun_setup_button);
    settings_panel.append(&check_updates_button);
    settings_panel.append(&theme_label);
    settings_panel.append(&theme_dropdown);
    settings_panel.append(&mod_folder_label);
    settings_panel.append(&mod_folder_row);
    settings_panel.append(&change_mod_folder_button);
    settings_panel.append(&backup_before_save_check);
    settings_panel.append(&destination_label);
    settings_panel.append(&destination_controls);
    settings_panel.append(&copy_hint);
    let settings_shell = gtk::Frame::new(None);
    settings_shell.set_size_request(340, -1);
    settings_shell.set_hexpand(false);
    settings_shell.set_vexpand(true);
    settings_shell.add_css_class("background");
    settings_shell.add_css_class("view");
    settings_shell.set_child(Some(&settings_panel));
    settings_revealer.set_child(Some(&settings_shell));

    let packages_panel = build_panel_box("Mod Files");
    packages_panel.set_size_request(320, -1);
    let package_target_label = gtk::Label::new(Some("GTA V mods folder not configured"));
    package_target_label.set_xalign(0.0);
    package_target_label.set_wrap(true);
    package_target_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    let new_folder_entry = gtk::Entry::new();
    new_folder_entry.set_placeholder_text(Some("New folder name"));
    new_folder_entry.set_hexpand(true);
    let create_folder_button = gtk::Button::with_label("Create");
    let import_here_button = gtk::Button::with_label("Import Here");
    import_here_button.set_hexpand(true);
    let move_here_button = gtk::Button::with_label("Move Here");
    move_here_button.set_hexpand(true);

    let folder_controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    folder_controls.append(&new_folder_entry);
    folder_controls.append(&create_folder_button);

    let action_controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
    action_controls.append(&import_here_button);
    action_controls.append(&move_here_button);

    let package_list_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let package_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&package_list_box)
        .build();

    new_folder_entry.set_visible(false);
    create_folder_button.set_visible(false);
    import_here_button.set_visible(false);
    move_here_button.set_visible(false);
    folder_controls.set_visible(false);
    action_controls.set_visible(false);
    packages_panel.append(&package_target_label);
    packages_panel.append(&package_scroll);

    let textures_panel = build_panel_box("Textures");
    textures_panel.set_size_request(340, -1);
    let textures_title_label = gtk::Label::new(Some("Select a package from the left pane."));
    textures_title_label.set_xalign(0.0);
    textures_title_label.set_wrap(true);
    let textures_search_entry = gtk::SearchEntry::new();
    textures_search_entry.set_placeholder_text(Some("Search current section"));
    textures_search_entry.set_visible(false);
    let textures_nav_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let textures_back_button = gtk::Button::with_label("Up");
    let textures_path_label = gtk::Label::new(None);
    textures_path_label.set_xalign(0.0);
    textures_path_label.set_wrap(true);
    textures_path_label.set_hexpand(true);
    let textures_notice_label = gtk::Label::new(None);
    textures_notice_label.set_xalign(0.0);
    textures_notice_label.set_wrap(true);
    textures_notice_label.add_css_class("caption");
    textures_back_button.set_visible(false);
    textures_nav_row.append(&textures_back_button);
    textures_nav_row.append(&textures_path_label);
    let texture_list_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let textures_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&texture_list_box)
        .build();
    textures_panel.append(&textures_title_label);
    textures_panel.append(&textures_search_entry);
    textures_panel.append(&textures_nav_row);
    textures_panel.append(&textures_notice_label);
    textures_panel.append(&textures_scroll);

    let preview_panel = build_panel_box("Preview");
    preview_panel.set_size_request(360, -1);
    let preview_asset_label = gtk::Label::new(Some("Select a package first."));
    preview_asset_label.set_xalign(0.0);
    preview_asset_label.add_css_class("title-4");
    preview_asset_label.set_wrap(true);
    let preview_texture_label = gtk::Label::new(None);
    preview_texture_label.set_xalign(0.0);
    preview_texture_label.set_wrap(true);
    let preview_meta_label = gtk::Label::new(None);
    preview_meta_label.set_xalign(0.0);
    preview_meta_label.set_wrap(true);
    let preview_picture = gtk::Picture::new();
    preview_picture.set_can_shrink(true);
    preview_picture.set_content_fit(gtk::ContentFit::Contain);
    preview_picture.set_vexpand(true);
    let preview_notice_label = gtk::Label::new(None);
    preview_notice_label.set_xalign(0.0);
    preview_notice_label.add_css_class("caption");
    let edit_button = gtk::Button::with_label("Edit Texture");

    preview_panel.append(&preview_asset_label);
    preview_panel.append(&preview_texture_label);
    preview_panel.append(&preview_meta_label);
    preview_panel.append(&preview_notice_label);
    preview_panel.append(&preview_picture);
    preview_panel.append(&edit_button);

    main_paned.set_start_child(Some(&packages_panel));
    main_paned.set_end_child(Some(&right_paned));
    right_paned.set_start_child(Some(&textures_panel));
    right_paned.set_end_child(Some(&preview_panel));
    browser_overlay.set_child(Some(&main_paned));
    browser_overlay.add_overlay(&settings_backdrop);
    browser_overlay.add_overlay(&settings_revealer);
    browser_page.append(&browser_overlay);

    let editor_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    editor_page.set_margin_top(12);
    editor_page.set_margin_bottom(12);
    editor_page.set_margin_start(12);
    editor_page.set_margin_end(12);

    let editor_title_label = gtk::Label::new(Some("Editing"));
    editor_title_label.set_xalign(0.0);
    editor_title_label.add_css_class("title-3");
    let editor_meta_label = gtk::Label::new(None);
    editor_meta_label.set_xalign(0.0);
    let editor_notice_label = gtk::Label::new(None);
    editor_notice_label.set_xalign(0.0);
    editor_notice_label.add_css_class("caption");

    let editor_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    editor_paned.set_wide_handle(true);
    editor_paned.set_position(460);

    let original_panel = build_panel_box("Original");
    let editor_original_picture = gtk::Picture::new();
    editor_original_picture.set_can_shrink(true);
    editor_original_picture.set_content_fit(gtk::ContentFit::Contain);
    editor_original_picture.set_vexpand(true);
    original_panel.append(&editor_original_picture);

    let replacement_panel = build_panel_box("Replacement");
    let editor_canvas_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    editor_canvas_box.set_hexpand(true);
    editor_canvas_box.set_vexpand(true);
    replacement_panel.append(&editor_canvas_box);

    editor_paned.set_start_child(Some(&original_panel));
    editor_paned.set_end_child(Some(&replacement_panel));

    let editor_apply_button = gtk::Button::with_label("Apply Changes");
    editor_apply_button.add_css_class("suggested-action");

    editor_page.append(&editor_title_label);
    editor_page.append(&editor_meta_label);
    editor_page.append(&editor_notice_label);
    editor_page.append(&editor_paned);
    editor_page.append(&editor_apply_button);

    stack.add_named(&setup_page, Some("setup"));
    stack.add_named(&browser_page, Some("browser"));
    stack.add_named(&editor_page, Some("editor"));
    stack.set_visible_child_name("setup");

    let copy_destination_window: gtk::Window = window.clone().upcast();

    root.append(&stack);
    root.append(&status_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&root));
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar_view));
    window.set_content(Some(&toast_overlay));

    {
        let status_label = status_label.clone();
        let toast_overlay = toast_overlay.clone();
        status_button.connect_clicked(move |button| {
            let text = status_label.text().trim().to_owned();
            if text.is_empty() {
                return;
            }

            button.clipboard().set_text(&text);
            toast_overlay.add_toast(adw::Toast::new("Copied status text."));
        });
    }

    let copy_destination_default = tool_paths.builds_dir.display().to_string();
    copy_destination_entry.set_text(&copy_destination_default);

    AppWidgets {
        window,
        toast_overlay,
        app_menu_button,
        settings_backdrop,
        settings_revealer,
        rerun_setup_button,
        check_updates_button,
        theme_dropdown,
        mod_folder_path_label,
        open_mod_folder_button,
        change_mod_folder_button,
        backup_before_save_check,
        back_button,
        import_button,
        save_button,
        open_build_folder_button,
        copy_all_button,
        settings_button,
        status_label,
        stack,
        browser_main_paned: main_paned,
        packages_panel,
        package_target_label,
        new_folder_entry,
        create_folder_button,
        import_here_button,
        move_here_button,
        package_list_box,
        textures_title_label,
        textures_search_entry,
        textures_path_label,
        textures_back_button,
        textures_notice_label,
        texture_list_box,
        preview_asset_label,
        preview_texture_label,
        preview_meta_label,
        preview_picture,
        preview_notice_label,
        edit_button,
        editor_title_label,
        editor_meta_label,
        editor_original_picture,
        editor_canvas_box,
        editor_notice_label,
        editor_apply_button,
        copy_destination_window,
        copy_destination_entry,
        browse_copy_destination_button,
        setup_step_label,
        setup_title_label,
        setup_body_label,
        setup_list_box,
        setup_back_button,
        setup_next_button,
        setup_action_button,
    }
}

fn build_panel_box(title: &str) -> gtk::Box {
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 10);
    panel.set_margin_top(12);
    panel.set_margin_bottom(12);
    panel.set_margin_start(12);
    panel.set_margin_end(12);
    let title_label = gtk::Label::new(Some(title));
    title_label.set_xalign(0.0);
    title_label.add_css_class("title-4");
    panel.append(&title_label);
    panel
}

fn append_mod_folder_rows(app: &App, container: &gtk::Box, directory: &Path, depth: i32) {
    let Ok(entries) = read_mod_tree_entries(directory) else {
        let error_label = gtk::Label::new(Some("Failed to read this folder."));
        error_label.set_xalign(0.0);
        error_label.add_css_class("caption");
        error_label.set_margin_start((depth * 18).max(0));
        container.append(&error_label);
        return;
    };

    for path in entries {
        if path.is_dir() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            row.set_margin_start((depth * 18).max(0));
            row.set_margin_end(4);

            let expanded = app.mod_tree_expanded_paths.contains(&path);
            let toggle = gtk::Button::with_label(if expanded { "-" } else { "+" });
            toggle.add_css_class("flat");
            let folder_path = path.clone();
            toggle.connect_clicked(move |_| {
                with_app(|app| {
                    app.toggle_mod_tree_path(&folder_path);
                });
            });

            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let button = gtk::Button::with_label(&format!("[Folder] {}", label));
            button.set_halign(gtk::Align::Fill);
            button.set_hexpand(true);
            let folder_path = path.clone();
            button.connect_clicked(move |_| {
                with_app(|app| {
                    app.toggle_mod_tree_path(&folder_path);
                });
            });

            row.append(&toggle);
            row.append(&button);
            container.append(&row);

            if expanded {
                append_mod_folder_rows(app, container, &path, depth + 1);
            }
            continue;
        }

        let Some(kind) = AssetKind::from_path(&path) else {
            continue;
        };

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
        row.set_margin_start((depth * 18).max(0));
        row.set_margin_end(4);

        let asset_index = app.asset_index_for_source_path(&path);
        let asset = asset_index.and_then(|index| app.assets.get(index));
        let pending = app.pending_import_paths.contains(&path);
        let selected = asset_index.is_some_and(|index| app.selected_asset == Some(index))
            || app
                .selected_mod_path
                .as_ref()
                .is_some_and(|selected_path| selected_path == &path);

        let mut label = format!(
            "{} [{}]",
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            kind.label()
        );
        if pending {
            label.push_str(" (loading...)");
        } else if asset.is_some_and(|asset| asset.dirty) {
            label.push_str(" *");
        }

        let button = build_tree_button(&label, 0, selected, false);
        button.set_margin_start(0);
        let file_path = path.clone();
        button.connect_clicked(move |_| {
            with_app(|app| {
                app.open_mod_asset_path(file_path.clone());
            });
        });

        let save_button = gtk::Button::from_icon_name("document-save-symbolic");
        save_button.add_css_class("flat");
        save_button.set_tooltip_text(Some("Apply changes to this file"));
        save_button.set_sensitive(asset.is_some_and(|asset| asset.dirty));
        if asset.is_some_and(|asset| asset.dirty) {
            save_button.add_css_class("suggested-action");
        }
        let file_path = path.clone();
        save_button.connect_clicked(move |_| {
            with_app(|app| {
                if let Some(asset_index) = app.asset_index_for_source_path(&file_path) {
                    app.queue_save_asset_by_index(asset_index);
                } else {
                    app.set_status("Open this file before applying changes.");
                }
            });
        });

        row.append(&button);
        row.append(&save_button);
        container.append(&row);
    }
}

fn read_mod_tree_entries(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    let mut files = Vec::new();

    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            directories.push(path);
        } else if is_supported_mod_asset_path(&path) {
            files.push(path);
        }
    }

    directories.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });
    files.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });

    directories.extend(files);
    Ok(directories)
}

fn is_supported_mod_asset_path(path: &Path) -> bool {
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".bak"))
    {
        return false;
    }

    AssetKind::from_path(path).is_some()
}

fn build_tree_button(label: &str, depth: i32, selected: bool, folder: bool) -> gtk::Button {
    let prefix = if folder { "▾ " } else { "" };
    let button = gtk::Button::new();
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button.set_margin_start((depth * 18).max(0));
    button.set_margin_top(2);
    button.set_margin_bottom(2);
    button.set_tooltip_text(Some(label));

    let text = gtk::Label::new(Some(&format!("{}{}", prefix, label)));
    text.set_xalign(0.0);
    text.set_single_line_mode(true);
    text.set_ellipsize(gtk::pango::EllipsizeMode::End);
    button.set_child(Some(&text));

    if selected {
        button.add_css_class("suggested-action");
    }
    button
}

fn append_texture_rows(
    container: &gtk::Box,
    textures: &[TextureEntry],
    selected_texture: Option<usize>,
) {
    for (index, texture) in textures.iter().enumerate() {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.add_css_class("boxed-list-row");
        row.set_margin_top(4);
        row.set_margin_bottom(4);
        row.set_margin_start(4);
        row.set_margin_end(4);

        let button = gtk::Button::with_label(&format!(
            "{} ({})",
            texture.name,
            texture.width_height_label()
        ));
        button.set_halign(gtk::Align::Fill);
        button.set_hexpand(true);
        if selected_texture == Some(index) {
            button.add_css_class("suggested-action");
        }
        button.connect_clicked(move |_| {
            with_app(|app| {
                app.select_texture(index);
            });
        });

        let details = gtk::Label::new(Some(&format!(
            "{} | {} | {} mips",
            texture.file_name,
            texture.format.label(),
            texture.mips
        )));
        details.set_wrap(true);
        details.set_xalign(0.0);
        if texture.modified {
            details.add_css_class("accent");
        }

        row.append(&button);
        row.append(&details);
        container.append(&row);
    }
}

fn append_archive_rows(
    container: &gtk::Box,
    asset_index: usize,
    node: &RpfTreeNode,
    expanded_paths: &HashSet<String>,
    filter_query: Option<&str>,
    depth: i32,
) -> usize {
    let mut children = node.children.iter().collect::<Vec<_>>();
    children.sort_by_key(|child| {
        (
            match child.kind {
                RpfTreeNodeKind::Folder => 0,
                RpfTreeNodeKind::Package => 1,
                RpfTreeNodeKind::File => 2,
            },
            child.name.to_ascii_lowercase(),
        )
    });
    if let Some(filter_query) = filter_query {
        children.retain(|child| archive_name_matches_query(&child.name, filter_query));
    }

    let mut rendered_rows = 0;

    for child in children {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
        row.set_margin_start((depth * 18).max(0));
        row.set_margin_end(4);

        let is_branch = matches!(
            child.kind,
            RpfTreeNodeKind::Folder | RpfTreeNodeKind::Package
        );
        if is_branch {
            let toggle_label = if expanded_paths.contains(&child.path) {
                "-"
            } else {
                "+"
            };
            let toggle_button = gtk::Button::with_label(toggle_label);
            toggle_button.add_css_class("flat");
            let child_path = child.path.clone();
            toggle_button.connect_clicked(move |_| {
                with_app(|app| {
                    app.toggle_archive_path_expanded(asset_index, &child_path);
                });
            });
            row.append(&toggle_button);
        }

        let label_prefix = match child.kind {
            RpfTreeNodeKind::Folder => "[Folder] ",
            RpfTreeNodeKind::Package => "[Package] ",
            RpfTreeNodeKind::File => "[File] ",
        };
        let button = gtk::Button::with_label(&format!("{}{}", label_prefix, child.name));
        button.set_halign(gtk::Align::Fill);
        button.set_hexpand(true);
        let child_path = child.path.clone();
        match child.kind {
            RpfTreeNodeKind::Folder | RpfTreeNodeKind::Package => {
                button.connect_clicked(move |_| {
                    with_app(|app| {
                        app.browse_archive_path(asset_index, child_path.clone());
                    });
                });
            }
            RpfTreeNodeKind::File => {
                button.connect_clicked(move |_| {
                    with_app(|app| {
                        app.open_archive_file(asset_index, child_path.clone());
                    });
                });
            }
        }
        row.append(&button);
        container.append(&row);
        rendered_rows += 1;

        if is_branch && expanded_paths.contains(&child.path) {
            rendered_rows += append_archive_rows(
                container,
                asset_index,
                child,
                expanded_paths,
                filter_query,
                depth + 1,
            );
        }
    }

    rendered_rows
}

fn archive_name_matches_query(name: &str, filter_query: &str) -> bool {
    name.to_ascii_lowercase().contains(filter_query)
}

fn find_archive_parent_path(
    node: &RpfTreeNode,
    path: &str,
    parent_path: &mut Option<String>,
) -> bool {
    for child in &node.children {
        if child.path == path {
            *parent_path = Some(node.path.clone());
            return true;
        }
        if find_archive_parent_path(child, path, parent_path) {
            return true;
        }
    }
    false
}

fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn build_section_widget(node: &SectionNode, parent_axis: Option<SplitAxis>) -> gtk::Widget {
    match node {
        SectionNode::Leaf(leaf) => build_leaf_section_widget(leaf, parent_axis).upcast(),
        SectionNode::Group(group) => {
            let orientation = match group.axis {
                SplitAxis::Horizontal => gtk::Orientation::Vertical,
                SplitAxis::Vertical => gtk::Orientation::Horizontal,
            };

            let outer = gtk::Overlay::new();
            outer.set_hexpand(true);
            outer.set_vexpand(true);
            outer.set_margin_top(4);
            outer.set_margin_bottom(4);
            outer.set_margin_start(4);
            outer.set_margin_end(4);

            let frame = gtk::Frame::new(None);
            frame.set_hexpand(true);
            frame.set_vexpand(true);
            frame.add_css_class("card");

            let container = gtk::Box::new(orientation, 10);
            container.set_hexpand(true);
            container.set_vexpand(true);
            container.set_margin_top(12);
            container.set_margin_bottom(52);
            container.set_margin_start(12);
            container.set_margin_end(12);

            for child in &group.children {
                let child_widget = build_section_widget(child, Some(group.axis));
                if matches!(group.axis, SplitAxis::Horizontal) {
                    child_widget.set_vexpand(true);
                } else {
                    child_widget.set_hexpand(true);
                }
                container.append(&child_widget);
            }

            frame.set_child(Some(&container));
            outer.set_child(Some(&frame));
            outer.add_overlay(&add_section_controls(group.id, parent_axis));
            outer.upcast()
        }
    }
}

fn build_leaf_section_widget(leaf: &LeafSection, parent_axis: Option<SplitAxis>) -> gtk::Widget {
    let outer = gtk::Overlay::new();
    outer.set_hexpand(true);
    outer.set_vexpand(true);
    outer.set_margin_top(4);
    outer.set_margin_bottom(4);
    outer.set_margin_start(4);
    outer.set_margin_end(4);

    let frame = gtk::Frame::new(None);
    frame.set_hexpand(true);
    frame.set_vexpand(true);
    frame.add_css_class("card");

    let overlay = gtk::Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);

    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    if let Some(texture) = &leaf.preview_texture {
        picture.set_paintable(Some(texture));
    }

    let placeholder_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    placeholder_box.set_hexpand(true);
    placeholder_box.set_vexpand(true);
    placeholder_box.set_valign(gtk::Align::Center);
    placeholder_box.set_halign(gtk::Align::Center);
    let plus_label = gtk::Label::new(Some("+"));
    plus_label.add_css_class("title-1");
    let hint_label = gtk::Label::new(Some("Pick image"));
    hint_label.add_css_class("caption");
    if leaf.preview_texture.is_none() {
        placeholder_box.append(&plus_label);
        placeholder_box.append(&hint_label);
    }

    let base = gtk::Overlay::new();
    base.set_child(Some(&picture));
    base.set_margin_top(12);
    base.set_margin_bottom(56);
    base.set_margin_start(12);
    base.set_margin_end(12);
    if leaf.preview_texture.is_none() {
        base.add_overlay(&placeholder_box);
    }
    overlay.set_child(Some(&base));

    let add_image_button = gtk::Button::from_icon_name("list-add-symbolic");
    add_image_button.set_tooltip_text(Some("Pick or replace image"));
    add_image_button.set_halign(gtk::Align::End);
    add_image_button.set_valign(gtk::Align::Start);
    add_image_button.set_margin_top(14);
    add_image_button.set_margin_end(14);
    overlay.add_overlay(&add_image_button);

    let leaf_id = leaf.id;
    add_image_button.connect_clicked(move |_| {
        with_app_ref(|app_ref| {
            present_image_file_dialog(&app_ref, leaf_id);
        });
    });

    frame.set_child(Some(&overlay));
    outer.set_child(Some(&frame));
    outer.add_overlay(&add_section_controls(leaf.id, parent_axis));
    outer.upcast()
}

fn add_section_controls(section_id: u64, parent_axis: Option<SplitAxis>) -> gtk::Box {
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.set_margin_start(10);
    controls.set_margin_end(10);
    controls.set_margin_bottom(10);
    controls.set_halign(gtk::Align::Center);
    controls.set_valign(gtk::Align::End);
    let add_row_button = build_editor_icon_button("split-row-add-symbolic.svg", "Add row");
    let add_column_button = build_editor_icon_button("split-col-add-symbolic.svg", "Add column");
    let remove_row_button = build_editor_icon_button("split-row-remove-symbolic.svg", "Remove row");
    let remove_column_button =
        build_editor_icon_button("split-col-remove-symbolic.svg", "Remove column");
    remove_row_button.set_sensitive(parent_axis == Some(SplitAxis::Horizontal));
    remove_column_button.set_sensitive(parent_axis == Some(SplitAxis::Vertical));
    controls.append(&add_row_button);
    controls.append(&add_column_button);
    controls.append(&remove_row_button);
    controls.append(&remove_column_button);

    add_row_button.connect_clicked(move |_| {
        with_app(|app| {
            if let Some(editor) = app.editor.as_mut() {
                editor.add_section(section_id, SplitAxis::Horizontal);
                app.refresh_editor_page();
                app.refresh_header();
            }
        });
    });

    add_column_button.connect_clicked(move |_| {
        with_app(|app| {
            if let Some(editor) = app.editor.as_mut() {
                editor.add_section(section_id, SplitAxis::Vertical);
                app.refresh_editor_page();
                app.refresh_header();
            }
        });
    });

    remove_row_button.connect_clicked(move |_| {
        with_app(|app| {
            if let Some(editor) = app.editor.as_mut() {
                editor.remove_section(section_id, SplitAxis::Horizontal);
                app.refresh_editor_page();
                app.refresh_header();
            }
        });
    });

    remove_column_button.connect_clicked(move |_| {
        with_app(|app| {
            if let Some(editor) = app.editor.as_mut() {
                editor.remove_section(section_id, SplitAxis::Vertical);
                app.refresh_editor_page();
                app.refresh_header();
            }
        });
    });

    controls
}

fn build_editor_icon_button(icon_file: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("circular");
    button.set_tooltip_text(Some(tooltip));
    button.set_size_request(28, 28);

    let image = gtk::Image::from_file(asset_icon_path(icon_file));
    image.set_pixel_size(16);
    button.set_child(Some(&image));

    button
}

fn asset_icon_path(file_name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(file_name)
}

fn present_asset_file_dialog(app: &Rc<RefCell<App>>) {
    let app_borrow = app.borrow();
    let dialog = gtk::FileDialog::builder()
        .title("Import GTA V asset files")
        .modal(true)
        .build();

    let filters = gio::ListStore::new::<gtk::FileFilter>();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("GTA V assets"));
    filter.add_suffix("ydr");
    filter.add_suffix("yft");
    filter.add_suffix("ytd");
    filter.add_suffix("rpf");
    filters.append(&filter);
    dialog.set_filters(Some(&filters));

    if let Some(dir) = app_borrow
        .last_asset_dir
        .as_ref()
        .filter(|dir| dir.exists())
    {
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
    }

    let parent = app_borrow.widgets.window.clone();
    drop(app_borrow);

    let app_ref = Rc::clone(app);
    dialog.open_multiple(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if let Ok(files) = result {
            let mut paths = Vec::new();
            for index in 0..files.n_items() {
                if let Some(file) = files.item(index).and_downcast::<gio::File>() {
                    if let Some(path) = file.path() {
                        paths.push(path);
                    }
                }
            }

            if let Some(first_path) = paths.first() {
                let mut app = app_ref.borrow_mut();
                app.last_asset_dir = first_path.parent().map(Path::to_path_buf);
                app.config.last_asset_dir = app.last_asset_dir.clone();
                app.persist_config();
            }

            app_ref.borrow_mut().queue_import_files(paths);
        }
    });
}

fn present_image_file_dialog(app: &Rc<RefCell<App>>, leaf_id: u64) {
    let app_borrow = app.borrow();
    let dialog = gtk::FileDialog::builder()
        .title("Choose replacement image")
        .modal(true)
        .build();

    let filters = gio::ListStore::new::<gtk::FileFilter>();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Images"));
    for suffix in ["png", "jpg", "jpeg", "bmp", "webp"] {
        filter.add_suffix(suffix);
    }
    filters.append(&filter);
    dialog.set_filters(Some(&filters));

    if let Some(dir) = app_borrow
        .last_image_dir
        .as_ref()
        .filter(|dir| dir.exists())
    {
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
    }

    let parent = app_borrow.widgets.window.clone();
    drop(app_borrow);

    let app_ref = Rc::clone(app);
    dialog.open(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result {
            if let Some(path) = file.path() {
                match texture_from_path(&path) {
                    Ok(texture) => {
                        let mut app = app_ref.borrow_mut();
                        app.last_image_dir = path.parent().map(Path::to_path_buf);
                        app.config.last_image_dir = app.last_image_dir.clone();
                        app.persist_config();
                        if let Some(editor) = app.editor.as_mut() {
                            editor.set_leaf_image(leaf_id, path, texture);
                            app.refresh_editor_page();
                            app.refresh_header();
                        }
                    }
                    Err(error) => {
                        app_ref
                            .borrow_mut()
                            .set_status(format!("Failed to load image: {error:#}"));
                    }
                }
            }
        }
    });
}

fn present_copy_destination_dialog(app: &Rc<RefCell<App>>) {
    let app_borrow = app.borrow();
    let dialog = gtk::FileDialog::builder()
        .title("Choose destination folder")
        .modal(true)
        .build();

    if let Some(dir) = app_borrow.last_copy_dir.as_ref().filter(|dir| dir.exists()) {
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
    }

    let parent = app_borrow.widgets.copy_destination_window.clone();
    drop(app_borrow);

    let app_ref = Rc::clone(app);
    dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result {
            if let Some(path) = file.path() {
                let mut app = app_ref.borrow_mut();
                app.last_copy_dir = Some(path.clone());
                app.config.last_copy_dir = app.last_copy_dir.clone();
                app.widgets
                    .copy_destination_entry
                    .set_text(&path.display().to_string());
                app.config.copy_destination = path.display().to_string();
                app.persist_config();
                app.refresh_header();
            }
        }
    });
}

fn present_mod_folder_dialog(app: &Rc<RefCell<App>>) {
    let app_borrow = app.borrow();
    let dialog = gtk::FileDialog::builder()
        .title("Choose GTA V mods folder")
        .modal(true)
        .build();

    if let Some(dir) = app_borrow
        .config
        .mod_folder_path
        .as_ref()
        .filter(|dir| dir.exists())
    {
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
    } else if let Some(dir) = app_borrow
        .last_asset_dir
        .as_ref()
        .filter(|dir| dir.exists())
    {
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
    }

    let parent = app_borrow.widgets.window.clone();
    drop(app_borrow);

    let app_ref = Rc::clone(app);
    dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result {
            if let Some(path) = file.path() {
                let mut app = app_ref.borrow_mut();
                app.last_asset_dir = Some(path.clone());
                app.config.last_asset_dir = app.last_asset_dir.clone();
                app.set_mod_folder_path(path);
            }
        }
    });
}

fn texture_from_path(path: &Path) -> Result<gdk::Texture> {
    gdk::Texture::from_file(&gio::File::for_path(path)).map_err(|error| anyhow!(error.to_string()))
}

fn import_asset_draft(
    tool_paths: &ToolPaths,
    asset_path: &Path,
    folder_id: u64,
) -> Result<ImportedAssetDraft> {
    let kind = AssetKind::from_path(asset_path)
        .ok_or_else(|| anyhow!("Only .ydr, .yft, .ytd, and .rpf files are supported for import"))?;

    if kind == AssetKind::Rpf {
        let archive_tree = list_rpf_tree(tool_paths, asset_path)?;
        return Ok(ImportedAssetDraft {
            id: format!(
                "{}_{}",
                sanitize_for_path(&asset_path.file_stem().unwrap_or_default().to_string_lossy()),
                unix_timestamp_ms()
            ),
            source_path: asset_path.to_path_buf(),
            kind,
            folder_id,
            xml_path: None,
            textures: Vec::new(),
            archive_tree: Some(archive_tree),
        });
    }

    let asset_name = asset_path
        .file_name()
        .context("Asset path does not contain a file name")?
        .to_string_lossy()
        .into_owned();

    let session_id = format!(
        "{}_{}",
        sanitize_for_path(&asset_path.file_stem().unwrap_or_default().to_string_lossy()),
        unix_timestamp_ms()
    );

    let session_dir = tool_paths.workspace_dir.join("imports").join(&session_id);
    let template_dir = session_dir.join("template");
    let working_dir = session_dir.join("current");
    let preview_dir = session_dir.join("previews");
    fs::create_dir_all(&session_dir)?;
    fs::create_dir_all(&preview_dir)?;

    let output = Command::new(&tool_paths.cwassettool_bin)
        .arg("export")
        .arg(asset_path)
        .arg(&template_dir)
        .output()
        .with_context(|| format!("Failed to export {}", asset_path.display()))?;
    ensure_success("cwassettool export", output)?;

    copy_dir_recursive(&template_dir, &working_dir)?;
    let xml_path = working_dir.join(format!("{}.xml", asset_name));
    let textures = parse_textures_from_xml(&xml_path, &working_dir, &preview_dir, true)?;

    Ok(ImportedAssetDraft {
        id: session_id,
        source_path: asset_path.to_path_buf(),
        kind,
        folder_id,
        xml_path: Some(xml_path),
        textures,
        archive_tree: None,
    })
}

fn parse_textures_from_xml(
    xml_path: &Path,
    working_dir: &Path,
    preview_dir: &Path,
    require_textures: bool,
) -> Result<Vec<TextureEntryDraft>> {
    let xml = fs::read_to_string(xml_path)
        .with_context(|| format!("Failed to read {}", xml_path.display()))?;
    let document = Document::parse(&xml).context("Failed to parse exported XML")?;
    let mut textures = Vec::new();

    for (index, node) in document
        .descendants()
        .filter(|node| node.has_tag_name("Item"))
        .enumerate()
    {
        let Some(file_name) = child_text(node, "FileName") else {
            continue;
        };
        if !file_name.to_ascii_lowercase().ends_with(".dds") {
            continue;
        }

        let Some(name) = child_text(node, "Name") else {
            continue;
        };
        let Some(width) = child_value_u32(node, "Width") else {
            continue;
        };
        let Some(height) = child_value_u32(node, "Height") else {
            continue;
        };
        let Some(mips) = child_value_u32(node, "MipLevels") else {
            continue;
        };
        let Some(format_label) = child_text(node, "Format") else {
            continue;
        };

        let usage = child_text(node, "Usage").unwrap_or_else(|| "UNKNOWN".to_owned());
        let dds_path = working_dir.join(&file_name);
        let preview_png_path = preview_dir.join(format!(
            "{}_{}.png",
            index,
            sanitize_for_path(file_name.trim_end_matches(".dds"))
        ));

        textures.push(TextureEntryDraft {
            name,
            file_name,
            width,
            height,
            mips,
            format: TextureFormat::from_label(&format_label),
            usage,
            dds_path,
            preview_png_path,
        });
    }

    if require_textures && textures.is_empty() {
        bail!("No DDS textures were found in {}", xml_path.display());
    }

    Ok(textures)
}

fn list_rpf_tree(tool_paths: &ToolPaths, rpf_path: &Path) -> Result<RpfTreeNode> {
    let output = Command::new(&tool_paths.cwassettool_bin)
        .arg("list-rpf")
        .arg(rpf_path)
        .output()
        .with_context(|| format!("Failed to inspect {}", rpf_path.display()))?;

    if !output.status.success() {
        ensure_success("cwassettool list-rpf", output)?;
        unreachable!();
    }

    let stdout = String::from_utf8(output.stdout).context("Invalid UTF-8 from cwassettool")?;
    serde_json::from_str(&stdout).context("Failed to parse RPF tree JSON")
}

fn export_archive_entry_draft(
    tool_paths: &ToolPaths,
    rpf_path: &Path,
    entry_path: &str,
) -> Result<ImportedArchiveEntryDraft> {
    let session_id = format!(
        "{}_{}",
        sanitize_for_path(entry_path.split('\\').last().unwrap_or(entry_path)),
        unix_timestamp_ms()
    );
    let session_dir = tool_paths.workspace_dir.join("imports").join(&session_id);
    let template_dir = session_dir.join("template");
    let working_dir = session_dir.join("current");
    let preview_dir = session_dir.join("previews");
    fs::create_dir_all(&session_dir)?;
    fs::create_dir_all(&preview_dir)?;

    let output = Command::new(&tool_paths.cwassettool_bin)
        .arg("export-rpf-entry")
        .arg(rpf_path)
        .arg(entry_path)
        .arg(&template_dir)
        .output()
        .with_context(|| {
            format!(
                "Failed to export {} from {}",
                entry_path,
                rpf_path.display()
            )
        })?;
    ensure_success("cwassettool export-rpf-entry", output)?;

    let entry_name = entry_path
        .split('\\')
        .next_back()
        .context("Archive entry path did not contain a file name")?
        .to_owned();
    copy_dir_recursive(&template_dir, &working_dir)?;
    let xml_path = working_dir.join(format!("{}.xml", entry_name));
    let textures = parse_textures_from_xml(&xml_path, &working_dir, &preview_dir, false)?;

    Ok(ImportedArchiveEntryDraft {
        entry_path: entry_path.to_owned(),
        title: entry_name,
        xml_path,
        textures,
    })
}

fn child_text(node: roxmltree::Node<'_, '_>, tag_name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == tag_name)
        .and_then(|child| child.text())
        .map(|text| text.trim().to_owned())
}

fn child_value_u32(node: roxmltree::Node<'_, '_>, tag_name: &str) -> Option<u32> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == tag_name)
        .and_then(|child| child.attribute("value"))
        .and_then(|text| text.parse().ok())
}

fn generate_preview_png(dds_path: &Path, preview_png_path: &Path) -> Result<()> {
    if let Some(parent) = preview_png_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let output = Command::new("magick")
        .arg(dds_path)
        .arg(format!("png32:{}", preview_png_path.display()))
        .output()
        .with_context(|| format!("Failed to generate preview for {}", dds_path.display()))?;
    ensure_success("magick preview", output)
}

fn apply_texture_job(
    dds_path: &Path,
    preview_png_path: &Path,
    format: &TextureFormat,
    mip_levels: u32,
    target_width: u32,
    target_height: u32,
    cells: Vec<CompositionCell>,
) -> Result<()> {
    let composed =
        compose_final_image(target_width, target_height, format.supports_alpha(), &cells)?;
    let temp_png = dds_path.with_extension("tmp.png");
    composed.save(&temp_png)?;
    convert_png_to_dds(
        &temp_png,
        dds_path,
        format,
        mip_levels,
        target_width,
        target_height,
    )?;
    generate_preview_png(dds_path, preview_png_path)?;
    let _ = fs::remove_file(temp_png);
    Ok(())
}

fn compose_final_image(
    target_width: u32,
    target_height: u32,
    keep_alpha: bool,
    cells: &[CompositionCell],
) -> Result<DynamicImage> {
    let background = if keep_alpha {
        Rgba([0, 0, 0, 0])
    } else {
        Rgba([255, 255, 255, 255])
    };
    let mut canvas: RgbaImage = ImageBuffer::from_pixel(target_width, target_height, background);

    for cell in cells {
        let source = image::open(&cell.image_path)
            .with_context(|| format!("Failed to load {}", cell.image_path.display()))?;
        let fitted =
            prepare_cell_image(&source, cell.rect.width, cell.rect.height, cell.keep_alpha);
        image::imageops::replace(&mut canvas, &fitted, cell.rect.x as i64, cell.rect.y as i64);
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}

fn prepare_cell_image(
    source: &DynamicImage,
    target_width: u32,
    target_height: u32,
    keep_alpha: bool,
) -> RgbaImage {
    let mut rgba = source.to_rgba8();
    if !keep_alpha {
        flatten_alpha_on_white(&mut rgba);
    }

    let width = rgba.width();
    let height = rgba.height();
    let scale =
        ((target_width as f32 / width as f32).max(target_height as f32 / height as f32)).max(0.01);
    let resized_width = ((width as f32 * scale).round() as u32).max(target_width);
    let resized_height = ((height as f32 * scale).round() as u32).max(target_height);
    let resized =
        image::imageops::resize(&rgba, resized_width, resized_height, FilterType::Lanczos3);
    let x = (resized_width.saturating_sub(target_width)) / 2;
    let y = (resized_height.saturating_sub(target_height)) / 2;
    image::imageops::crop_imm(&resized, x, y, target_width, target_height).to_image()
}

fn flatten_alpha_on_white(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = pixel[3] as f32 / 255.0;
        let red = (pixel[0] as f32 * alpha + 255.0 * (1.0 - alpha)).round() as u8;
        let green = (pixel[1] as f32 * alpha + 255.0 * (1.0 - alpha)).round() as u8;
        let blue = (pixel[2] as f32 * alpha + 255.0 * (1.0 - alpha)).round() as u8;
        *pixel = Rgba([red, green, blue, 255]);
    }
}

fn convert_png_to_dds(
    png_path: &Path,
    dds_path: &Path,
    format: &TextureFormat,
    mip_levels: u32,
    target_width: u32,
    target_height: u32,
) -> Result<()> {
    let compression = format
        .magick_compression()
        .ok_or_else(|| anyhow!("Unsupported output format {}", format.label()))?;

    let mut command = Command::new("magick");
    command.arg(png_path);
    if !format.supports_alpha() {
        command.arg("-background").arg("white");
        command.arg("-alpha").arg("remove");
        command.arg("-alpha").arg("off");
    }
    command.arg("-colorspace").arg("sRGB");
    command.arg("-type").arg("TrueColor");
    command
        .arg("-resize")
        .arg(format!("{}x{}!", target_width, target_height));
    command
        .arg("-define")
        .arg(format!("dds:compression={compression}"));
    command
        .arg("-define")
        .arg(format!("dds:mipmaps={mip_levels}"));
    command.arg(format!("DDS:{}", dds_path.display()));

    let output = command
        .output()
        .with_context(|| format!("Failed to convert {} to DDS", png_path.display()))?;
    ensure_success("magick DDS convert", output)
}

fn save_asset_build_job(
    tool_paths: &ToolPaths,
    xml_path: &Path,
    output_path: &Path,
) -> Result<PathBuf> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let output = Command::new(&tool_paths.cwassettool_bin)
        .arg("import")
        .arg(xml_path)
        .arg(output_path)
        .output()
        .with_context(|| format!("Failed to build {}", output_path.display()))?;
    ensure_success("cwassettool import", output)?;
    Ok(output_path.to_path_buf())
}

fn save_rpf_build_job(
    tool_paths: &ToolPaths,
    source_path: &Path,
    output_path: &Path,
    changes: Vec<RpfBuildChange>,
) -> Result<PathBuf> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let manifest_path = tool_paths
        .workspace_dir
        .join("imports")
        .join(format!("rpf_build_{}.json", unix_timestamp_ms()));
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&RpfBuildManifest { changes })?,
    )?;

    let output = Command::new(&tool_paths.cwassettool_bin)
        .arg("build-rpf")
        .arg(source_path)
        .arg(output_path)
        .arg(&manifest_path)
        .output()
        .with_context(|| format!("Failed to build {}", output_path.display()))?;
    let result = ensure_success("cwassettool build-rpf", output);
    let _ = fs::remove_file(&manifest_path);
    result?;
    Ok(output_path.to_path_buf())
}

fn save_asset_in_place_job(
    tool_paths: &ToolPaths,
    source_path: &Path,
    xml_path: Option<&Path>,
    changes: Vec<RpfBuildChange>,
    backup_before_save: bool,
) -> Result<PathBuf> {
    let temp_output_path = temporary_save_path(source_path);
    if let Some(parent) = temp_output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if backup_before_save && source_path.is_file() {
        let backup_path = backup_path_for(source_path)?;
        fs::copy(source_path, &backup_path).with_context(|| {
            format!(
                "Failed to create backup {} from {}",
                backup_path.display(),
                source_path.display()
            )
        })?;
    }

    if let Some(xml_path) = xml_path {
        save_asset_build_job(tool_paths, xml_path, &temp_output_path)?;
    } else {
        save_rpf_build_job(tool_paths, source_path, &temp_output_path, changes)?;
    }

    replace_file_from_temp(&temp_output_path, source_path)?;
    Ok(source_path.to_path_buf())
}

fn backup_path_for(source_path: &Path) -> Result<PathBuf> {
    let file_name = source_path
        .file_name()
        .context("Asset path does not contain a file name")?
        .to_string_lossy();
    Ok(source_path.with_file_name(format!("{}.bak", file_name)))
}

fn temporary_save_path(source_path: &Path) -> PathBuf {
    let file_name = source_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("save_{}", unix_timestamp_ms()));
    let temp_dir_name = format!(".gtav_texture_importer_tmp_{}", unix_timestamp_ms());
    source_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(temp_dir_name)
        .join(file_name)
}

fn replace_file_from_temp(temp_path: &Path, destination_path: &Path) -> Result<()> {
    if let Err(error) = fs::rename(temp_path, destination_path) {
        fs::copy(temp_path, destination_path).with_context(|| {
            format!(
                "Failed to replace {} with {} after rename error: {}",
                destination_path.display(),
                temp_path.display(),
                error
            )
        })?;
        let _ = fs::remove_file(temp_path);
    }

    if let Some(parent) = temp_path.parent() {
        let _ = fs::remove_dir(parent);
    }

    Ok(())
}

fn copy_all_builds(copy_jobs: Vec<(PathBuf, PathBuf)>) -> Result<usize> {
    let mut copied = 0usize;
    for (source, destination) in copy_jobs {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "Failed to copy {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        copied += 1;
    }
    Ok(copied)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    if !target.exists() {
        fs::create_dir_all(target)?;
    }

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &target_path)?;
        }
    }

    Ok(())
}

fn open_directory(path: &Path) -> Result<()> {
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .with_context(|| format!("Failed to open {}", path.display()))?;
    Ok(())
}

fn run_command<const N: usize>(program: &str, args: [&str; N]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("Failed to start {program}"))
}

fn ensure_success(label: &str, output: Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !stderr.is_empty() {
        bail!("{label} failed: {stderr}");
    }
    if !stdout.is_empty() {
        bail!("{label} failed: {stdout}");
    }
    bail!("{label} failed with exit status {}", output.status)
}

fn apply_theme(preference: ThemePreference) {
    let style_manager = adw::StyleManager::default();
    style_manager.set_color_scheme(preference.color_scheme());
}

fn setup_status_row(title: &str, ok: bool, detail: &str) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
    row.add_css_class("card");
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    row.set_margin_start(4);
    row.set_margin_end(4);

    let title_label = gtk::Label::new(Some(&format!(
        "{}: {}",
        title,
        if ok { "OK" } else { "Missing" }
    )));
    title_label.set_xalign(0.0);
    if ok {
        title_label.add_css_class("success");
    } else {
        title_label.add_css_class("error");
    }

    let detail_label = gtk::Label::new(Some(detail));
    detail_label.set_xalign(0.0);
    detail_label.set_wrap(true);
    detail_label.add_css_class("caption");

    row.append(&title_label);
    row.append(&detail_label);
    row.upcast()
}

fn setup_info_row(title: &str, detail: &str) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
    row.add_css_class("card");
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    row.set_margin_start(4);
    row.set_margin_end(4);

    let title_label = gtk::Label::new(Some(title));
    title_label.set_xalign(0.0);
    let detail_label = gtk::Label::new(Some(detail));
    detail_label.set_xalign(0.0);
    detail_label.set_wrap(true);
    detail_label.add_css_class("caption");

    row.append(&title_label);
    row.append(&detail_label);
    row.upcast()
}

fn download_codewalker(tool_paths: &ToolPaths) -> Result<()> {
    tool_paths.ensure_git()?;
    fs::create_dir_all(&tool_paths.external_dir)?;

    if tool_paths.codewalker_present() {
        return Ok(());
    }

    if tool_paths.codewalker_dir.exists() {
        fs::remove_dir_all(&tool_paths.codewalker_dir)?;
    }

    let output = Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg(tool_paths.codewalker_clone_url())
        .arg(&tool_paths.codewalker_dir)
        .output()
        .context("Failed to start git clone for CodeWalker")?;
    ensure_success("git clone CodeWalker", output)
}

fn check_codewalker_updates(tool_paths: &ToolPaths) -> Result<String> {
    tool_paths.ensure_git()?;

    if !tool_paths.codewalker_present() {
        bail!("CodeWalker source is missing. Run the setup wizard first.");
    }

    let git_dir = tool_paths.codewalker_dir.join(".git");
    if !git_dir.exists() {
        return Ok(
            "CodeWalker is present but not a git checkout, so automatic updates are unavailable."
                .to_owned(),
        );
    }

    let fetch = Command::new("git")
        .arg("-C")
        .arg(&tool_paths.codewalker_dir)
        .arg("fetch")
        .arg("origin")
        .arg("master")
        .output()
        .context("Failed to fetch CodeWalker updates")?;
    ensure_success("git fetch CodeWalker", fetch)?;

    let local = command_stdout(
        "git",
        &[
            "-C",
            tool_paths.codewalker_dir.to_string_lossy().as_ref(),
            "rev-parse",
            "HEAD",
        ],
    )?;
    let remote = command_stdout(
        "git",
        &[
            "-C",
            tool_paths.codewalker_dir.to_string_lossy().as_ref(),
            "rev-parse",
            "origin/master",
        ],
    )?;

    if local == remote {
        return Ok("CodeWalker is already up to date.".to_owned());
    }

    let pull = Command::new("git")
        .arg("-C")
        .arg(&tool_paths.codewalker_dir)
        .arg("pull")
        .arg("--ff-only")
        .arg("origin")
        .arg("master")
        .output()
        .context("Failed to update CodeWalker")?;
    ensure_success("git pull CodeWalker", pull)?;
    tool_paths.build_cwassettool()?;

    Ok("Updated CodeWalker and rebuilt CwAssetTool.".to_owned())
}

fn command_stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("Failed to start {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !stderr.is_empty() {
            bail!("{} {} failed: {}", program, args.join(" "), stderr);
        }
        if !stdout.is_empty() {
            bail!("{} {} failed: {}", program, args.join(" "), stdout);
        }
        bail!("{} {} failed", program, args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn sanitize_for_path(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    let trimmed = result.trim_matches('_').to_owned();
    if trimmed.is_empty() {
        "item".to_owned()
    } else {
        trimmed
    }
}

fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

trait TextureDimensionsLabel {
    fn width_height_label(&self) -> String;
}

impl TextureDimensionsLabel for TextureEntry {
    fn width_height_label(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|slot| {
        let app_ref = slot.borrow().as_ref()?.upgrade()?;
        Some(f(&mut app_ref.borrow_mut()))
    })
}

fn with_app_ref(f: impl FnOnce(Rc<RefCell<App>>)) {
    APP.with(|slot| {
        if let Some(app_ref) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            f(app_ref);
        }
    });
}
