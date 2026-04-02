using System.Text.Json;
using CodeWalker.GameFiles;
using CodeWalker.Utils;

var jsonOptions = new JsonSerializerOptions
{
    PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
    WriteIndented = true,
};

static int Usage()
{
    Console.Error.WriteLine("Usage:");
    Console.Error.WriteLine("  cwassettool inspect <asset>");
    Console.Error.WriteLine("  cwassettool inspect-dds <dds>");
    Console.Error.WriteLine("  cwassettool export <asset> <output-dir>");
    Console.Error.WriteLine("  cwassettool import <xml> <output-asset>");
    Console.Error.WriteLine("  cwassettool list-rpf <archive.rpf>");
    Console.Error.WriteLine("  cwassettool export-rpf-entry <archive.rpf> <entry-path> <output-dir>");
    Console.Error.WriteLine("  cwassettool build-rpf <source.rpf> <output.rpf> <changes.json>");
    return 1;
}

static string FullPath(string path) => Path.GetFullPath(path);

static (string RootPath, bool Gen9)? FindGameRoot(string path)
{
    var current = new FileInfo(path).Directory;
    while (current != null)
    {
        if (Directory.EnumerateFiles(current.FullName)
            .Select(Path.GetFileName)
            .Any(name => string.Equals(name, "gta5.exe", StringComparison.OrdinalIgnoreCase)))
        {
            return (current.FullName, false);
        }
        if (Directory.EnumerateFiles(current.FullName)
            .Select(Path.GetFileName)
            .Any(name => string.Equals(name, "gta5_enhanced.exe", StringComparison.OrdinalIgnoreCase)))
        {
            return (current.FullName, true);
        }
        current = current.Parent;
    }

    return null;
}

static void EnsureGameKeys(string path)
{
    var gameRoot = FindGameRoot(path);
    if (gameRoot == null)
    {
        return;
    }

    if (string.Equals(KeyLoadState.LoadedKeysRoot, gameRoot.Value.RootPath, StringComparison.OrdinalIgnoreCase))
    {
        return;
    }

    GTA5Keys.LoadFromPath(gameRoot.Value.RootPath, gameRoot.Value.Gen9, null);
    KeyLoadState.LoadedKeysRoot = gameRoot.Value.RootPath;
}

static bool IsSupportedAssetExtension(string extension) => extension switch
{
    ".ydr" or ".yft" or ".ytd" => true,
    _ => false,
};

static IEnumerable<Texture> EnumerateTextures(string assetPath)
{
    var data = File.ReadAllBytes(assetPath);
    var ext = Path.GetExtension(assetPath).ToLowerInvariant();

    return ext switch
    {
        ".ydr" => EnumerateYdr(data),
        ".yft" => EnumerateYft(data),
        ".ytd" => EnumerateYtd(data),
        _ => throw new InvalidOperationException($"Unsupported asset type: {ext}")
    };
}

static IEnumerable<Texture> EnumerateYdr(byte[] data)
{
    var ydr = new YdrFile();
    ydr.Load(data);
    return ydr.Drawable?.ShaderGroup?.TextureDictionary?.Textures?.data_items ?? Array.Empty<Texture>();
}

static IEnumerable<Texture> EnumerateYft(byte[] data)
{
    var yft = new YftFile();
    yft.Load(data);
    var list = new List<Texture>();
    if (yft.Fragment?.Drawable?.ShaderGroup?.TextureDictionary?.Textures?.data_items != null)
    {
        list.AddRange(yft.Fragment.Drawable.ShaderGroup.TextureDictionary.Textures.data_items);
    }
    if (yft.Fragment?.DrawableCloth?.ShaderGroup?.TextureDictionary?.Textures?.data_items != null)
    {
        list.AddRange(yft.Fragment.DrawableCloth.ShaderGroup.TextureDictionary.Textures.data_items);
    }
    return list;
}

static IEnumerable<Texture> EnumerateYtd(byte[] data)
{
    var ytd = new YtdFile();
    ytd.Load(data);
    return ytd.TextureDict?.Textures?.data_items ?? Array.Empty<Texture>();
}

static int InspectAsset(string assetPath)
{
    var textures = EnumerateTextures(assetPath).ToList();
    Console.WriteLine($"asset={assetPath}");
    Console.WriteLine($"textures={textures.Count}");
    foreach (var texture in textures)
    {
        Console.WriteLine(
            $"name={texture.Name}\twidth={texture.Width}\theight={texture.Height}\tmips={texture.Levels}\tformat={texture.Format}\tstride={texture.Stride}");
    }
    return 0;
}

static int InspectDds(string ddsPath)
{
    var texture = DDSIO.GetTexture(File.ReadAllBytes(ddsPath))
        ?? throw new InvalidOperationException("Unable to parse DDS file.");
    Console.WriteLine($"dds={ddsPath}");
    Console.WriteLine($"name={texture.Name}");
    Console.WriteLine($"width={texture.Width}");
    Console.WriteLine($"height={texture.Height}");
    Console.WriteLine($"mips={texture.Levels}");
    Console.WriteLine($"format={texture.Format}");
    Console.WriteLine($"stride={texture.Stride}");
    return 0;
}

static string ExportAssetBytes(string assetName, string ext, byte[] data, string outputDir)
{
    Directory.CreateDirectory(outputDir);
    var xmlPath = Path.Combine(outputDir, assetName + ".xml");

    string xml = ext switch
    {
        ".ydr" => ExportYdr(data, outputDir),
        ".yft" => ExportYft(data, outputDir),
        ".ytd" => ExportYtd(data, outputDir),
        _ => throw new InvalidOperationException($"Unsupported asset type: {ext}")
    };

    File.WriteAllText(xmlPath, xml);
    return xmlPath;
}

static int ExportAsset(string assetPath, string outputDir)
{
    var data = File.ReadAllBytes(assetPath);
    var ext = Path.GetExtension(assetPath).ToLowerInvariant();
    var xmlPath = ExportAssetBytes(Path.GetFileName(assetPath), ext, data, outputDir);
    Console.WriteLine($"xml={xmlPath}");
    return 0;
}

static string ExportYdr(byte[] data, string outputDir)
{
    var ydr = new YdrFile();
    ydr.Load(data);
    return YdrXml.GetXml(ydr, outputDir);
}

static string ExportYft(byte[] data, string outputDir)
{
    var yft = new YftFile();
    yft.Load(data);
    return YftXml.GetXml(yft, outputDir);
}

static string ExportYtd(byte[] data, string outputDir)
{
    var ytd = new YtdFile();
    ytd.Load(data);
    return YtdXml.GetXml(ytd, outputDir);
}

static byte[] ImportAssetBytes(string xmlPath, string ext)
{
    var xml = File.ReadAllText(xmlPath);
    var inputDir = Path.GetDirectoryName(xmlPath) ?? Environment.CurrentDirectory;

    return ext switch
    {
        ".ydr" => XmlYdr.GetYdr(xml, inputDir).Save(),
        ".yft" => XmlYft.GetYft(xml, inputDir).Save(),
        ".ytd" => XmlYtd.GetYtd(xml, inputDir).Save(),
        _ => throw new InvalidOperationException($"Unsupported asset type: {ext}")
    };
}

static int ImportAsset(string xmlPath, string outputAssetPath)
{
    var ext = Path.GetExtension(outputAssetPath).ToLowerInvariant();
    var data = ImportAssetBytes(xmlPath, ext);
    File.WriteAllBytes(outputAssetPath, data);
    Console.WriteLine($"asset={outputAssetPath}");
    return 0;
}

static RpfFile LoadRpf(string rpfPath)
{
    EnsureGameKeys(rpfPath);
    var file = new RpfFile(rpfPath, Path.GetFileName(rpfPath));
    var loadErrors = new List<string>();
    file.ScanStructure(null, message => loadErrors.Add(message));
    if (file.LastException != null)
    {
        var detail = loadErrors.LastOrDefault()
            ?? file.LastError
            ?? file.LastException.Message;
        throw new InvalidOperationException(
            $"Failed to scan RPF {rpfPath}: {detail}",
            file.LastException
        );
    }
    if (file.Root == null)
    {
        throw new InvalidOperationException($"RPF {rpfPath} did not expose a root directory.");
    }
    return file;
}

static RpfManager CreateRpfManager(RpfFile rootFile)
{
    var allRpfs = new List<RpfFile>();
    var stack = new Stack<RpfFile>();
    stack.Push(rootFile);

    while (stack.Count > 0)
    {
        var file = stack.Pop();
        allRpfs.Add(file);

        if (file.Children == null)
        {
            continue;
        }

        for (var index = file.Children.Count - 1; index >= 0; index--)
        {
            stack.Push(file.Children[index]);
        }
    }

    var manager = new RpfManager();
    manager.Init(allRpfs, false);
    return manager;
}

static List<RpfTreeNode> BuildRpfChildren(RpfDirectoryEntry directory, RpfFile archive, string parentDisplayPath)
{
    if (directory == null)
    {
        throw new InvalidOperationException($"RPF directory tree was missing for {parentDisplayPath}.");
    }

    var children = new List<RpfTreeNode>();

    foreach (var childDirectory in directory.Directories.OrderBy(dir => dir.Name, StringComparer.OrdinalIgnoreCase))
    {
        var childDisplayPath = parentDisplayPath + " / " + childDirectory.Name;
        children.Add(new RpfTreeNode(
            childDirectory.Name,
            childDirectory.Path,
            childDisplayPath,
            "folder",
            false,
            BuildRpfChildren(childDirectory, archive, childDisplayPath)
        ));
    }

    foreach (var childFile in directory.Files.OrderBy(file => file.Name, StringComparer.OrdinalIgnoreCase))
    {
        var childFileName = childFile.Name
            ?? childFile.Path?.Split('\\').LastOrDefault()
            ?? "(unnamed file)";
        var childFilePath = childFile.Path ?? string.Empty;
        var childArchive = archive.Children?.FirstOrDefault(candidate => candidate.ParentFileEntry?.Path == childFilePath);
        var childDisplayPath = parentDisplayPath + " / " + childFileName;
        if (childArchive != null)
        {
            children.Add(BuildRpfNode(childArchive, childDisplayPath));
        }
        else
        {
            children.Add(new RpfTreeNode(
                childFileName,
                childFilePath,
                childDisplayPath,
                "file",
                IsSupportedAssetExtension((Path.GetExtension(childFileName) ?? string.Empty).ToLowerInvariant()),
                new List<RpfTreeNode>()
            ));
        }
    }

    return children;
}

static RpfTreeNode BuildRpfNode(RpfFile archive, string displayPath)
{
    if (archive.Root == null)
    {
        throw new InvalidOperationException($"RPF archive {archive.FilePath} has no root directory.");
    }

    return new RpfTreeNode(
        archive.Name ?? Path.GetFileName(archive.FilePath) ?? "(archive)",
        archive.Path ?? archive.FilePath ?? string.Empty,
        displayPath,
        "package",
        false,
        BuildRpfChildren(archive.Root, archive, displayPath)
    );
}

static int ListRpf(string rpfPath, JsonSerializerOptions options)
{
    var root = LoadRpf(rpfPath);
    var tree = BuildRpfNode(root, root.Name);
    Console.WriteLine(JsonSerializer.Serialize(tree, options));
    return 0;
}

static int ExportRpfEntry(string rpfPath, string entryPath, string outputDir)
{
    var root = LoadRpf(rpfPath);
    var manager = CreateRpfManager(root);
    var entry = manager.GetEntry(entryPath) as RpfFileEntry
        ?? throw new InvalidOperationException($"RPF entry not found: {entryPath}");
    var ext = Path.GetExtension(entry.Name).ToLowerInvariant();
    if (!IsSupportedAssetExtension(ext))
    {
        throw new InvalidOperationException($"Unsupported archive entry type: {ext}");
    }

    var data = GetRpfEntryExportData(entry);
    var xmlPath = ExportAssetBytes(entry.Name, ext, data, outputDir);
    Console.WriteLine($"xml={xmlPath}");
    return 0;
}

static byte[] GetRpfEntryExportData(RpfFileEntry entry)
{
    var data = entry.File.ExtractFile(entry)
        ?? throw new InvalidOperationException($"Unable to extract archive entry: {entry.Path}");

    if (entry is RpfResourceFileEntry resourceEntry)
    {
        data = ResourceBuilder.Compress(data);
        data = ResourceBuilder.AddResourceHeader(resourceEntry, data);
    }

    return data;
}

static int BuildRpf(string sourceRpfPath, string outputRpfPath, string manifestPath, JsonSerializerOptions options)
{
    var manifest = JsonSerializer.Deserialize<RpfBuildManifest>(File.ReadAllText(manifestPath), options)
        ?? throw new InvalidOperationException("Unable to parse build manifest.");

    Directory.CreateDirectory(Path.GetDirectoryName(outputRpfPath) ?? Environment.CurrentDirectory);
    if (File.Exists(outputRpfPath))
    {
        File.Delete(outputRpfPath);
    }
    File.Copy(sourceRpfPath, outputRpfPath);

    foreach (var change in manifest.Changes)
    {
        var root = LoadRpf(outputRpfPath);
        RpfFile.EnsureValidEncryption(root, _ => true, true);
        var manager = CreateRpfManager(root);
        var entry = manager.GetEntry(change.EntryPath) as RpfFileEntry
            ?? throw new InvalidOperationException($"RPF entry not found: {change.EntryPath}");

        if (entry.Parent == null)
        {
            throw new InvalidOperationException($"RPF entry has no parent directory: {change.EntryPath}");
        }

        var ext = Path.GetExtension(entry.Name).ToLowerInvariant();
        var data = ImportAssetBytes(change.XmlPath, ext);
        RpfFile.CreateFile(entry.Parent, entry.Name, data);
    }

    Console.WriteLine($"asset={outputRpfPath}");
    return 0;
}

try
{
    RpfManager.IsGen9 = false;

    if (args.Length == 0)
    {
        return Usage();
    }

    return args[0] switch
    {
        "inspect" when args.Length == 2 => InspectAsset(FullPath(args[1])),
        "inspect-dds" when args.Length == 2 => InspectDds(FullPath(args[1])),
        "export" when args.Length == 3 => ExportAsset(FullPath(args[1]), FullPath(args[2])),
        "import" when args.Length == 3 => ImportAsset(FullPath(args[1]), FullPath(args[2])),
        "list-rpf" when args.Length == 2 => ListRpf(FullPath(args[1]), jsonOptions),
        "export-rpf-entry" when args.Length == 4 => ExportRpfEntry(FullPath(args[1]), args[2], FullPath(args[3])),
        "build-rpf" when args.Length == 4 => BuildRpf(FullPath(args[1]), FullPath(args[2]), FullPath(args[3]), jsonOptions),
        _ => Usage()
    };
}
catch (Exception ex)
{
    Console.Error.WriteLine(ex.Message);
    return 1;
}

record RpfTreeNode(
    string Name,
    string Path,
    string DisplayPath,
    string Kind,
    bool SupportedAsset,
    List<RpfTreeNode> Children
);

record RpfBuildManifest(List<RpfBuildChange> Changes);

record RpfBuildChange(string EntryPath, string XmlPath);

static class KeyLoadState
{
    public static string LoadedKeysRoot = string.Empty;
}
