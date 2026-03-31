using CodeWalker.GameFiles;
using CodeWalker.Utils;

static int Usage()
{
    Console.Error.WriteLine("Usage:");
    Console.Error.WriteLine("  cwassettool inspect <asset>");
    Console.Error.WriteLine("  cwassettool inspect-dds <dds>");
    Console.Error.WriteLine("  cwassettool export <asset> <output-dir>");
    Console.Error.WriteLine("  cwassettool import <xml> <output-asset>");
    return 1;
}

static string FullPath(string path) => Path.GetFullPath(path);

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

static int ExportAsset(string assetPath, string outputDir)
{
    Directory.CreateDirectory(outputDir);

    var data = File.ReadAllBytes(assetPath);
    var ext = Path.GetExtension(assetPath).ToLowerInvariant();
    var xmlPath = Path.Combine(outputDir, Path.GetFileName(assetPath) + ".xml");

    string xml = ext switch
    {
        ".ydr" => ExportYdr(data, outputDir),
        ".yft" => ExportYft(data, outputDir),
        ".ytd" => ExportYtd(data, outputDir),
        _ => throw new InvalidOperationException($"Unsupported asset type: {ext}")
    };

    File.WriteAllText(xmlPath, xml);
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

static int ImportAsset(string xmlPath, string outputAssetPath)
{
    var xml = File.ReadAllText(xmlPath);
    var inputDir = Path.GetDirectoryName(xmlPath) ?? Environment.CurrentDirectory;
    var ext = Path.GetExtension(outputAssetPath).ToLowerInvariant();

    byte[] data = ext switch
    {
        ".ydr" => XmlYdr.GetYdr(xml, inputDir).Save(),
        ".yft" => XmlYft.GetYft(xml, inputDir).Save(),
        ".ytd" => XmlYtd.GetYtd(xml, inputDir).Save(),
        _ => throw new InvalidOperationException($"Unsupported asset type: {ext}")
    };

    File.WriteAllBytes(outputAssetPath, data);
    Console.WriteLine($"asset={outputAssetPath}");
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
        _ => Usage()
    };
}
catch (Exception ex)
{
    Console.Error.WriteLine(ex.Message);
    return 1;
}
