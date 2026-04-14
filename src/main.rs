mod config;
mod filesystem;
mod launcher;
mod setup;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
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

use config::{AppConfig, ThemePreference, CURRENT_SETUP_REVISION};
use filesystem::{missing_script_hook_files, set_directory_enabled, update_scripthook_ini};
use setup::{SetupStatus, SetupStep};

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
    pending_archive_folders: Vec<PendingArchiveFolder>,
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
            self.dirty = !self.pending_archive_folders.is_empty()
                || self.archive_entries.iter().any(|entry| entry.dirty);
        }
    }

    fn build_archive_actions(&self) -> Vec<RpfBuildAction> {
        let mut actions = Vec::new();

        let mut pending_folders = self.pending_archive_folders.iter().collect::<Vec<_>>();
        pending_folders.sort_by_key(|folder| folder.path.matches('\\').count());
        for folder in pending_folders {
            if let Some(parent_path) = archive_parent_path_from_entry_path(&folder.path) {
                actions.push(RpfBuildAction::AddFolder {
                    parent_path,
                    name: folder.name.clone(),
                });
            }
        }

        for entry in self.archive_entries.iter().filter(|entry| entry.dirty) {
            match (&entry.data, entry.added) {
                (ImportedArchiveEntryData::TextureAsset { xml_path, .. }, false) => {
                    actions.push(RpfBuildAction::ReplaceAssetXml {
                        entry_path: entry.entry_path.clone(),
                        source_path: xml_path.clone(),
                    });
                }
                (
                    ImportedArchiveEntryData::XmlText {
                        source_path,
                        source_kind,
                        ..
                    },
                    false,
                ) => match source_kind {
                    ArchiveTextSourceKind::RawText => {
                        actions.push(RpfBuildAction::ReplaceRawFile {
                            entry_path: entry.entry_path.clone(),
                            source_path: source_path.clone(),
                        });
                    }
                    ArchiveTextSourceKind::YmtXml => {
                        actions.push(RpfBuildAction::ReplaceYmtXml {
                            entry_path: entry.entry_path.clone(),
                            source_path: source_path.clone(),
                        });
                    }
                },
                (ImportedArchiveEntryData::XmlText { source_path, .. }, true)
                | (ImportedArchiveEntryData::StagedRaw { source_path }, true) => {
                    if let Some(parent_path) =
                        archive_parent_path_from_entry_path(&entry.entry_path)
                    {
                        actions.push(RpfBuildAction::AddRawFile {
                            parent_path,
                            name: entry.title.clone(),
                            source_path: source_path.clone(),
                        });
                    }
                }
                (ImportedArchiveEntryData::StagedRaw { source_path }, false) => {
                    actions.push(RpfBuildAction::ReplaceRawFile {
                        entry_path: entry.entry_path.clone(),
                        source_path: source_path.clone(),
                    });
                }
                (ImportedArchiveEntryData::TextureAsset { .. }, true) => {}
            }
        }

        actions
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

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ArchiveContentKind {
    Folder,
    Package,
    TextureAsset,
    XmlText,
    ConvertedXml,
    File,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchiveTextSourceKind {
    RawText,
    YmtXml,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpfTreeNode {
    name: String,
    path: String,
    display_path: String,
    kind: RpfTreeNodeKind,
    content_kind: ArchiveContentKind,
    children: Vec<RpfTreeNode>,
}

impl RpfTreeNode {
    fn find(&self, path: &str) -> Option<&Self> {
        if self.path == path {
            return Some(self);
        }

        self.children.iter().find_map(|child| child.find(path))
    }

    fn find_mut(&mut self, path: &str) -> Option<&mut Self> {
        if self.path == path {
            return Some(self);
        }

        for child in &mut self.children {
            if let Some(found) = child.find_mut(path) {
                return Some(found);
            }
        }

        None
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RpfTreeNodeKind {
    Folder,
    Package,
    File,
}

enum ImportedArchiveEntryData {
    TextureAsset {
        xml_path: PathBuf,
        textures: Vec<TextureEntry>,
    },
    XmlText {
        source_path: PathBuf,
        original_text: String,
        source_kind: ArchiveTextSourceKind,
    },
    StagedRaw {
        source_path: PathBuf,
    },
}

struct ImportedArchiveEntry {
    entry_path: String,
    title: String,
    content_kind: ArchiveContentKind,
    data: ImportedArchiveEntryData,
    dirty: bool,
    added: bool,
}

impl ImportedArchiveEntry {
    fn textures(&self) -> Option<&[TextureEntry]> {
        match &self.data {
            ImportedArchiveEntryData::TextureAsset { textures, .. } => Some(textures),
            ImportedArchiveEntryData::XmlText { .. }
            | ImportedArchiveEntryData::StagedRaw { .. } => None,
        }
    }

    fn textures_mut(&mut self) -> Option<&mut Vec<TextureEntry>> {
        match &mut self.data {
            ImportedArchiveEntryData::TextureAsset { textures, .. } => Some(textures),
            ImportedArchiveEntryData::XmlText { .. }
            | ImportedArchiveEntryData::StagedRaw { .. } => None,
        }
    }

    fn text_source_path(&self) -> Option<&Path> {
        match &self.data {
            ImportedArchiveEntryData::XmlText { source_path, .. }
            | ImportedArchiveEntryData::StagedRaw { source_path } => Some(source_path),
            ImportedArchiveEntryData::TextureAsset { .. } => None,
        }
    }

    fn original_text(&self) -> Option<&str> {
        match &self.data {
            ImportedArchiveEntryData::XmlText { original_text, .. } => Some(original_text),
            ImportedArchiveEntryData::TextureAsset { .. }
            | ImportedArchiveEntryData::StagedRaw { .. } => None,
        }
    }

    fn is_xml_text(&self) -> bool {
        matches!(
            self.content_kind,
            ArchiveContentKind::XmlText | ArchiveContentKind::ConvertedXml
        )
    }

    fn matches_node_content(&self, node: &RpfTreeNode) -> bool {
        if self.content_kind != node.content_kind {
            return false;
        }

        match node.content_kind {
            ArchiveContentKind::ConvertedXml => self
                .text_source_path()
                .and_then(|path| path.extension())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("xml")),
            ArchiveContentKind::Folder
            | ArchiveContentKind::Package
            | ArchiveContentKind::TextureAsset
            | ArchiveContentKind::XmlText
            | ArchiveContentKind::File => true,
        }
    }
}

struct PendingArchiveFolder {
    path: String,
    name: String,
}

#[derive(Clone)]
struct ImportedArchiveEntryDraft {
    entry_path: String,
    title: String,
    xml_path: PathBuf,
    textures: Vec<TextureEntryDraft>,
}

#[derive(Clone)]
struct ImportedArchiveTextDraft {
    entry_path: String,
    title: String,
    source_path: PathBuf,
    original_text: String,
    source_kind: ArchiveTextSourceKind,
}

enum ArchiveEntryOpenOutcome {
    TextureAsset(ImportedArchiveEntryDraft),
    XmlText(ImportedArchiveTextDraft),
    Unsupported(String),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RpfBuildManifest {
    actions: Vec<RpfBuildAction>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind")]
enum RpfBuildAction {
    #[serde(rename = "replace_asset_xml")]
    ReplaceAssetXml {
        #[serde(rename = "entryPath")]
        entry_path: String,
        #[serde(rename = "sourcePath")]
        source_path: PathBuf,
    },
    #[serde(rename = "replace_raw_file")]
    ReplaceRawFile {
        #[serde(rename = "entryPath")]
        entry_path: String,
        #[serde(rename = "sourcePath")]
        source_path: PathBuf,
    },
    #[serde(rename = "replace_ymt_xml")]
    ReplaceYmtXml {
        #[serde(rename = "entryPath")]
        entry_path: String,
        #[serde(rename = "sourcePath")]
        source_path: PathBuf,
    },
    #[serde(rename = "add_folder")]
    AddFolder {
        #[serde(rename = "parentPath")]
        parent_path: String,
        name: String,
    },
    #[serde(rename = "add_raw_file")]
    AddRawFile {
        #[serde(rename = "parentPath")]
        parent_path: String,
        name: String,
        #[serde(rename = "sourcePath")]
        source_path: PathBuf,
    },
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

struct TextEditorState {
    asset_index: usize,
    entry_path: String,
    draft_text: String,
    validation_message: Option<String>,
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
    PrepareVulkanRuntimeFinished(std::result::Result<String, String>),
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
    LaunchProgress(String),
    LaunchFinished(std::result::Result<String, String>),
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
    dashboard_play_button: gtk::Button,
    dashboard_editor_button: gtk::Button,
    dashboard_launch_settings_button: gtk::Button,
    dashboard_launch_settings_revealer: gtk::Revealer,
    dashboard_addons_toggle: gtk::Switch,
    dashboard_scripts_toggle: gtk::Switch,
    dashboard_notice_label: gtk::Label,
    dashboard_toggle_syncing: Rc<Cell<bool>>,
    browser_main_paned: gtk::Paned,
    packages_panel: gtk::Box,
    package_target_label: gtk::Label,
    import_to_mod_folder_button: gtk::Button,
    save_builds_button: gtk::Button,
    new_folder_entry: gtk::Entry,
    create_folder_button: gtk::Button,
    import_here_button: gtk::Button,
    move_here_button: gtk::Button,
    package_list_box: gtk::Box,
    textures_title_label: gtk::Label,
    textures_search_entry: gtk::SearchEntry,
    textures_path_label: gtk::Label,
    textures_back_button: gtk::Button,
    archive_add_button: gtk::Button,
    textures_notice_label: gtk::Label,
    texture_list_box: gtk::Box,
    preview_asset_label: gtk::Label,
    preview_texture_label: gtk::Label,
    preview_meta_label: gtk::Label,
    preview_stack: gtk::Stack,
    preview_picture: gtk::Picture,
    preview_text_view: gtk::TextView,
    preview_notice_label: gtk::Label,
    edit_button: gtk::Button,
    editor_title_label: gtk::Label,
    editor_meta_label: gtk::Label,
    editor_stack: gtk::Stack,
    editor_original_picture: gtk::Picture,
    editor_canvas_box: gtk::Box,
    editor_notice_label: gtk::Label,
    editor_apply_button: gtk::Button,
    text_editor_original_view: gtk::TextView,
    text_editor_edit_view: gtk::TextView,
    text_editor_notice_label: gtk::Label,
    text_editor_validate_button: gtk::Button,
    text_editor_save_button: gtk::Button,
    text_editor_buffer_syncing: Rc<Cell<bool>>,
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
    current_mod_browser_path: Option<PathBuf>,
    selected_texture: Option<usize>,
    editor: Option<EditorState>,
    text_editor: Option<TextEditorState>,
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
        let initial_status = if config.game_root_path.is_some() {
            "Ready.".to_owned()
        } else {
            "Ready. Choose the GTA V Linux game folder to begin.".to_owned()
        };
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
            current_mod_browser_path: None,
            selected_texture: None,
            editor: None,
            text_editor: None,
            last_asset_dir: None,
            last_image_dir: None,
            last_copy_dir: None,
            pending_jobs: 0,
            status: initial_status,
            job_tx,
            job_rx,
            widgets,
        }));

        {
            let mut borrowed = app.borrow_mut();
            borrowed.last_asset_dir = borrowed.config.last_asset_dir.clone();
            borrowed.last_image_dir = borrowed.config.last_image_dir.clone();
            borrowed.last_copy_dir = borrowed.config.last_copy_dir.clone();
            let _ = borrowed.apply_addon_script_settings();
            if let Some(path) = borrowed.mods_root_path() {
                borrowed.mod_tree_expanded_paths.insert(path);
            }
        }

        connect_signals(&app);
        attach_job_poller(&app);
        app.borrow_mut().refresh_all();
        if !app.borrow().setup_required() {
            app.borrow()
                .widgets
                .stack
                .set_visible_child_name("dashboard");
        }
        app.borrow().widgets.window.present();
        Ok(app)
    }

    fn refresh_all(&mut self) {
        self.setup_status = SetupStatus::detect(&self.tool_paths);
        self.refresh_header();
        self.refresh_status();
        self.refresh_setup_page();
        self.refresh_dashboard_page();
        self.refresh_package_tree();
        self.refresh_textures_list();
        self.refresh_preview_pane();
        self.refresh_editor_page();
    }

    fn setup_required(&self) -> bool {
        self.should_open_setup_on_start()
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

    fn game_root_path(&self) -> Option<&Path> {
        self.config
            .game_root_path
            .as_deref()
            .filter(|path| path.is_dir())
    }

    fn derived_mods_root_path(&self) -> Option<PathBuf> {
        self.game_root_path().map(|path| path.join("mods"))
    }

    fn hidden_mods_root_path(&self) -> Option<PathBuf> {
        self.game_root_path()
            .map(|path| path.join(".mods.disabled"))
    }

    fn derived_scripts_root_path(&self) -> Option<PathBuf> {
        self.game_root_path().map(|path| path.join("scripts"))
    }

    fn hidden_scripts_root_path(&self) -> Option<PathBuf> {
        self.game_root_path()
            .map(|path| path.join(".scripts.disabled"))
    }

    fn mods_root_path(&self) -> Option<PathBuf> {
        self.config
            .addons_enabled
            .then(|| self.derived_mods_root_path())
            .flatten()
    }

    fn scripthook_ini_path(&self) -> Option<PathBuf> {
        self.game_root_path()
            .map(|path| path.join("ScriptHookVDotNet.ini"))
    }

    fn editor_available(&self) -> bool {
        self.game_root_path().is_some() && self.config.addons_enabled
    }

    fn current_page_name(&self) -> Option<String> {
        self.widgets
            .stack
            .visible_child_name()
            .map(|name| name.to_string())
    }

    fn show_dashboard(&self) {
        self.widgets.stack.set_visible_child_name("dashboard");
    }

    fn show_browser(&self) {
        self.widgets.stack.set_visible_child_name("browser");
    }

    fn open_editor_browser(&mut self) {
        if !self.editor_available() {
            self.set_status("Enable addons to use the editor.");
            self.show_dashboard();
            return;
        }

        self.show_browser();
        self.refresh_all();
    }

    fn open_dashboard(&mut self) {
        self.editor = None;
        self.text_editor = None;
        self.show_dashboard();
        self.refresh_all();
    }

    fn queue_launch_game(&mut self) {
        let Some(game_root) = self.game_root_path().map(Path::to_path_buf) else {
            self.set_status("Choose the GTA V Linux game folder first.");
            return;
        };

        self.pending_jobs += 1;
        self.set_status("Preparing GTA V launch environment...");
        let tx = self.job_tx.clone();
        let workspace_dir = self.tool_paths.workspace_dir.clone();

        thread::spawn(move || {
            let result = (|| -> Result<String> {
                let _ = tx.send(JobResult::LaunchProgress(
                    "Preparing the Wine environment...".to_owned(),
                ));
                let prepare = launcher::prepare_environment(&workspace_dir)
                    .map_err(|error| anyhow!(error.to_string()))?;
                let _ = tx.send(JobResult::LaunchProgress(
                    "Checking or installing runtime dependencies. This can take several minutes on first launch..."
                        .to_owned(),
                ));
                let dependency_status =
                    launcher::ensure_runtime_dependencies(&workspace_dir, &prepare)
                        .map_err(|error| anyhow!(error.to_string()))?;
                let _ = tx.send(JobResult::LaunchProgress(
                    "Launching PlayGTAV.exe...".to_owned(),
                ));
                launcher::launch_game_prepared(&game_root, &prepare)
                    .map_err(|error| anyhow!(error.to_string()))?;
                Ok(format!(
                    "Started GTA V (runtime dependencies {}, Vulkan {}).",
                    dependency_status.as_label()
                    ,prepare.vulkan_status.as_label()
                ))
            })()
            .map_err(|error| format!("{error:#}"));

            let _ = tx.send(JobResult::LaunchFinished(result));
        });
    }

    fn should_open_setup_on_start(&self) -> bool {
        self.config.setup_revision < CURRENT_SETUP_REVISION
            || !self.config.setup_complete
            || !self.setup_status.setup_ready()
            || self.game_root_path().is_none()
    }

    fn set_game_root_path(&mut self, path: PathBuf) {
        let (game_root, selected_mods_folder) = normalize_selected_game_root(&path);
        if launcher::validate_game_directory(&game_root).is_err() {
            self.set_status(format!(
                "{} does not look like a GTA V Linux game folder.",
                path.display()
            ));
            return;
        }

        self.config.game_root_path = Some(game_root.clone());
        self.mod_tree_expanded_paths.clear();
        self.selected_mod_path = None;
        self.pending_import_paths.clear();
        self.assets.clear();
        self.selected_asset = None;
        self.selected_texture = None;
        self.editor = None;
        self.text_editor = None;
        if let Err(error) = self.apply_addon_script_settings() {
            self.set_status(format!("Failed to prepare game folders: {error:#}"));
            return;
        }
        if let Some(mods_path) = self.mods_root_path() {
            self.mod_tree_expanded_paths.insert(mods_path.clone());
            self.current_mod_browser_path = Some(mods_path.clone());
        } else {
            self.current_mod_browser_path = None;
        }
        self.close_settings_panel();
        self.persist_config();
        self.set_status(format!(
            "Using GTA V game folder at {} and mods folder at {}",
            game_root.display(),
            self.derived_mods_root_path()
                .as_deref()
                .unwrap_or_else(|| Path::new("mods"))
                .display()
        ));
        if selected_mods_folder {
            self.show_toast("Using the parent GTA V game folder and its mods subfolder.");
        } else {
            self.show_toast("GTA V game folder updated.");
        }
        self.refresh_all();
    }

    fn close_settings_panel(&self) {
        sync_settings_panel_visibility(&self.widgets, false);
        self.widgets.app_menu_button.set_active(false);
    }

    fn open_mods_root_folder(&mut self) {
        let Some(directory) = self
            .mods_root_path()
            .or_else(|| self.hidden_mods_root_path())
            .or_else(|| self.derived_mods_root_path())
        else {
            self.set_status("Choose a GTA V game folder first.");
            return;
        };

        if let Err(error) = open_directory(&directory) {
            self.set_status(format!("Failed to open folder: {error:#}"));
        }
    }

    fn set_addons_enabled(&mut self, enabled: bool) {
        self.config.addons_enabled = enabled;
        if let Err(error) = self.apply_addon_script_settings() {
            self.set_status(format!("Failed to update addons: {error:#}"));
            return;
        }
        self.persist_config();
        self.refresh_all();
    }

    fn set_script_mods_enabled(&mut self, enabled: bool) {
        self.config.script_mods_enabled = enabled;
        if let Err(error) = self.apply_addon_script_settings() {
            self.set_status(format!("Failed to update script mods: {error:#}"));
            return;
        }
        self.persist_config();
        self.refresh_all();
    }

    fn apply_addon_script_settings(&mut self) -> Result<()> {
        let Some(game_root) = self.game_root_path().map(Path::to_path_buf) else {
            return Ok(());
        };

        let mods_path = self
            .derived_mods_root_path()
            .unwrap_or_else(|| game_root.join("mods"));
        let hidden_mods_path = self
            .hidden_mods_root_path()
            .unwrap_or_else(|| game_root.join(".mods.disabled"));
        let scripts_path = self
            .derived_scripts_root_path()
            .unwrap_or_else(|| game_root.join("scripts"));
        let hidden_scripts_path = self
            .hidden_scripts_root_path()
            .unwrap_or_else(|| game_root.join(".scripts.disabled"));

        set_directory_enabled(&mods_path, &hidden_mods_path, self.config.addons_enabled)?;
        set_directory_enabled(
            &scripts_path,
            &hidden_scripts_path,
            self.config.script_mods_enabled,
        )?;

        let missing_files = missing_script_hook_files(&game_root);
        let script_hook_ready = missing_files.is_empty();

        let scripthook_ini = self
            .scripthook_ini_path()
            .unwrap_or_else(|| game_root.join("ScriptHookVDotNet.ini"));

        if self.config.addons_enabled || self.config.script_mods_enabled {
            if script_hook_ready {
                update_scripthook_ini(&scripthook_ini, true)?;
            } else {
                self.status = format!("ScriptHook files are missing: {}", missing_files.join(", "));
            }
        } else if scripthook_ini.exists() {
            update_scripthook_ini(&scripthook_ini, false)?;
        }

        if !self.config.addons_enabled {
            self.assets.clear();
            self.selected_asset = None;
            self.selected_texture = None;
            self.editor = None;
            self.text_editor = None;
            self.selected_mod_path = None;
            self.current_mod_browser_path = None;
            if matches!(
                self.current_page_name().as_deref(),
                Some("browser") | Some("editor")
            ) {
                self.show_dashboard();
            }
        } else if let Some(mods_path) = self.mods_root_path() {
            self.mod_tree_expanded_paths.insert(mods_path.clone());
            if self.current_mod_browser_path.is_none() {
                self.current_mod_browser_path = Some(mods_path);
            }
        }

        Ok(())
    }

    fn in_editor(&self) -> bool {
        self.editor.is_some() || self.text_editor.is_some()
    }

    fn current_entry_editable(&self) -> bool {
        self.current_archive_text_entry().is_some()
            || self
                .current_texture_entry()
                .is_some_and(|texture| texture.format.supported_for_write())
    }

    fn current_edit_button_label(&self) -> &'static str {
        if self.current_archive_text_entry().is_some() {
            "Edit XML"
        } else {
            "Edit Texture"
        }
    }

    fn rerun_setup_wizard(&mut self) {
        self.setup_step = SetupStep::Welcome;
        self.widgets.stack.set_visible_child_name("setup");
        self.refresh_all();
    }

    fn show_browser_if_editor_visible(&self) {
        if self.widgets.stack.visible_child_name().as_deref() == Some("editor") {
            self.widgets.stack.set_visible_child_name("browser");
        }
    }

    fn refresh_header(&self) {
        let in_editor = self.in_editor();
        let setup_required = self.setup_required();
        let current_page = self.current_page_name();
        let on_browser = current_page.as_deref() == Some("browser");
        let on_dashboard = current_page.as_deref() == Some("dashboard");

        self.widgets.back_button.set_visible(
            in_editor || matches!(current_page.as_deref(), Some("browser") | Some("setup")),
        );
        self.widgets
            .app_menu_button
            .set_sensitive(!in_editor && (on_browser || on_dashboard));
        if !(on_browser || on_dashboard) {
            self.widgets.app_menu_button.set_active(false);
        }
        let mod_folder_text = match (
            self.game_root_path(),
            self.derived_mods_root_path(),
            self.derived_scripts_root_path(),
        ) {
            (Some(game_root), Some(mods_root), Some(scripts_root)) => format!(
                "Game: {}\nMods: {}\nScripts: {}",
                game_root.display(),
                mods_root.display(),
                scripts_root.display()
            ),
            _ => "Not configured".to_owned(),
        };
        self.widgets
            .mod_folder_path_label
            .set_text(&mod_folder_text);
        self.widgets
            .open_mod_folder_button
            .set_sensitive(self.derived_mods_root_path().is_some());
        self.widgets
            .backup_before_save_check
            .set_active(self.config.backup_before_save);
        self.widgets.save_button.set_visible(on_browser);
        self.widgets
            .open_build_folder_button
            .set_visible(on_browser);
        self.widgets.copy_all_button.set_visible(on_browser);
        self.widgets.import_button.set_visible(on_browser);
        self.widgets.settings_button.set_visible(on_browser);
        self.widgets
            .edit_button
            .set_label(self.current_edit_button_label());
        self.widgets.edit_button.set_sensitive(
            on_browser && !in_editor && !setup_required && self.current_entry_editable(),
        );
        self.widgets.editor_apply_button.set_sensitive(
            !setup_required
                && self.editor.as_ref().is_some_and(|editor| {
                    let texture = self.assets.get(editor.asset_index).and_then(|asset| {
                        if let Some(entry_path) = editor.entry_path.as_deref() {
                            asset
                                .find_archive_entry(entry_path)
                                .and_then(|entry| entry.textures())
                                .and_then(|textures| textures.get(editor.texture_index))
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
                        && self.setup_status.wine_available
                        && self.setup_status.wineboot_available
                        && self.setup_status.wineserver_available
                        && self.setup_status.winetricks_available
                        && self.setup_status.bash_available
                        && self.setup_status.tar_available
                }
                SetupStep::BuildHelper => self.setup_status.cwassettool_binary,
                SetupStep::GameFolder => self.game_root_path().is_some(),
                SetupStep::VulkanRuntime => self.setup_status.vulkan_runtime_ready,
                SetupStep::Ready => false,
            });
        self.widgets
            .setup_action_button
            .set_sensitive(match self.setup_step {
                SetupStep::Welcome => true,
                SetupStep::ExternalTools => !self.setup_status.codewalker_source,
                SetupStep::SystemDependencies => false,
                SetupStep::BuildHelper => !self.setup_status.cwassettool_binary,
                SetupStep::GameFolder => true,
                SetupStep::VulkanRuntime => !self.setup_status.vulkan_runtime_ready,
                SetupStep::Ready => {
                    self.setup_status.setup_ready() && self.game_root_path().is_some()
                }
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
                "The app also needs git, dotnet, ImageMagick, and the Wine launcher tools available on the system. Install any missing dependency before continuing.",
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
            SetupStep::GameFolder => (
                "Choose the GTA V game folder. The app will automatically use or create the mods folder inside it, while all temporary edit files stay in the app workspace.",
                Some("Choose Game Folder"),
            ),
            SetupStep::VulkanRuntime => (
                "Prepare the local Vulkan runtime bundle used to run the Linux-optimized GTA V build. The wizard will cache the required files in this app workspace.",
                Some("Prepare Vulkan Runtime"),
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
                self.widgets.setup_list_box.append(&setup_status_row(
                    "wine",
                    self.setup_status.wine_available,
                    "Required to launch the GTA V Linux variant",
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "wineboot",
                    self.setup_status.wineboot_available,
                    "Required to initialize the Wine prefix",
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "wineserver",
                    self.setup_status.wineserver_available,
                    "Required to wait for Wine prefix setup",
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "winetricks",
                    self.setup_status.winetricks_available,
                    "Required to install the GTA runtime dependencies",
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "bash",
                    self.setup_status.bash_available,
                    "Required to apply the Vulkan runtime scripts",
                ));
                self.widgets.setup_list_box.append(&setup_status_row(
                    "tar",
                    self.setup_status.tar_available,
                    "Required to extract the cached Vulkan runtime bundle",
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
            SetupStep::GameFolder => {
                let game_root = self
                    .game_root_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "Not configured".to_owned());
                let mods_root = self
                    .mods_root_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "Will be created automatically".to_owned());
                self.widgets.setup_list_box.append(&setup_status_row(
                    "Game folder",
                    self.game_root_path().is_some(),
                    &game_root,
                ));
                self.widgets
                    .setup_list_box
                    .append(&setup_info_row("Derived mods folder", &mods_root));
            }
            SetupStep::VulkanRuntime => {
                let cache_path =
                    launcher::cached_vulkan_archive_path(&self.tool_paths.workspace_dir);
                self.widgets.setup_list_box.append(&setup_status_row(
                    "Cached Vulkan runtime",
                    self.setup_status.vulkan_runtime_ready,
                    &cache_path.display().to_string(),
                ));
                self.widgets.setup_list_box.append(&setup_info_row(
                    "Expected source",
                    "/home/takasu/Documents/codinglab/rusty-gta/vulkan.tar.xz",
                ));
            }
            SetupStep::Ready => {
                self.widgets.setup_list_box.append(&setup_status_row(
                    "Setup complete",
                    self.setup_status.setup_ready() && self.game_root_path().is_some(),
                    "All required tools are present and the GTA V game folder is configured",
                ));
            }
        }
    }

    fn refresh_dashboard_page(&self) {
        let page_name = self.current_page_name();
        let on_dashboard = page_name.as_deref() == Some("dashboard");
        self.widgets
            .dashboard_launch_settings_button
            .set_visible(on_dashboard && self.game_root_path().is_some());
        self.widgets
            .dashboard_launch_settings_revealer
            .set_reveal_child(self.config.play_settings_expanded);
        self.widgets
            .dashboard_play_button
            .set_sensitive(!self.setup_required() && self.game_root_path().is_some());
        self.widgets
            .dashboard_editor_button
            .set_sensitive(!self.setup_required() && self.editor_available());

        self.widgets.dashboard_toggle_syncing.set(true);
        if self.widgets.dashboard_addons_toggle.is_active() != self.config.addons_enabled {
            self.widgets
                .dashboard_addons_toggle
                .set_active(self.config.addons_enabled);
        }
        if self.widgets.dashboard_scripts_toggle.is_active() != self.config.script_mods_enabled {
            self.widgets
                .dashboard_scripts_toggle
                .set_active(self.config.script_mods_enabled);
        }
        self.widgets.dashboard_toggle_syncing.set(false);

        let notice = if self.game_root_path().is_none() {
            "Choose the GTA V Linux game folder in setup before launching or editing.".to_owned()
        } else {
            let missing_files =
                missing_script_hook_files(self.game_root_path().expect("checked is_some above"));
            let mut parts = Vec::new();
            if !self.config.addons_enabled {
                parts.push("Addons are disabled, so the editor is unavailable.".to_owned());
            }
            if (self.config.addons_enabled || self.config.script_mods_enabled)
                && !missing_files.is_empty()
            {
                parts.push(format!(
                    "ScriptHook files missing: {}",
                    missing_files.join(", ")
                ));
            }
            if parts.is_empty() {
                "Game, addons, and editor are ready.".to_owned()
            } else {
                parts.join(" ")
            }
        };
        self.widgets.dashboard_notice_label.set_text(&notice);
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
        self.current_mod_browser_path = path.parent().map(Path::to_path_buf);

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
        let texture_index = self.selected_texture?;
        if let Some(entry) = self.current_archive_entry() {
            return entry.textures()?.get(texture_index);
        }

        let asset_index = self.selected_asset?;
        self.assets.get(asset_index)?.textures.get(texture_index)
    }

    fn current_texture_entry_mut(&mut self) -> Option<&mut TextureEntry> {
        let asset_index = self.selected_asset?;
        let texture_index = self.selected_texture?;
        let entry_path = self.active_archive_entry_path(asset_index);
        let asset = self.assets.get_mut(asset_index)?;

        if let Some(entry_path) = entry_path {
            asset
                .find_archive_entry_mut(&entry_path)?
                .textures_mut()?
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

    fn current_archive_text_entry(&self) -> Option<&ImportedArchiveEntry> {
        let entry = self.current_archive_entry()?;
        entry.is_xml_text().then_some(entry)
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
        if self.selected_asset.is_none() {
            self.open_mod_browser_parent();
            return;
        }

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
        let parent_path = self
            .assets
            .get(asset_index)
            .and_then(|asset| self.archive_parent_path(asset));
        let Some(parent_path) = parent_path else {
            if let Some(folder_path) = self.current_mod_browser_path.clone() {
                self.browse_mod_directory(folder_path);
            }
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

        let cached_entry_usable = asset
            .find_archive_entry(&entry_path)
            .is_some_and(|entry| entry.matches_node_content(&node));

        if let Some(existing_kind) = cached_entry_usable
            .then(|| {
                asset
                    .find_archive_entry(&entry_path)
                    .map(|entry| entry.content_kind)
            })
            .flatten()
        {
            asset.archive_file_loading_path = None;
            match existing_kind {
                ArchiveContentKind::TextureAsset => {
                    let texture_count = asset
                        .find_archive_entry(&entry_path)
                        .and_then(|entry| entry.textures())
                        .map(|textures| textures.len())
                        .unwrap_or(0);
                    asset.archive_file_notice = if texture_count == 0 {
                        Some("Not supported.".to_owned())
                    } else {
                        None
                    };
                    self.selected_texture = if texture_count == 0 { None } else { Some(0) };
                }
                ArchiveContentKind::XmlText | ArchiveContentKind::ConvertedXml => {
                    if existing_kind == ArchiveContentKind::ConvertedXml {
                        if let Some(entry) = asset.find_archive_entry_mut(&entry_path) {
                            if let Some(source_path) =
                                entry.text_source_path().map(Path::to_path_buf)
                            {
                                if let Ok(text) = fs::read_to_string(&source_path) {
                                    if let Ok(resolved_text) =
                                        apply_known_hash_name_overrides(&text)
                                    {
                                        if resolved_text != text {
                                            let _ = fs::write(&source_path, &resolved_text);
                                        }
                                        if let ImportedArchiveEntryData::XmlText {
                                            original_text,
                                            ..
                                        } = &mut entry.data
                                        {
                                            *original_text = resolved_text;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    asset.archive_file_notice = None;
                    self.selected_texture = None;
                }
                ArchiveContentKind::File => {
                    asset.archive_file_notice = Some(
                        "This staged file will be added when you rebuild the package.".to_owned(),
                    );
                    self.selected_texture = None;
                }
                ArchiveContentKind::Folder | ArchiveContentKind::Package => {
                    asset.archive_file_notice = Some("Not supported.".to_owned());
                    self.selected_texture = None;
                }
            };
            let _ = asset;
            if matches!(node.content_kind, ArchiveContentKind::TextureAsset) {
                self.request_preview_for_selected_texture();
            }
            self.refresh_all();
            return;
        }

        let asset_id = asset.id.clone();
        let archive_path = asset.source_path.clone();
        let file_title = node.name.clone();
        let content_kind = node.content_kind;
        if !matches!(
            content_kind,
            ArchiveContentKind::TextureAsset
                | ArchiveContentKind::XmlText
                | ArchiveContentKind::ConvertedXml
        ) {
            asset.archive_file_loading_path = None;
            asset.archive_file_notice = Some("Not supported.".to_owned());
            let _ = asset;
            self.refresh_all();
            return;
        }
        asset.archive_file_loading_path = Some(entry_path.clone());
        let tool_paths = self.tool_paths.clone();
        let tx = self.job_tx.clone();

        self.pending_jobs += 1;
        self.set_status(format!("Opening {}...", file_title));

        thread::spawn(move || {
            let result = match content_kind {
                ArchiveContentKind::TextureAsset => {
                    export_archive_entry_draft(&tool_paths, &archive_path, &entry_path)
                        .map(ArchiveEntryOpenOutcome::TextureAsset)
                        .or_else(|error| {
                            if error.to_string().contains("No DDS textures were found") {
                                Ok(ArchiveEntryOpenOutcome::Unsupported(
                                    "Not supported.".to_owned(),
                                ))
                            } else {
                                Err(error)
                            }
                        })
                        .map_err(|error| format!("{}", error))
                }
                ArchiveContentKind::XmlText => {
                    export_archive_text_draft(&tool_paths, &archive_path, &entry_path)
                        .map(ArchiveEntryOpenOutcome::XmlText)
                        .map_err(|error| format!("{}", error))
                }
                ArchiveContentKind::ConvertedXml => {
                    export_archive_ymt_draft(&tool_paths, &archive_path, &entry_path)
                        .map(ArchiveEntryOpenOutcome::XmlText)
                        .map_err(|error| format!("{}", error))
                }
                ArchiveContentKind::Folder
                | ArchiveContentKind::Package
                | ArchiveContentKind::File => Ok(ArchiveEntryOpenOutcome::Unsupported(
                    "Not supported.".to_owned(),
                )),
            };

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
            self.current_mod_browser_path = asset.source_path.parent().map(Path::to_path_buf);
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
            if !matches!(job, JobResult::LaunchProgress(_)) {
                self.pending_jobs = self.pending_jobs.saturating_sub(1);
            }

            match job {
                JobResult::LaunchProgress(message) => {
                    self.set_status(message);
                }
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
                                pending_archive_folders: Vec::new(),
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
                            Ok(ArchiveEntryOpenOutcome::TextureAsset(draft)) => {
                                let opened_title = draft.title.clone();
                                let imported_entry = ImportedArchiveEntry {
                                    entry_path: draft.entry_path.clone(),
                                    title: draft.title,
                                    content_kind: ArchiveContentKind::TextureAsset,
                                    data: ImportedArchiveEntryData::TextureAsset {
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
                                    },
                                    dirty: false,
                                    added: false,
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
                            Ok(ArchiveEntryOpenOutcome::XmlText(draft)) => {
                                let opened_title = draft.title.clone();
                                let imported_entry = ImportedArchiveEntry {
                                    entry_path: draft.entry_path.clone(),
                                    title: draft.title,
                                    content_kind: match draft.source_kind {
                                        ArchiveTextSourceKind::RawText => {
                                            ArchiveContentKind::XmlText
                                        }
                                        ArchiveTextSourceKind::YmtXml => {
                                            ArchiveContentKind::ConvertedXml
                                        }
                                    },
                                    data: ImportedArchiveEntryData::XmlText {
                                        source_path: draft.source_path,
                                        original_text: draft.original_text,
                                        source_kind: draft.source_kind,
                                    },
                                    dirty: false,
                                    added: false,
                                };

                                if let Some(existing) =
                                    asset.find_archive_entry_mut(&draft.entry_path)
                                {
                                    *existing = imported_entry;
                                } else {
                                    asset.archive_entries.push(imported_entry);
                                }

                                asset.archive_selected_file = Some(entry_path);
                                asset.archive_file_notice = None;
                                self.selected_texture = None;
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
                            self.setup_step = SetupStep::GameFolder;
                        }
                        self.set_status("Built the CwAssetTool helper successfully.");
                    }
                    Err(error) => {
                        self.set_status(format!("Helper build failed: {error}"));
                    }
                },
                JobResult::PrepareVulkanRuntimeFinished(result) => match result {
                    Ok(message) => {
                        self.setup_status = SetupStatus::detect(&self.tool_paths);
                        if self.setup_step == SetupStep::VulkanRuntime {
                            self.setup_step = SetupStep::Ready;
                        }
                        self.set_status(message);
                    }
                    Err(error) => {
                        self.set_status(format!("Vulkan runtime setup failed: {error}"));
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
                                .and_then(|entry| entry.textures_mut())
                                .and_then(|textures| textures.get_mut(texture_index))
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
                                asset.pending_archive_folders.clear();
                                for entry in &mut asset.archive_entries {
                                    entry.dirty = false;
                                    entry.added = false;
                                    if let ImportedArchiveEntryData::XmlText {
                                        source_path,
                                        original_text,
                                        ..
                                    } = &mut entry.data
                                    {
                                        if let Ok(text) = fs::read_to_string(source_path) {
                                            *original_text = text;
                                        }
                                    }
                                    if let Some(textures) = entry.textures_mut() {
                                        for texture in textures {
                                            texture.modified = false;
                                        }
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
                                .and_then(|entry| entry.textures_mut())
                                .and_then(|textures| textures.get_mut(texture_index))
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
                            self.text_editor = None;
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
                JobResult::LaunchFinished(result) => match result {
                    Ok(message) => {
                        self.set_status(message.clone());
                        self.show_toast(message);
                    }
                    Err(error) => {
                        self.set_status(format!("Launch failed: {error}"));
                    }
                },
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

        if self.game_root_path().is_some() && !self.config.addons_enabled {
            self.widgets.packages_panel.set_size_request(250, -1);
            self.widgets.browser_main_paned.set_position(250);
            self.widgets
                .import_to_mod_folder_button
                .set_sensitive(false);
            self.widgets.save_builds_button.set_sensitive(false);
            self.widgets
                .package_target_label
                .set_text("Addons are disabled");
            let empty_state = gtk::Box::new(gtk::Orientation::Vertical, 12);
            empty_state.set_margin_top(20);
            empty_state.set_margin_bottom(12);
            empty_state.set_margin_start(6);
            empty_state.set_margin_end(6);
            empty_state.add_css_class("card");
            let message = gtk::Label::new(Some(
                "Enable addons from the Play settings on the dashboard to use the editor and the mods folder browser.",
            ));
            message.set_xalign(0.0);
            message.set_wrap(true);
            empty_state.append(&message);
            self.widgets.package_list_box.append(&empty_state);
            return;
        }

        let Some(mods_root) = self.mods_root_path() else {
            self.widgets.packages_panel.set_size_request(250, -1);
            self.widgets.browser_main_paned.set_position(250);
            self.widgets
                .import_to_mod_folder_button
                .set_sensitive(false);
            self.widgets.save_builds_button.set_sensitive(false);
            self.widgets
                .package_target_label
                .set_text("GTA V game folder not configured");
            let empty_state = gtk::Box::new(gtk::Orientation::Vertical, 12);
            empty_state.set_margin_top(20);
            empty_state.set_margin_bottom(12);
            empty_state.set_margin_start(6);
            empty_state.set_margin_end(6);
            empty_state.add_css_class("card");
            let choose_button = gtk::Button::from_icon_name("folder-open-symbolic");
            choose_button.add_css_class("suggested-action");
            choose_button.set_tooltip_text(Some("Choose GTA V game folder"));
            choose_button.connect_clicked(move |_| {
                with_app_ref(|app| {
                    present_mod_folder_dialog(&app);
                });
            });
            choose_button.set_halign(gtk::Align::Start);
            let message = gtk::Label::new(Some(
                "The GTA V game folder is required. The app will automatically use or create the mods folder inside it.",
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
        self.widgets
            .import_to_mod_folder_button
            .set_sensitive(self.selected_mod_directory().is_some());
        self.widgets
            .save_builds_button
            .set_sensitive(self.assets.iter().any(|asset| asset.dirty));
        append_mod_folder_rows(self, &self.widgets.package_list_box, &mods_root, 0);
    }

    fn refresh_textures_list(&self) {
        clear_box(&self.widgets.texture_list_box);
        self.widgets.textures_notice_label.set_text("");
        self.widgets.textures_path_label.set_text("");

        let Some(asset_index) = self.selected_asset else {
            if let Some(current_dir) = self.current_mod_browser_path.as_ref() {
                self.widgets.textures_search_entry.set_visible(false);
                self.widgets.archive_add_button.set_visible(false);
                self.widgets
                    .textures_title_label
                    .set_text("Folder Explorer");
                self.widgets
                    .textures_path_label
                    .set_text(&current_dir.display().to_string());
                self.widgets
                    .textures_back_button
                    .set_visible(self.mod_browser_parent_path().is_some());

                match read_mod_tree_entries(current_dir) {
                    Ok(entries) => {
                        if entries.is_empty() {
                            self.widgets
                                .textures_notice_label
                                .set_text("This folder is empty.");
                        } else {
                            append_mod_browser_rows(self, &self.widgets.texture_list_box, &entries);
                        }
                    }
                    Err(_) => {
                        self.widgets
                            .textures_notice_label
                            .set_text("Failed to read this folder.");
                    }
                }
                return;
            }

            self.widgets.textures_search_entry.set_visible(false);
            self.widgets.textures_back_button.set_visible(false);
            self.widgets.archive_add_button.set_visible(false);
            self.widgets
                .textures_title_label
                .set_text("Select a package from the left pane.");
            self.widgets.textures_path_label.set_text("");
            return;
        };

        let Some(asset) = self.assets.get(asset_index) else {
            self.widgets.textures_search_entry.set_visible(false);
            self.widgets.textures_back_button.set_visible(false);
            self.widgets.archive_add_button.set_visible(false);
            self.widgets
                .textures_title_label
                .set_text("Select a package from the left pane.");
            self.widgets.textures_path_label.set_text("");
            return;
        };

        if asset.is_archive() {
            self.widgets.textures_search_entry.set_visible(true);
            self.widgets.archive_add_button.set_visible(true);
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
                asset.archive_selected_file.is_some()
                    || self.archive_parent_path(asset).is_some()
                    || self.current_mod_browser_path.is_some(),
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
                    match entry.content_kind {
                        ArchiveContentKind::TextureAsset => {
                            let textures = entry.textures().unwrap_or(&[]);
                            self.widgets.textures_title_label.set_text(&format!(
                                "{} ({} textures)",
                                entry.title,
                                textures.len()
                            ));
                            if textures.is_empty() {
                                self.widgets
                                    .textures_notice_label
                                    .set_text("Not supported.");
                            } else {
                                append_texture_rows(
                                    &self.widgets.texture_list_box,
                                    textures,
                                    self.selected_texture,
                                );
                            }
                        }
                        ArchiveContentKind::XmlText | ArchiveContentKind::ConvertedXml => {
                            self.widgets.textures_title_label.set_text(&entry.title);
                            self.widgets
                                .textures_notice_label
                                .set_text("XML preview is available on the right.");
                        }
                        ArchiveContentKind::File => {
                            self.widgets.textures_title_label.set_text(&entry.title);
                            self.widgets.textures_notice_label.set_text(
                                "This staged file will be added when you rebuild the package.",
                            );
                        }
                        ArchiveContentKind::Folder | ArchiveContentKind::Package => {
                            self.widgets.textures_title_label.set_text(&entry.title);
                            self.widgets
                                .textures_notice_label
                                .set_text("Not supported.");
                        }
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
                    asset.archive_selected_file.as_deref(),
                    asset.archive_current_path.as_deref(),
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
        self.widgets
            .textures_back_button
            .set_visible(self.current_mod_browser_path.is_some());
        self.widgets.archive_add_button.set_visible(false);
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
            if let Some(current_dir) = self.current_mod_browser_path.as_ref() {
                self.widgets
                    .preview_asset_label
                    .set_text(&current_dir.display().to_string());
                self.widgets
                    .preview_texture_label
                    .set_text("Folder browser");
                self.widgets
                    .preview_meta_label
                    .set_text("Open a supported file or navigate into a subfolder.");
                self.widgets.preview_notice_label.set_text("");
                self.widgets.preview_stack.set_visible_child_name("image");
                self.widgets.preview_text_view.buffer().set_text("");
                self.widgets
                    .preview_picture
                    .set_paintable(Option::<&gdk::Paintable>::None);
                return;
            }

            self.widgets
                .preview_asset_label
                .set_text("Select a package first.");
            self.widgets.preview_texture_label.set_text("");
            self.widgets.preview_meta_label.set_text("");
            self.widgets.preview_notice_label.set_text("");
            self.widgets.preview_stack.set_visible_child_name("image");
            self.widgets.preview_text_view.buffer().set_text("");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
            return;
        };
        let asset = &self.assets[asset_index];

        if let Some(entry) = self.current_archive_entry() {
            match entry.content_kind {
                ArchiveContentKind::XmlText | ArchiveContentKind::ConvertedXml => {
                    self.widgets.preview_asset_label.set_text(&format!(
                        "{} / {}",
                        asset.title(),
                        entry.title
                    ));
                    self.widgets.preview_texture_label.set_text(&entry.title);
                    self.widgets
                        .preview_meta_label
                        .set_text("XML / META / YMT text");
                    self.widgets.preview_stack.set_visible_child_name("text");
                    self.widgets
                        .preview_picture
                        .set_paintable(Option::<&gdk::Paintable>::None);
                    let text = entry
                        .text_source_path()
                        .and_then(|path| fs::read_to_string(path).ok())
                        .unwrap_or_default();
                    self.widgets.preview_text_view.buffer().set_text(&text);
                    self.widgets.preview_notice_label.set_text(if entry.dirty {
                        "Staged changes will be used when you rebuild the package."
                    } else {
                        ""
                    });
                    return;
                }
                ArchiveContentKind::File => {
                    self.widgets.preview_asset_label.set_text(&format!(
                        "{} / {}",
                        asset.title(),
                        entry.title
                    ));
                    self.widgets.preview_texture_label.set_text(&entry.title);
                    self.widgets.preview_meta_label.set_text("Staged file");
                    self.widgets
                        .preview_notice_label
                        .set_text("This staged file will be added when you rebuild the package.");
                    self.widgets.preview_stack.set_visible_child_name("image");
                    self.widgets.preview_text_view.buffer().set_text("");
                    self.widgets
                        .preview_picture
                        .set_paintable(Option::<&gdk::Paintable>::None);
                    return;
                }
                ArchiveContentKind::Folder
                | ArchiveContentKind::Package
                | ArchiveContentKind::TextureAsset => {}
            }
        }

        if !asset.is_archive() && self.selected_texture.is_none() {
            self.widgets
                .preview_asset_label
                .set_text("Select a texture to preview.");
            self.widgets.preview_texture_label.set_text("");
            self.widgets.preview_meta_label.set_text("");
            self.widgets.preview_notice_label.set_text("");
            self.widgets.preview_stack.set_visible_child_name("image");
            self.widgets.preview_text_view.buffer().set_text("");
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
            self.widgets.preview_stack.set_visible_child_name("image");
            self.widgets.preview_text_view.buffer().set_text("");
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
            self.widgets.preview_stack.set_visible_child_name("image");
            self.widgets.preview_text_view.buffer().set_text("");
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
            self.widgets.preview_stack.set_visible_child_name("image");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
        } else if let Some(preview) = &texture.preview_texture {
            self.widgets.preview_notice_label.set_text("");
            self.widgets.preview_stack.set_visible_child_name("image");
            self.widgets.preview_picture.set_paintable(Some(preview));
        } else {
            self.widgets
                .preview_notice_label
                .set_text("Preview not available yet.");
            self.widgets.preview_stack.set_visible_child_name("image");
            self.widgets
                .preview_picture
                .set_paintable(Option::<&gdk::Paintable>::None);
        }
    }

    fn open_editor_page(&mut self) {
        if let Some((entry_path, draft_text)) = self.current_archive_text_entry().map(|entry| {
            (
                entry.entry_path.clone(),
                entry
                    .text_source_path()
                    .and_then(|path| fs::read_to_string(path).ok())
                    .unwrap_or_default(),
            )
        }) {
            let Some(asset_index) = self.selected_asset else {
                return;
            };
            self.editor = None;
            self.text_editor = Some(TextEditorState {
                asset_index,
                entry_path,
                draft_text,
                validation_message: None,
            });
            self.widgets.stack.set_visible_child_name("editor");
            self.refresh_all();
            return;
        }

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
        self.text_editor = None;
        self.widgets.stack.set_visible_child_name("editor");
        self.refresh_all();
    }

    fn close_editor_page(&mut self) {
        self.editor = None;
        self.text_editor = None;
        self.widgets.stack.set_visible_child_name("browser");
        self.refresh_all();
    }

    fn refresh_editor_page(&self) {
        if self.setup_required() {
            self.widgets.stack.set_visible_child_name("setup");
            return;
        }

        if let Some(text_editor) = &self.text_editor {
            let Some(asset) = self.assets.get(text_editor.asset_index) else {
                self.show_browser_if_editor_visible();
                return;
            };
            let Some(entry) = asset.find_archive_entry(&text_editor.entry_path) else {
                self.show_browser_if_editor_visible();
                return;
            };
            let Some(original_text) = entry.original_text() else {
                self.show_browser_if_editor_visible();
                return;
            };

            self.widgets.stack.set_visible_child_name("editor");
            self.widgets.editor_stack.set_visible_child_name("text");
            self.widgets.editor_title_label.set_text(&format!(
                "Editing {} / {}",
                asset.title(),
                entry.title
            ));
            self.widgets
                .editor_meta_label
                .set_text("Original text on the left, staged text on the right.");
            self.widgets
                .text_editor_notice_label
                .set_text(text_editor.validation_message.as_deref().unwrap_or(""));

            let original_buffer = self.widgets.text_editor_original_view.buffer();
            if text_buffer_text(&original_buffer) != original_text {
                original_buffer.set_text(original_text);
            }

            let edit_buffer = self.widgets.text_editor_edit_view.buffer();
            if text_buffer_text(&edit_buffer) != text_editor.draft_text {
                self.widgets.text_editor_buffer_syncing.set(true);
                edit_buffer.set_text(&text_editor.draft_text);
                self.widgets.text_editor_buffer_syncing.set(false);
            }
            return;
        }

        let Some(editor) = &self.editor else {
            self.show_browser_if_editor_visible();
            return;
        };

        let asset = &self.assets[editor.asset_index];
        let texture = if let Some(entry_path) = editor.entry_path.as_deref() {
            let Some(entry) = asset.find_archive_entry(entry_path) else {
                self.show_browser_if_editor_visible();
                return;
            };
            let Some(texture) = entry
                .textures()
                .and_then(|textures| textures.get(editor.texture_index))
            else {
                self.show_browser_if_editor_visible();
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
                self.show_browser_if_editor_visible();
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
        self.widgets.editor_stack.set_visible_child_name("texture");
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

    fn sync_text_editor_from_buffer(&mut self) {
        let Some(text_editor) = self.text_editor.as_mut() else {
            return;
        };
        text_editor.draft_text = text_buffer_text(&self.widgets.text_editor_edit_view.buffer());
        text_editor.validation_message = None;
    }

    fn validate_text_editor(&mut self) {
        self.sync_text_editor_from_buffer();
        let Some(text_editor) = self.text_editor.as_mut() else {
            return;
        };

        match Document::parse(&text_editor.draft_text) {
            Ok(_) => {
                text_editor.validation_message = Some("XML is valid.".to_owned());
                self.set_status("XML is valid.");
            }
            Err(error) => {
                text_editor.validation_message = Some(format!("XML is not valid: {error}"));
                self.set_status(format!("XML validation failed: {error}"));
            }
        }

        self.refresh_editor_page();
    }

    fn save_text_editor(&mut self) {
        self.sync_text_editor_from_buffer();
        let Some(text_editor) = &self.text_editor else {
            return;
        };

        let asset_index = text_editor.asset_index;
        let entry_path = text_editor.entry_path.clone();
        let draft_text = text_editor.draft_text.clone();

        let Some(asset) = self.assets.get_mut(asset_index) else {
            return;
        };
        let Some(entry) = asset.find_archive_entry_mut(&entry_path) else {
            self.set_status("The selected XML entry is no longer available.");
            return;
        };
        let Some(source_path) = entry.text_source_path().map(Path::to_path_buf) else {
            self.set_status("This entry cannot be edited as XML.");
            return;
        };
        let original_text = entry.original_text().unwrap_or("").to_owned();

        if let Err(error) = fs::write(&source_path, &draft_text) {
            self.set_status(format!("Failed to save staged XML: {error}"));
            return;
        }

        entry.dirty = entry.added || draft_text != original_text;
        let entry_title = entry.title.clone();
        asset.sync_archive_dirty();
        if let Some(text_editor) = self.text_editor.as_mut() {
            text_editor.validation_message = Some("Staged XML saved for rebuild.".to_owned());
        }
        self.set_status(format!("Saved staged XML for {}", entry_title));
        self.show_toast(format!("Staged XML saved for {}.", entry_title));
        self.refresh_all();
    }

    fn selected_mod_directory(&self) -> Option<&Path> {
        self.current_mod_browser_path
            .as_deref()
            .or(self.selected_mod_path.as_deref())
            .filter(|path| path.is_dir())
    }

    fn select_mod_directory(&mut self, path: PathBuf) {
        self.browse_mod_directory(path);
    }

    fn browse_mod_directory(&mut self, path: PathBuf) {
        self.selected_mod_path = Some(path.clone());
        self.current_mod_browser_path = Some(path);
        self.selected_asset = None;
        self.selected_texture = None;
        self.editor = None;
        self.text_editor = None;
        self.widgets.stack.set_visible_child_name("browser");
        self.refresh_all();
    }

    fn mod_browser_parent_path(&self) -> Option<PathBuf> {
        let current = self.current_mod_browser_path.as_ref()?;
        let root = self.mods_root_path()?;
        if current == &root {
            return None;
        }

        let parent = current.parent()?;
        parent.starts_with(&root).then(|| parent.to_path_buf())
    }

    fn open_mod_browser_parent(&mut self) {
        if let Some(parent) = self.mod_browser_parent_path() {
            self.browse_mod_directory(parent);
        }
    }

    fn import_files_into_selected_mod_directory(&mut self, files: Vec<PathBuf>) {
        let Some(target_dir) = self.selected_mod_directory().map(Path::to_path_buf) else {
            self.set_status("Select a folder in the left pane first.");
            return;
        };
        if files.is_empty() {
            return;
        }

        match copy_files_into_directory(&files, &target_dir) {
            Ok(count) => {
                self.set_status(format!(
                    "Imported {count} file(s) into {}",
                    target_dir.display()
                ));
                self.show_toast(format!("Imported {count} file(s)."));
                self.refresh_package_tree();
            }
            Err(error) => {
                self.set_status(format!("Folder import failed: {error:#}"));
            }
        }
    }

    fn current_archive_target_directory(&self) -> Option<(usize, String)> {
        let asset_index = self.selected_asset?;
        let asset = self.assets.get(asset_index)?;
        if !asset.is_archive() {
            return None;
        }

        let current_path = asset
            .archive_current_path
            .clone()
            .or_else(|| asset.archive_root_path().map(ToOwned::to_owned))?;
        Some((asset_index, current_path))
    }

    fn add_archive_folder(&mut self, name: String) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.set_status("Enter a folder name first.");
            return;
        }
        if name.contains(['\\', '/']) {
            self.set_status("Folder names cannot contain path separators.");
            return;
        }

        let Some((asset_index, parent_path)) = self.current_archive_target_directory() else {
            self.set_status("Open a package folder first.");
            return;
        };

        let Some(asset) = self.assets.get_mut(asset_index) else {
            return;
        };

        let new_path = format!("{}\\{}", parent_path, name.to_ascii_lowercase());
        if asset.find_archive_node(&new_path).is_some() {
            self.set_status("An item with that name already exists in this folder.");
            return;
        }

        let Some(parent_node) = asset
            .archive_tree
            .as_mut()
            .and_then(|tree| tree.find_mut(&parent_path))
        else {
            self.set_status("The selected archive folder could not be found.");
            return;
        };

        let display_path = format!("{} / {}", parent_node.display_path, name);
        parent_node.children.push(RpfTreeNode {
            name: name.clone(),
            path: new_path.clone(),
            display_path,
            kind: RpfTreeNodeKind::Folder,
            content_kind: ArchiveContentKind::Folder,
            children: Vec::new(),
        });
        asset.pending_archive_folders.push(PendingArchiveFolder {
            path: new_path,
            name: name.clone(),
        });
        asset.archive_expanded_paths.insert(parent_path);
        asset.sync_archive_dirty();
        self.set_status(format!(
            "Added folder {} to the package staging area.",
            name
        ));
        self.refresh_all();
    }

    fn add_archive_file(
        &mut self,
        file_name: String,
        node_content_kind: ArchiveContentKind,
        staged_source_path: PathBuf,
    ) {
        let Some((asset_index, parent_path)) = self.current_archive_target_directory() else {
            self.set_status("Open a package folder first.");
            return;
        };

        let Some(asset) = self.assets.get_mut(asset_index) else {
            return;
        };

        let entry_path = format!("{}\\{}", parent_path, file_name.to_ascii_lowercase());
        if asset.find_archive_node(&entry_path).is_some() {
            self.set_status("An item with that name already exists in this folder.");
            return;
        }

        let entry = if node_content_kind == ArchiveContentKind::XmlText {
            let original_text = match fs::read_to_string(&staged_source_path) {
                Ok(text) => text,
                Err(error) => {
                    self.set_status(format!(
                        "Failed to read {} as UTF-8 XML text: {}",
                        staged_source_path.display(),
                        error
                    ));
                    return;
                }
            };
            ImportedArchiveEntry {
                entry_path: entry_path.clone(),
                title: file_name.clone(),
                content_kind: ArchiveContentKind::XmlText,
                data: ImportedArchiveEntryData::XmlText {
                    source_path: staged_source_path,
                    original_text,
                    source_kind: ArchiveTextSourceKind::RawText,
                },
                dirty: true,
                added: true,
            }
        } else {
            ImportedArchiveEntry {
                entry_path: entry_path.clone(),
                title: file_name.clone(),
                content_kind: ArchiveContentKind::File,
                data: ImportedArchiveEntryData::StagedRaw {
                    source_path: staged_source_path,
                },
                dirty: true,
                added: true,
            }
        };

        let Some(parent_node) = asset
            .archive_tree
            .as_mut()
            .and_then(|tree| tree.find_mut(&parent_path))
        else {
            self.set_status("The selected archive folder could not be found.");
            return;
        };

        let display_path = format!("{} / {}", parent_node.display_path, file_name);
        parent_node.children.push(RpfTreeNode {
            name: file_name.clone(),
            path: entry_path.clone(),
            display_path,
            kind: RpfTreeNodeKind::File,
            content_kind: node_content_kind,
            children: Vec::new(),
        });

        asset.archive_entries.push(entry);
        asset.archive_expanded_paths.insert(parent_path);
        asset.archive_selected_file = Some(entry_path);
        asset.archive_file_notice = None;
        asset.sync_archive_dirty();
        self.selected_texture = None;
        self.set_status(format!("Added {} to the package staging area.", file_name));
        self.refresh_all();
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

    fn queue_save_all_dirty_assets(&mut self) {
        let dirty_indices = self
            .assets
            .iter()
            .enumerate()
            .filter_map(|(index, asset)| asset.dirty.then_some(index))
            .collect::<Vec<_>>();

        if dirty_indices.is_empty() {
            self.set_status("There are no unsaved builds to save.");
            return;
        }

        for asset_index in dirty_indices {
            self.queue_save_asset_by_index(asset_index);
        }
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
        let archive_changes: Vec<_> = asset.build_archive_actions();

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
            let Some(texture) = entry
                .textures()
                .and_then(|textures| textures.get(editor.texture_index))
            else {
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
            SetupStep::GameFolder => {
                gtk::glib::idle_add_local_once(|| {
                    with_app_ref(|app_ref| {
                        present_mod_folder_dialog(&app_ref);
                    });
                });
            }
            SetupStep::VulkanRuntime => {
                if !self.setup_status.vulkan_runtime_ready {
                    self.queue_prepare_vulkan_runtime();
                }
            }
            SetupStep::Ready => {
                if !self.setup_status.setup_ready() || self.game_root_path().is_none() {
                    self.set_status("Setup is not complete yet.");
                    return;
                }
                self.config.setup_complete = true;
                self.config.setup_revision = CURRENT_SETUP_REVISION;
                self.persist_config();
                self.show_dashboard();
                self.refresh_all();
            }
            SetupStep::SystemDependencies => {}
        }
    }

    fn queue_prepare_vulkan_runtime(&mut self) {
        let tx = self.job_tx.clone();
        let workspace_dir = self.tool_paths.workspace_dir.clone();
        self.pending_jobs += 1;
        self.set_status("Caching the Vulkan runtime bundle...");

        thread::spawn(move || {
            let result = launcher::cache_vulkan_runtime(&workspace_dir)
                .map(|status| format!("Vulkan runtime {}.", status.as_label()))
                .map_err(|error| error.to_string());
            let _ = tx.send(JobResult::PrepareVulkanRuntimeFinished(result));
        });
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
            let app = Rc::clone(&app);
            gtk::glib::idle_add_local_once(move || {
                let mut app = app.borrow_mut();
                app.close_settings_panel();
                app.rerun_setup_wizard();
            });
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
            .import_to_mod_folder_button
            .connect_clicked(move |_| {
                present_mod_folder_import_dialog(&app);
            });
    }
    {
        let app = Rc::clone(app);
        widgets.dashboard_play_button.connect_clicked(move |_| {
            app.borrow_mut().queue_launch_game();
        });
    }
    {
        let app = Rc::clone(app);
        widgets.dashboard_editor_button.connect_clicked(move |_| {
            app.borrow_mut().open_editor_browser();
        });
    }
    {
        widgets
            .dashboard_launch_settings_button
            .connect_clicked(move |_| {
                with_app(|app| {
                    let reveal = !app
                        .widgets
                        .dashboard_launch_settings_revealer
                        .reveals_child();
                    app.widgets
                        .dashboard_launch_settings_revealer
                        .set_reveal_child(reveal);
                    app.config.play_settings_expanded = reveal;
                    app.persist_config();
                });
            });
    }
    {
        let app = Rc::clone(app);
        let toggle_syncing = widgets.dashboard_toggle_syncing.clone();
        widgets
            .dashboard_addons_toggle
            .connect_active_notify(move |toggle| {
                if toggle_syncing.get() {
                    return;
                }
                let enabled = toggle.is_active();
                let mut app = app.borrow_mut();
                if app.config.addons_enabled != enabled {
                    app.set_addons_enabled(enabled);
                }
            });
    }
    {
        let app = Rc::clone(app);
        let toggle_syncing = widgets.dashboard_toggle_syncing.clone();
        widgets
            .dashboard_scripts_toggle
            .connect_active_notify(move |toggle| {
                if toggle_syncing.get() {
                    return;
                }
                let enabled = toggle.is_active();
                let mut app = app.borrow_mut();
                if app.config.script_mods_enabled != enabled {
                    app.set_script_mods_enabled(enabled);
                }
            });
    }
    {
        let app = Rc::clone(app);
        widgets.save_builds_button.connect_clicked(move |_| {
            app.borrow_mut().queue_save_all_dirty_assets();
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
        widgets.archive_add_button.connect_clicked(move |_| {
            present_archive_add_dialog(&app);
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
            if app.in_editor() {
                app.close_editor_page();
            } else if app.current_page_name().as_deref() == Some("browser") {
                app.open_dashboard();
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
        widgets
            .text_editor_validate_button
            .connect_clicked(move |_| {
                app.borrow_mut().validate_text_editor();
            });
    }
    {
        let app = Rc::clone(app);
        widgets.text_editor_save_button.connect_clicked(move |_| {
            app.borrow_mut().save_text_editor();
        });
    }
    {
        let app = Rc::clone(app);
        let buffer_syncing = widgets.text_editor_buffer_syncing.clone();
        widgets
            .text_editor_edit_view
            .buffer()
            .connect_changed(move |_| {
                if buffer_syncing.get() {
                    return;
                }
                app.borrow_mut().sync_text_editor_from_buffer();
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
    let content_overlay = gtk::Overlay::new();
    content_overlay.set_hexpand(true);
    content_overlay.set_vexpand(true);

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

    let dashboard_page = gtk::Box::new(gtk::Orientation::Vertical, 20);
    dashboard_page.set_margin_top(28);
    dashboard_page.set_margin_bottom(28);
    dashboard_page.set_margin_start(28);
    dashboard_page.set_margin_end(28);
    let dashboard_title = gtk::Label::new(Some("GTA V Linux Dashboard"));
    dashboard_title.set_xalign(0.0);
    dashboard_title.add_css_class("title-1");
    let dashboard_subtitle = gtk::Label::new(Some(
        "Launch the Linux variant or open the editor for package and texture work.",
    ));
    dashboard_subtitle.set_xalign(0.0);
    dashboard_subtitle.set_wrap(true);
    dashboard_subtitle.add_css_class("dim-label");
    let dashboard_cards = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    dashboard_cards.set_hexpand(true);
    dashboard_cards.set_homogeneous(true);

    let play_card = build_panel_box("Play");
    play_card.set_hexpand(true);
    let dashboard_play_button = gtk::Button::new();
    dashboard_play_button.add_css_class("suggested-action");
    dashboard_play_button.set_hexpand(true);
    dashboard_play_button.set_vexpand(true);
    dashboard_play_button.set_height_request(240);
    let play_button_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    play_button_content.set_margin_top(28);
    play_button_content.set_margin_bottom(28);
    play_button_content.set_margin_start(24);
    play_button_content.set_margin_end(24);
    let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
    play_icon.set_pixel_size(48);
    let play_title = gtk::Label::new(Some("Play GTA V"));
    play_title.add_css_class("title-3");
    let play_body = gtk::Label::new(Some(
        "Prepare Wine, ensure runtime dependencies, and launch Story Mode.",
    ));
    play_body.set_wrap(true);
    play_body.set_xalign(0.5);
    play_button_content.append(&play_icon);
    play_button_content.append(&play_title);
    play_button_content.append(&play_body);
    dashboard_play_button.set_child(Some(&play_button_content));
    let dashboard_launch_settings_button = gtk::Button::from_icon_name("emblem-system-symbolic");
    dashboard_launch_settings_button.set_tooltip_text(Some("Play settings"));
    dashboard_launch_settings_button.add_css_class("flat");
    dashboard_launch_settings_button.set_halign(gtk::Align::Start);
    let dashboard_launch_settings_revealer = gtk::Revealer::new();
    dashboard_launch_settings_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    dashboard_launch_settings_revealer.set_reveal_child(config.play_settings_expanded);
    let launch_settings_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let dashboard_toggle_syncing = Rc::new(Cell::new(false));
    let dashboard_addons_toggle_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let dashboard_addons_label = gtk::Label::new(Some("Enable addons (mods folder)"));
    dashboard_addons_label.set_xalign(0.0);
    dashboard_addons_label.set_hexpand(true);
    let dashboard_addons_toggle = gtk::Switch::new();
    dashboard_addons_toggle_row.append(&dashboard_addons_label);
    dashboard_addons_toggle_row.append(&dashboard_addons_toggle);
    let dashboard_scripts_toggle_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let dashboard_scripts_label = gtk::Label::new(Some("Enable script mods (scripts folder)"));
    dashboard_scripts_label.set_xalign(0.0);
    dashboard_scripts_label.set_hexpand(true);
    let dashboard_scripts_toggle = gtk::Switch::new();
    dashboard_scripts_toggle_row.append(&dashboard_scripts_label);
    dashboard_scripts_toggle_row.append(&dashboard_scripts_toggle);
    let dashboard_notice_label = gtk::Label::new(None);
    dashboard_notice_label.set_xalign(0.0);
    dashboard_notice_label.set_wrap(true);
    dashboard_notice_label.add_css_class("caption");
    launch_settings_box.append(&dashboard_addons_toggle_row);
    launch_settings_box.append(&dashboard_scripts_toggle_row);
    launch_settings_box.append(&dashboard_notice_label);
    dashboard_launch_settings_revealer.set_child(Some(&launch_settings_box));
    play_card.append(&dashboard_play_button);
    play_card.append(&dashboard_launch_settings_button);
    play_card.append(&dashboard_launch_settings_revealer);

    let editor_card = build_panel_box("Editor");
    editor_card.set_hexpand(true);
    let dashboard_editor_button = gtk::Button::new();
    dashboard_editor_button.set_hexpand(true);
    dashboard_editor_button.set_vexpand(true);
    dashboard_editor_button.set_height_request(240);
    let editor_button_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    editor_button_content.set_margin_top(28);
    editor_button_content.set_margin_bottom(28);
    editor_button_content.set_margin_start(24);
    editor_button_content.set_margin_end(24);
    let editor_icon = gtk::Image::from_icon_name("document-edit-symbolic");
    editor_icon.set_pixel_size(48);
    let editor_title = gtk::Label::new(Some("Editor"));
    editor_title.add_css_class("title-3");
    let editor_body = gtk::Label::new(Some(
        "Open the current package and texture editor workflow.",
    ));
    editor_body.set_wrap(true);
    editor_body.set_xalign(0.5);
    editor_button_content.append(&editor_icon);
    editor_button_content.append(&editor_title);
    editor_button_content.append(&editor_body);
    dashboard_editor_button.set_child(Some(&editor_button_content));
    editor_card.append(&dashboard_editor_button);

    dashboard_cards.append(&play_card);
    dashboard_cards.append(&editor_card);
    dashboard_page.append(&dashboard_title);
    dashboard_page.append(&dashboard_subtitle);
    dashboard_page.append(&dashboard_cards);

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
    let mod_folder_label = gtk::Label::new(Some("GTA V game and mods folders"));
    mod_folder_label.set_xalign(0.0);
    let mod_folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let mod_folder_path_label = gtk::Label::new(Some("Not configured"));
    mod_folder_path_label.set_xalign(0.0);
    mod_folder_path_label.set_hexpand(true);
    mod_folder_path_label.set_wrap(true);
    mod_folder_path_label.add_css_class("caption");
    let open_mod_folder_button = gtk::Button::from_icon_name("folder-open-symbolic");
    open_mod_folder_button.add_css_class("flat");
    open_mod_folder_button.set_tooltip_text(Some("Open derived GTA V mods folder"));
    open_mod_folder_button.set_valign(gtk::Align::Start);
    let change_mod_folder_button = gtk::Button::with_label("Choose Game Folder");
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
    let package_target_label = gtk::Label::new(Some("GTA V game folder not configured"));
    package_target_label.set_xalign(0.0);
    package_target_label.set_wrap(true);
    package_target_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    let package_actions_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let import_to_mod_folder_button = gtk::Button::with_label("Import Into Folder");
    import_to_mod_folder_button.set_sensitive(false);
    let save_builds_button = gtk::Button::with_label("Save Builds");
    save_builds_button.set_sensitive(false);
    package_actions_row.append(&import_to_mod_folder_button);
    package_actions_row.append(&save_builds_button);
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
    packages_panel.append(&package_actions_row);
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
    let archive_add_button = gtk::Button::with_label("Add");
    archive_add_button.set_visible(false);
    let textures_notice_label = gtk::Label::new(None);
    textures_notice_label.set_xalign(0.0);
    textures_notice_label.set_wrap(true);
    textures_notice_label.add_css_class("caption");
    textures_back_button.set_visible(false);
    textures_nav_row.append(&textures_back_button);
    textures_nav_row.append(&textures_path_label);
    textures_nav_row.append(&archive_add_button);
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
    let preview_stack = gtk::Stack::new();
    preview_stack.set_hexpand(true);
    preview_stack.set_vexpand(true);
    let preview_picture = gtk::Picture::new();
    preview_picture.set_can_shrink(true);
    preview_picture.set_content_fit(gtk::ContentFit::Contain);
    preview_picture.set_vexpand(true);
    let preview_text_view = gtk::TextView::new();
    preview_text_view.set_editable(false);
    preview_text_view.set_cursor_visible(false);
    preview_text_view.set_monospace(true);
    let preview_text_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&preview_text_view)
        .build();
    preview_stack.add_named(&preview_picture, Some("image"));
    preview_stack.add_named(&preview_text_scroll, Some("text"));
    preview_stack.set_visible_child_name("image");
    let preview_notice_label = gtk::Label::new(None);
    preview_notice_label.set_xalign(0.0);
    preview_notice_label.add_css_class("caption");
    let edit_button = gtk::Button::with_label("Edit Texture");

    preview_panel.append(&preview_asset_label);
    preview_panel.append(&preview_texture_label);
    preview_panel.append(&preview_meta_label);
    preview_panel.append(&preview_notice_label);
    preview_panel.append(&preview_stack);
    preview_panel.append(&edit_button);

    main_paned.set_start_child(Some(&packages_panel));
    main_paned.set_end_child(Some(&right_paned));
    right_paned.set_start_child(Some(&textures_panel));
    right_paned.set_end_child(Some(&preview_panel));
    browser_overlay.set_child(Some(&main_paned));
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
    let editor_stack = gtk::Stack::new();
    editor_stack.set_hexpand(true);
    editor_stack.set_vexpand(true);
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

    let text_editor_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    text_editor_paned.set_wide_handle(true);
    text_editor_paned.set_position(460);
    let text_original_panel = build_panel_box("Original XML");
    let text_editor_original_view = gtk::TextView::new();
    text_editor_original_view.set_editable(false);
    text_editor_original_view.set_cursor_visible(false);
    text_editor_original_view.set_monospace(true);
    let text_original_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&text_editor_original_view)
        .build();
    text_original_panel.append(&text_original_scroll);

    let text_edit_panel = build_panel_box("Staged XML");
    let text_editor_edit_view = gtk::TextView::new();
    text_editor_edit_view.set_monospace(true);
    let text_editor_buffer_syncing = Rc::new(Cell::new(false));
    let text_edit_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&text_editor_edit_view)
        .build();
    text_edit_panel.append(&text_edit_scroll);
    let text_editor_notice_label = gtk::Label::new(None);
    text_editor_notice_label.set_xalign(0.0);
    text_editor_notice_label.add_css_class("caption");
    let text_editor_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let text_editor_validate_button = gtk::Button::with_label("Validate");
    let text_editor_save_button = gtk::Button::with_label("Save");
    text_editor_save_button.add_css_class("suggested-action");
    text_editor_actions.append(&text_editor_validate_button);
    text_editor_actions.append(&text_editor_save_button);
    text_edit_panel.append(&text_editor_notice_label);
    text_edit_panel.append(&text_editor_actions);

    text_editor_paned.set_start_child(Some(&text_original_panel));
    text_editor_paned.set_end_child(Some(&text_edit_panel));

    let texture_editor_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    texture_editor_box.append(&editor_notice_label);
    texture_editor_box.append(&editor_paned);
    texture_editor_box.append(&editor_apply_button);
    editor_stack.add_named(&texture_editor_box, Some("texture"));
    editor_stack.add_named(&text_editor_paned, Some("text"));
    editor_stack.set_visible_child_name("texture");

    editor_page.append(&editor_title_label);
    editor_page.append(&editor_meta_label);
    editor_page.append(&editor_stack);

    stack.add_named(&setup_page, Some("setup"));
    stack.add_named(&dashboard_page, Some("dashboard"));
    stack.add_named(&browser_page, Some("browser"));
    stack.add_named(&editor_page, Some("editor"));
    stack.set_visible_child_name("setup");

    content_overlay.set_child(Some(&stack));
    content_overlay.add_overlay(&settings_backdrop);
    content_overlay.add_overlay(&settings_revealer);

    let copy_destination_window: gtk::Window = window.clone().upcast();

    root.append(&content_overlay);
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
        dashboard_play_button,
        dashboard_editor_button,
        dashboard_launch_settings_button,
        dashboard_launch_settings_revealer,
        dashboard_addons_toggle,
        dashboard_scripts_toggle,
        dashboard_notice_label,
        dashboard_toggle_syncing,
        browser_main_paned: main_paned,
        packages_panel,
        package_target_label,
        import_to_mod_folder_button,
        save_builds_button,
        new_folder_entry,
        create_folder_button,
        import_here_button,
        move_here_button,
        package_list_box,
        textures_title_label,
        textures_search_entry,
        textures_path_label,
        textures_back_button,
        archive_add_button,
        textures_notice_label,
        texture_list_box,
        preview_asset_label,
        preview_texture_label,
        preview_meta_label,
        preview_stack,
        preview_picture,
        preview_text_view,
        preview_notice_label,
        edit_button,
        editor_title_label,
        editor_meta_label,
        editor_stack,
        editor_original_picture,
        editor_canvas_box,
        editor_notice_label,
        editor_apply_button,
        text_editor_original_view,
        text_editor_edit_view,
        text_editor_notice_label,
        text_editor_validate_button,
        text_editor_save_button,
        text_editor_buffer_syncing,
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
            let selected = app
                .current_mod_browser_path
                .as_ref()
                .or(app.selected_mod_path.as_ref())
                .is_some_and(|selected_path| selected_path == &path);
            let button = build_tree_icon_button(&label, "folder-symbolic", 0, selected);
            button.set_margin_start(0);
            let folder_path = path.clone();
            button.connect_clicked(move |_| {
                with_app(|app| {
                    app.select_mod_directory(folder_path.clone());
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

fn append_mod_browser_rows(app: &App, container: &gtk::Box, entries: &[PathBuf]) {
    for path in entries {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
        row.set_margin_start(4);
        row.set_margin_end(4);

        if path.is_dir() {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let selected = app
                .current_mod_browser_path
                .as_ref()
                .is_some_and(|selected_path| selected_path == path);
            let button = build_tree_icon_button(&label, "folder-symbolic", 0, selected);
            button.set_margin_start(0);
            let folder_path = path.clone();
            button.connect_clicked(move |_| {
                with_app(|app| {
                    app.browse_mod_directory(folder_path.clone());
                });
            });
            row.append(&button);
            container.append(&row);
            continue;
        }

        let Some(kind) = AssetKind::from_path(path) else {
            continue;
        };

        let asset_index = app.asset_index_for_source_path(path);
        let asset = asset_index.and_then(|index| app.assets.get(index));
        let pending = app.pending_import_paths.contains(path);
        let selected = asset_index.is_some_and(|index| app.selected_asset == Some(index))
            || app
                .selected_mod_path
                .as_ref()
                .is_some_and(|selected_path| selected_path == path);

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

fn build_tree_icon_button(label: &str, icon_name: &str, depth: i32, selected: bool) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button.set_margin_start((depth * 18).max(0));
    button.set_margin_top(2);
    button.set_margin_bottom(2);
    button.set_tooltip_text(Some(label));

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(16);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_single_line_mode(true);
    text.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.set_hexpand(true);
    row.append(&icon);
    row.append(&text);
    button.set_child(Some(&row));

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
    selected_file_path: Option<&str>,
    current_directory_path: Option<&str>,
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

        let selected = selected_file_path.is_some_and(|path| path == child.path)
            || selected_file_path.is_none()
                && current_directory_path.is_some_and(|path| path == child.path);
        let button =
            build_tree_icon_button(&child.name, archive_icon_name_for_node(child), 0, selected);
        button.set_margin_start(0);
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
                selected_file_path,
                current_directory_path,
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

fn archive_icon_name_for_node(node: &RpfTreeNode) -> &'static str {
    match node.content_kind {
        ArchiveContentKind::Folder => "folder-symbolic",
        ArchiveContentKind::Package => "package-x-generic-symbolic",
        ArchiveContentKind::TextureAsset => "image-x-generic-symbolic",
        ArchiveContentKind::XmlText
        | ArchiveContentKind::ConvertedXml
        | ArchiveContentKind::File => "text-x-generic-symbolic",
    }
}

fn archive_parent_path_from_entry_path(path: &str) -> Option<String> {
    let (parent, _) = path.rsplit_once('\\')?;
    Some(parent.to_owned())
}

fn archive_content_kind_for_extension(extension: &str) -> ArchiveContentKind {
    match extension.to_ascii_lowercase().as_str() {
        "rpf" => ArchiveContentKind::Package,
        "ydr" | "yft" | "ytd" => ArchiveContentKind::TextureAsset,
        "xml" | "meta" => ArchiveContentKind::XmlText,
        _ => ArchiveContentKind::File,
    }
}

fn copy_files_into_directory(files: &[PathBuf], target_dir: &Path) -> Result<usize> {
    fs::create_dir_all(target_dir)?;
    for source in files {
        let file_name = source
            .file_name()
            .context("Source file does not contain a file name")?;
        fs::copy(source, target_dir.join(file_name)).with_context(|| {
            format!(
                "Failed to copy {} into {}",
                source.display(),
                target_dir.display()
            )
        })?;
    }
    Ok(files.len())
}

fn stage_archive_source_file(
    tool_paths: &ToolPaths,
    file_name: &str,
    source_path: Option<&Path>,
    initial_text: Option<&str>,
) -> Result<PathBuf> {
    let session_dir = tool_paths.workspace_dir.join("imports").join(format!(
        "archive_stage_{}_{}",
        sanitize_for_path(file_name),
        unix_timestamp_ms()
    ));
    fs::create_dir_all(&session_dir)?;
    let staged_path = session_dir.join(file_name);
    if let Some(source_path) = source_path {
        fs::copy(source_path, &staged_path).with_context(|| {
            format!(
                "Failed to stage {} into {}",
                source_path.display(),
                staged_path.display()
            )
        })?;
    } else {
        fs::write(&staged_path, initial_text.unwrap_or_default())
            .with_context(|| format!("Failed to create staged file {}", staged_path.display()))?;
    }
    Ok(staged_path)
}

fn text_buffer_text(buffer: &gtk::TextBuffer) -> String {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    buffer.text(&start, &end, true).to_string()
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

fn present_mod_folder_import_dialog(app: &Rc<RefCell<App>>) {
    let app_borrow = app.borrow();
    let Some(target_dir) = app_borrow.selected_mod_directory().map(Path::to_path_buf) else {
        drop(app_borrow);
        app.borrow_mut()
            .set_status("Select a folder in the left pane first.");
        return;
    };
    let dialog = gtk::FileDialog::builder()
        .title("Import files into selected mods folder")
        .modal(true)
        .build();

    let filters = gio::ListStore::new::<gtk::FileFilter>();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("GTA V assets"));
    for suffix in ["ydr", "yft", "ytd", "rpf"] {
        filter.add_suffix(suffix);
    }
    filters.append(&filter);
    dialog.set_filters(Some(&filters));

    if target_dir.exists() {
        dialog.set_initial_folder(Some(&gio::File::for_path(&target_dir)));
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

            app_ref
                .borrow_mut()
                .import_files_into_selected_mod_directory(paths);
        }
    });
}

fn present_archive_add_dialog(app: &Rc<RefCell<App>>) {
    let app_borrow = app.borrow();
    let parent = app_borrow.widgets.window.clone();
    let initial_dir = app_borrow.last_asset_dir.clone();
    drop(app_borrow);

    let dialog = gtk::Window::builder()
        .transient_for(&parent)
        .modal(true)
        .title("Add to Package")
        .default_width(420)
        .resizable(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_spacing(10);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let item_kind_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let item_kind_label = gtk::Label::new(Some("Add"));
    item_kind_label.set_xalign(0.0);
    let item_kind_dropdown = gtk::DropDown::from_strings(&["Folder", "File"]);
    item_kind_row.append(&item_kind_label);
    item_kind_row.append(&item_kind_dropdown);

    let file_type_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let file_type_label = gtk::Label::new(Some("File Type"));
    file_type_label.set_xalign(0.0);
    let file_type_dropdown =
        gtk::DropDown::from_strings(&["XML", "META", "YDR", "YFT", "YTD", "RPF"]);
    file_type_row.append(&file_type_label);
    file_type_row.append(&file_type_dropdown);

    let source_mode_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let source_mode_label = gtk::Label::new(Some("Source"));
    source_mode_label.set_xalign(0.0);
    let source_mode_dropdown = gtk::DropDown::from_strings(&["Empty", "Import from disk"]);
    source_mode_row.append(&source_mode_label);
    source_mode_row.append(&source_mode_dropdown);

    let name_label = gtk::Label::new(Some("Name (without extension)"));
    name_label.set_xalign(0.0);
    let name_entry = gtk::Entry::new();
    name_entry.set_hexpand(true);

    let import_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let import_button = gtk::Button::with_label("Import File");
    let import_path_label = gtk::Label::new(Some("No file selected"));
    import_path_label.set_xalign(0.0);
    import_path_label.set_wrap(true);
    import_path_label.set_hexpand(true);
    import_row.append(&import_button);
    import_row.append(&import_path_label);

    content.append(&item_kind_row);
    content.append(&file_type_row);
    content.append(&source_mode_row);
    content.append(&name_label);
    content.append(&name_entry);
    content.append(&import_row);

    let actions_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions_row.set_halign(gtk::Align::End);
    let cancel_button = gtk::Button::with_label("Cancel");
    let add_button = gtk::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    actions_row.append(&cancel_button);
    actions_row.append(&add_button);
    content.append(&actions_row);
    dialog.set_child(Some(&content));

    let selected_import_path = Rc::new(RefCell::new(None::<PathBuf>));
    let update_visibility = Rc::new({
        let item_kind_dropdown = item_kind_dropdown.clone();
        let source_mode_dropdown = source_mode_dropdown.clone();
        let file_type_row = file_type_row.clone();
        let source_mode_row = source_mode_row.clone();
        let import_row = import_row.clone();
        move || {
            let is_file = item_kind_dropdown.selected() == 1;
            file_type_row.set_visible(is_file);
            source_mode_row.set_visible(is_file);
            import_row.set_visible(is_file && source_mode_dropdown.selected() == 1);
        }
    });
    update_visibility();

    {
        let update_visibility = Rc::clone(&update_visibility);
        item_kind_dropdown.connect_selected_notify(move |_| update_visibility());
    }
    {
        let update_visibility = Rc::clone(&update_visibility);
        source_mode_dropdown.connect_selected_notify(move |_| update_visibility());
    }
    {
        let selected_import_path = Rc::clone(&selected_import_path);
        let import_path_label = import_path_label.clone();
        let parent = parent.clone();
        import_button.connect_clicked(move |_| {
            let file_dialog = gtk::FileDialog::builder()
                .title("Choose file to add")
                .modal(true)
                .build();
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Supported files"));
            for suffix in ["xml", "meta", "ydr", "yft", "ytd", "rpf"] {
                filter.add_suffix(suffix);
            }
            filters.append(&filter);
            file_dialog.set_filters(Some(&filters));
            if let Some(dir) = initial_dir.as_ref().filter(|dir| dir.exists()) {
                file_dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
            }
            let selected_import_path = Rc::clone(&selected_import_path);
            let import_path_label = import_path_label.clone();
            file_dialog.open(Some(&parent), None::<&gio::Cancellable>, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        import_path_label.set_text(&path.display().to_string());
                        *selected_import_path.borrow_mut() = Some(path);
                    }
                }
            });
        });
    }

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| {
            dialog.close();
        });
    }

    let app_ref = Rc::clone(app);
    {
        let dialog = dialog.clone();
        let selected_import_path = Rc::clone(&selected_import_path);
        add_button.connect_clicked(move |_| {
            let name = name_entry.text().trim().to_owned();
            if name.is_empty() {
                app_ref.borrow_mut().set_status("Enter a name first.");
                return;
            }
            if name.contains('.') {
                app_ref
                    .borrow_mut()
                    .set_status("Set the file name without an extension.");
                return;
            }

            if item_kind_dropdown.selected() == 0 {
                app_ref.borrow_mut().add_archive_folder(name);
                dialog.close();
                return;
            }

            let extension = match file_type_dropdown.selected() {
                1 => "meta",
                2 => "ydr",
                3 => "yft",
                4 => "ytd",
                5 => "rpf",
                _ => "xml",
            };
            let file_name = format!("{}.{}", name, extension);
            let source_mode = source_mode_dropdown.selected();

            let mut app = app_ref.borrow_mut();
            let staged_path = if source_mode == 0 {
                if !matches!(extension, "xml" | "meta") {
                    app.set_status("Empty files are only supported for XML and META.");
                    return;
                }
                match stage_archive_source_file(&app.tool_paths, &file_name, None, Some("")) {
                    Ok(path) => path,
                    Err(error) => {
                        app.set_status(format!("Failed to create staged file: {error:#}"));
                        return;
                    }
                }
            } else {
                let Some(source_path) = selected_import_path.borrow().clone() else {
                    app.set_status("Choose a file to import first.");
                    return;
                };
                app.last_asset_dir = source_path.parent().map(Path::to_path_buf);
                app.config.last_asset_dir = app.last_asset_dir.clone();
                app.persist_config();
                match stage_archive_source_file(
                    &app.tool_paths,
                    &file_name,
                    Some(&source_path),
                    None,
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        app.set_status(format!("Failed to stage imported file: {error:#}"));
                        return;
                    }
                }
            };

            let content_kind = archive_content_kind_for_extension(extension);
            app.add_archive_file(file_name, content_kind, staged_path);
            dialog.close();
        });
    }

    dialog.present();
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
        .title("Choose GTA V game folder")
        .modal(true)
        .build();

    if let Some(dir) = app_borrow
        .config
        .game_root_path
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
                app.set_game_root_path(path);
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

fn export_archive_text_draft(
    tool_paths: &ToolPaths,
    rpf_path: &Path,
    entry_path: &str,
) -> Result<ImportedArchiveTextDraft> {
    let entry_name = entry_path
        .split('\\')
        .next_back()
        .context("Archive entry path did not contain a file name")?
        .to_owned();
    let session_id = format!(
        "{}_{}",
        sanitize_for_path(entry_name.trim_end_matches(['.', ' '])),
        unix_timestamp_ms()
    );
    let session_dir = tool_paths.workspace_dir.join("imports").join(&session_id);
    fs::create_dir_all(&session_dir)?;
    let source_path = session_dir.join(&entry_name);

    let output = Command::new(&tool_paths.cwassettool_bin)
        .arg("export-rpf-raw-entry")
        .arg(rpf_path)
        .arg(entry_path)
        .arg(&source_path)
        .output()
        .with_context(|| {
            format!(
                "Failed to export {} from {}",
                entry_path,
                rpf_path.display()
            )
        })?;
    ensure_success("cwassettool export-rpf-raw-entry", output)?;

    let original_text = fs::read_to_string(&source_path)
        .with_context(|| format!("Failed to read {} as UTF-8 text", source_path.display()))?;

    Ok(ImportedArchiveTextDraft {
        entry_path: entry_path.to_owned(),
        title: entry_name,
        source_path,
        original_text,
        source_kind: ArchiveTextSourceKind::RawText,
    })
}

fn export_archive_ymt_draft(
    tool_paths: &ToolPaths,
    rpf_path: &Path,
    entry_path: &str,
) -> Result<ImportedArchiveTextDraft> {
    let entry_name = entry_path
        .split('\\')
        .next_back()
        .context("Archive entry path did not contain a file name")?
        .to_owned();
    let session_id = format!(
        "{}_{}",
        sanitize_for_path(entry_name.trim_end_matches(['.', ' '])),
        unix_timestamp_ms()
    );
    let session_dir = tool_paths.workspace_dir.join("imports").join(&session_id);
    fs::create_dir_all(&session_dir)?;

    let output = Command::new(&tool_paths.cwassettool_bin)
        .arg("export-rpf-ymt-entry")
        .arg(rpf_path)
        .arg(entry_path)
        .arg(&session_dir)
        .output()
        .with_context(|| {
            format!(
                "Failed to export {} from {}",
                entry_path,
                rpf_path.display()
            )
        })?;
    ensure_success("cwassettool export-rpf-ymt-entry", output.clone())?;

    let stdout = String::from_utf8(output.stdout).context("Invalid UTF-8 from cwassettool")?;
    let source_path = parse_named_output_path(&stdout, "xml")?;
    let original_text = fs::read_to_string(&source_path)
        .with_context(|| format!("Failed to read {} as UTF-8 text", source_path.display()))?;
    let resolved_text = apply_known_hash_name_overrides(&original_text)?;
    if resolved_text != original_text {
        fs::write(&source_path, &resolved_text)
            .with_context(|| format!("Failed to update {}", source_path.display()))?;
    }

    Ok(ImportedArchiveTextDraft {
        entry_path: entry_path.to_owned(),
        title: entry_name,
        source_path,
        original_text: resolved_text,
        source_kind: ArchiveTextSourceKind::YmtXml,
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
    actions: Vec<RpfBuildAction>,
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
        serde_json::to_vec_pretty(&RpfBuildManifest { actions })?,
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
    actions: Vec<RpfBuildAction>,
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
        save_rpf_build_job(tool_paths, source_path, &temp_output_path, actions)?;
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

fn parse_named_output_path(stdout: &str, key: &str) -> Result<PathBuf> {
    let prefix = format!("{key}=");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(PathBuf::from)
        .with_context(|| format!("Expected {key}=... in helper output"))
}

fn load_known_hash_name_overrides() -> Result<HashMap<u32, String>> {
    let content = fs::read_to_string(asset_icon_path("hash_name_seeds.txt"))?;
    let mut overrides = HashMap::new();
    for line in content.lines() {
        let name = line.trim();
        if name.is_empty() || name.starts_with('#') {
            continue;
        }
        overrides.insert(jenk_hash(name), name.to_owned());
    }
    Ok(overrides)
}

fn apply_known_hash_name_overrides(text: &str) -> Result<String> {
    let overrides = load_known_hash_name_overrides()?;
    Ok(replace_hash_placeholders(text, &overrides))
}

fn replace_hash_placeholders(text: &str, overrides: &HashMap<u32, String>) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index..].starts_with(b"hash_") && index + 13 <= bytes.len() {
            let candidate = &text[index + 5..index + 13];
            if candidate.chars().all(|ch| ch.is_ascii_hexdigit()) {
                if let Ok(hash) = u32::from_str_radix(candidate, 16) {
                    if let Some(name) = overrides.get(&hash) {
                        output.push_str(name);
                        index += 13;
                        continue;
                    }
                }
            }
        }

        let ch = text[index..].chars().next().unwrap();
        output.push(ch);
        index += ch.len_utf8();
    }

    output
}

fn jenk_hash(value: &str) -> u32 {
    let mut hash = 0u32;
    for byte in value.to_ascii_lowercase().bytes() {
        hash = hash.wrapping_add(byte as u32);
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}

fn normalize_selected_game_root(path: &Path) -> (PathBuf, bool) {
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("mods"))
    {
        if let Some(parent) = path.parent() {
            if is_valid_game_root(parent) {
                return (parent.to_path_buf(), true);
            }
        }
    }

    (path.to_path_buf(), false)
}

fn is_valid_game_root(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }

    fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .any(|name| {
            name.eq_ignore_ascii_case("gta5.exe") || name.eq_ignore_ascii_case("gta5_enhanced.exe")
        })
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
