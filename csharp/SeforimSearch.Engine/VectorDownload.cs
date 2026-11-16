using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Net;
using System.Net.Http;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using System.Threading.Tasks;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    // Host-driven download / update of a published vector set (issue #18): a port of
    // Otzaria's app (lib/semantic_search: semantic_release_locator.dart,
    // semantic_vectors_family.dart, semantic_download_client.dart, staged import), the
    // same as Dart's vector_download.dart. The install itself runs in the engine
    // (se_vectors_install / se_oxv_verify, a port of Otzaria's sidecar install_package).
    // A library feature: no UI, no app settings.

    /// <summary>A family of vector releases: the release tag prefixes of one query model.</summary>
    public sealed class VectorFamily
    {
        public VectorFamily(string releaseTagPrefix, string? deltaReleaseTagPrefix = null)
        {
            ReleaseTagPrefix = releaseTagPrefix;
            DeltaReleaseTagPrefix = deltaReleaseTagPrefix;
        }

        /// <summary><c>&lt;prefix&gt;&lt;libraryTag&gt;</c> names the base release.</summary>
        public string ReleaseTagPrefix { get; }

        /// <summary><c>&lt;prefix&gt;&lt;libraryTag&gt;</c> names a delta release; null = base only.</summary>
        public string? DeltaReleaseTagPrefix { get; }

        /// <summary>Otzaria's published families.</summary>
        public static readonly VectorFamily Qa491Windows = new VectorFamily("vectors-qa491-windows-", "vectors-qa491-windows-delta-");
        public static readonly VectorFamily Qa491 = new VectorFamily("vectors-qa491-");
        public static readonly VectorFamily PardesQa491Windows = new VectorFamily("vectors-pardesqa491-windows-", "vectors-pardesqa491-windows-delta-");
    }

    /// <summary>A release a host can use: a family plus the query model's identity.</summary>
    public sealed class VectorCandidate
    {
        public VectorCandidate(VectorFamily family, string modelFamilyId, ulong chunkingIdentity, string? modelJson = null)
        {
            Family = family;
            ModelFamilyId = modelFamilyId;
            ChunkingIdentity = chunkingIdentity;
            ModelJson = modelJson;
        }

        /// <summary>The query model's model.json text, when known: the install checks the
        /// release against it (Otzaria's <c>InstallExpectation</c>).</summary>
        public string? ModelJson { get; }

        public VectorFamily Family { get; }
        public string ModelFamilyId { get; }
        public ulong ChunkingIdentity { get; }

        /// <summary>From the query model's model.json text (<c>family_id</c>, <c>chunking_identity</c>).</summary>
        public static VectorCandidate FromModelJson(VectorFamily family, string modelJson)
        {
            var j = Json.Parse(modelJson) as Dictionary<string, object?> ?? throw new FormatException("model.json is not a JSON object");
            var id = j.TryGetValue("family_id", out var f) ? f as string : null;
            var chunking = VectorJson.Chunking(modelJson);
            if (string.IsNullOrEmpty(id) || chunking == null) throw new FormatException("model.json: family_id / chunking_identity missing");
            return new VectorCandidate(family, id!, chunking.Value, modelJson);
        }
    }

    /// <summary>A downloadable file of a release (a whole <c>.oxv.zst</c> or one <c>.part-NNN</c>).</summary>
    public sealed class VectorFile
    {
        public VectorFile(string name, string downloadUrl, long size, string sha256, string assetId)
        {
            Name = name; DownloadUrl = downloadUrl; Size = size; Sha256 = sha256; AssetId = assetId;
        }

        public string Name { get; }
        public string DownloadUrl { get; }
        public long Size { get; }
        public string Sha256 { get; }
        public string AssetId { get; }
    }

    /// <summary>A vector release (base or delta) of one library version, its manifest
    /// verified against the published SHA-256.</summary>
    public sealed class VectorRelease
    {
        public string LibraryTag { get; internal set; } = "";
        public string ReleaseTag { get; internal set; } = "";
        public string Kind { get; internal set; } = "";
        public string ManifestJson { get; internal set; } = "";
        public string PublishedManifestSha256 { get; internal set; } = "";
        public int FromLibraryVersion { get; internal set; }
        public int ToLibraryVersion { get; internal set; }
        public IReadOnlyList<VectorFile> Files { get; internal set; } = Array.Empty<VectorFile>();
        public string SegmentId { get; internal set; } = "";
        public string SegmentSha256 { get; internal set; } = "";
        public long SegmentSize { get; internal set; }
        public string IdentityDigest { get; internal set; } = "";
        public string ModelFamilyId { get; internal set; } = "";
        public ulong? ChunkingIdentity { get; internal set; }
        public VectorFamily Family { get; internal set; } = VectorFamily.Qa491;

        public bool IsDelta => Kind == "delta";
        public long DownloadSize => Files.Sum(f => f.Size);

        /// <summary>The compressed segment's name: the single file, or the parts' common name.</summary>
        public string SegmentFileName => Files.Count == 1 ? Files[0].Name : Regex.Replace(Files[0].Name, @"\.part-\d+$", "");
    }

    public enum VectorFailure
    {
        ReleaseMalformed,
        UnsupportedRelease,
        ArtifactNotPublished,
        ReleaseUnverified,
        ArtifactIncompatible,
        Network,
        RateLimited,
        Checksum,
    }

    public sealed class VectorReleaseException : Exception
    {
        public VectorReleaseException(VectorFailure kind, string message) : base(message) { Kind = kind; }
        public VectorFailure Kind { get; }
    }

    /// <summary>What an install did (<c>se_vectors_install</c>).</summary>
    public sealed class VectorInstallReport
    {
        public string Kind { get; set; } = "";
        public int LibraryVersion { get; set; }
        public long Generation { get; set; }
        public int Segments { get; set; }
        public long SlotsAdded { get; set; }
        public long TombstonesApplied { get; set; }
        public long DuplicatesRemoved { get; set; }
        public long ForeignUnresolved { get; set; }
        public long BytesOnDisk { get; set; }
        public bool AlreadyApplied { get; set; }
    }

    /// <summary>What <see cref="VectorSet.Verify"/> checked.</summary>
    public sealed class VectorVerifyReport
    {
        public string SegmentId { get; set; } = "";
        public string Kind { get; set; } = "";
        public int FromLibraryVersion { get; set; }
        public int ToLibraryVersion { get; set; }
        public long Slots { get; set; }
        public long Blocks { get; set; }
        public bool ManifestChecked { get; set; }
    }

    /// <summary>A vector set directory as Otzaria's sidecar lays it out
    /// (<c>segments/</c>, <c>gen-NNNNNN/set.json</c> + <c>.del</c> / <c>.links</c>,
    /// <c>CURRENT</c> / <c>PREVIOUS</c>, <c>incoming/</c>, <c>.lock</c>).</summary>
    public static unsafe class VectorSet
    {
        /// <summary>Verifies a plain <c>.oxv</c> in the engine: header and directory, every
        /// 1 MiB block CRC and, with <paramref name="manifestJson"/>, the header against its
        /// release manifest. Throws <see cref="SeforimEngineException"/> when it fails,
        /// <see cref="OperationCanceledException"/> when <paramref name="ct"/> is cancelled.</summary>
        public static VectorVerifyReport Verify(string path, string? manifestJson = null, CancellationToken ct = default)
        {
            byte[] p = Interop.ToUtf8(path ?? throw new ArgumentNullException(nameof(path)));
            byte[] m = Interop.ToUtf8(manifestJson ?? "");
            var text = SeforimSemantic.WithToken(ct, token =>
            {
                fixed (byte* pp = p) fixed (byte* pm = m)
                    return Interop.TakeString(NativeMethods.se_oxv_verify(pp, (UIntPtr)p.Length, pm, (UIntPtr)m.Length, token));
            });
            {
                var j = (Dictionary<string, object?>)Json.Parse(text)!;
                return new VectorVerifyReport
                {
                    SegmentId = (string)j["segment_id"]!,
                    Kind = (string)j["kind"]!,
                    FromLibraryVersion = (int)(double)j["from_library_version"]!,
                    ToLibraryVersion = (int)(double)j["to_library_version"]!,
                    Slots = (long)(double)j["slots"]!,
                    Blocks = (long)(double)j["blocks"]!,
                    ManifestChecked = (bool)j["manifest_checked"]!,
                };
            }
        }

        /// <summary>Installs a downloaded plain <c>.oxv</c> into the set at <paramref name="setDir"/>
        /// in the engine (Otzaria's sidecar <c>install_package</c>): a base starts a new
        /// generation, a delta applies its tombstones, duplicates and foreign links to the current
        /// one. A segment inside <c>&lt;setDir&gt;/incoming/</c> is moved, any other copied.
        /// <paramref name="expectedModelJson"/>: the query model's model.json, which the release must
        /// be of. <paramref name="ct"/> stops it between blocks, hashed chunks and slot ranges, leaving
        /// the set as it was (<see cref="OperationCanceledException"/>).</summary>
        public static VectorInstallReport Install(string setDir, string segmentPath, string manifestJson, string? publishedManifestSha256 = null,
            string? expectedModelJson = null, CancellationToken ct = default)
        {
            byte[] d = Interop.ToUtf8(setDir ?? throw new ArgumentNullException(nameof(setDir)));
            byte[] s = Interop.ToUtf8(segmentPath ?? throw new ArgumentNullException(nameof(segmentPath)));
            byte[] m = Interop.ToUtf8(manifestJson ?? throw new ArgumentNullException(nameof(manifestJson)));
            byte[] h = Interop.ToUtf8(publishedManifestSha256 ?? "");
            byte[] e = Interop.ToUtf8(expectedModelJson ?? "");
            var text = SeforimSemantic.WithToken(ct, token =>
            {
                fixed (byte* pd = d) fixed (byte* ps = s) fixed (byte* pm = m) fixed (byte* ph = h) fixed (byte* pe = e)
                    return Interop.TakeString(NativeMethods.se_vectors_install(
                        pd, (UIntPtr)d.Length, ps, (UIntPtr)s.Length, pm, (UIntPtr)m.Length, ph, (UIntPtr)h.Length, pe, (UIntPtr)e.Length, token));
            });
            {
                var j = (Dictionary<string, object?>)Json.Parse(text)!;
                long L(string k) => (long)(double)j[k]!;
                return new VectorInstallReport
                {
                    Kind = (string)j["kind"]!,
                    LibraryVersion = (int)L("library_version"),
                    Generation = L("generation"),
                    Segments = (int)L("segments"),
                    SlotsAdded = L("slots_added"),
                    TombstonesApplied = L("tombstones_applied"),
                    DuplicatesRemoved = L("duplicates_removed"),
                    ForeignUnresolved = L("foreign_unresolved"),
                    BytesOnDisk = L("bytes_on_disk"),
                    AlreadyApplied = (bool)j["already_applied"]!,
                };
            }
        }

        /// <summary>The set.json the set opens (CURRENT, else PREVIOUS), or null.</summary>
        public static IReadOnlyDictionary<string, object?>? CurrentSet(string setDir)
        {
            foreach (var name in new[] { "CURRENT", "PREVIOUS" })
            {
                try
                {
                    var p = Json.Parse(File.ReadAllText(Path.Combine(setDir, name))) as Dictionary<string, object?>;
                    if (p == null || !(p.TryGetValue("set", out var set) && set is string rel)) continue;
                    byte[] bytes = File.ReadAllBytes(Path.Combine(setDir, rel));
                    if (VectorJson.Hex(SHA256Hash(bytes)) != (p["set_sha256"] as string)?.ToLowerInvariant()) continue;
                    if (Json.Parse(Encoding.UTF8.GetString(bytes)) is Dictionary<string, object?> doc &&
                        doc.TryGetValue("format", out var f) && (f as string) == "otzaria-vector-set") return doc;
                }
                catch (IOException) { }
                catch (UnauthorizedAccessException) { }
                catch (FormatException) { }
            }
            return null;
        }

        private static byte[] SHA256Hash(byte[] bytes)
        {
            using var sha = SHA256.Create();
            return sha.ComputeHash(bytes);
        }
    }

    internal static class VectorJson
    {
        private static readonly Regex ChunkingRe = new Regex(@"""chunking_identity""\s*:\s*(0|[1-9][0-9]*)(?=\s*[,}])");

        /// <summary>The exact <c>chunking_identity</c> (a u64; doubles would round it):
        /// read from the text when it appears once.</summary>
        public static ulong? Chunking(string raw)
        {
            var ms = ChunkingRe.Matches(raw);
            return ms.Count == 1 && ulong.TryParse(ms[0].Groups[1].Value, NumberStyles.None, CultureInfo.InvariantCulture, out var v) ? v : (ulong?)null;
        }

        public static string Hex(byte[] b)
        {
            var sb = new StringBuilder(b.Length * 2);
            foreach (var x in b) sb.Append(x.ToString("x2", CultureInfo.InvariantCulture));
            return sb.ToString();
        }
    }

    /// <summary>Finds the vector release of a library version (Otzaria's
    /// <c>SemanticVectorsReleaseLocator</c>, GitHub releases API).</summary>
    public sealed class VectorReleaseLocator
    {
        public const string DefaultApiBase = "https://api.github.com/repos/Otzaria/SeforimLibrary";
        private const int MaxPages = 3;
        /// <summary>Otzaria's <c>kSemanticDeltaChainLimits</c>.</summary>
        public const int MaxDeltaSteps = 8;
        public const double MaxDeltaBaseSizeRatio = 0.5;
        private static readonly Regex LibraryTagRe = new Regex(@"^v(\d+)-");
        private static readonly Regex DeltaManifestRe = new Regex(@"-v(\d+)-v(\d+)\.manifest\.json$");
        private static readonly Regex Sha256Re = new Regex(@"\b[0-9a-f]{64}\b");

        private readonly HttpClient _http;
        private readonly string _apiBase;

        public VectorReleaseLocator(HttpClient? http = null, string apiBase = DefaultApiBase, string userAgent = "seforim-search")
        {
            _http = http ?? new HttpClient { Timeout = TimeSpan.FromSeconds(15) };
            _apiBase = apiBase;
            UserAgent = userAgent;
        }

        public string UserAgent { get; }

        /// <summary>The release of <paramref name="libraryVersion"/> for the first of
        /// <paramref name="candidates"/> published, or null.</summary>
        public async Task<VectorRelease?> FindForLibraryVersionAsync(int libraryVersion, IReadOnlyList<VectorCandidate> candidates,
            string? libraryTag = null, CancellationToken ct = default)
        {
            var tag = libraryTag != null && VersionOfTag(libraryTag) == libraryVersion ? libraryTag : await FindLibraryTagAsync(libraryVersion, ct).ConfigureAwait(false);
            if (tag == null) return null;
            Exception? failure = null;
            foreach (var c in candidates)
            {
                try
                {
                    var r = await FindCandidateAsync(c, tag, libraryVersion, ct).ConfigureAwait(false);
                    if (r != null) return r;
                }
                catch (VectorReleaseException e) { failure = e; }
                catch (HttpRequestException e) { failure = e; }
            }
            if (failure != null) throw failure;
            return null;
        }

        /// <summary>The deltas from <paramref name="installedVersion"/> to <paramref name="target"/>,
        /// oldest first; null = take the base.</summary>
        public async Task<IReadOnlyList<VectorRelease>?> FindDeltaChainAsync(VectorRelease target, int installedVersion, CancellationToken ct = default)
        {
            var prefix = target.Family.DeltaReleaseTagPrefix;
            if (prefix == null || target.IdentityDigest.Length == 0 || installedVersion >= target.ToLibraryVersion) return null;
            double maxBytes = target.DownloadSize * MaxDeltaBaseSizeRatio;
            try
            {
                var chain = new List<VectorRelease>();
                var scanned = new List<object?>();
                int pages = 0;
                string? tag = target.LibraryTag;
                int to = target.ToLibraryVersion;
                long bytes = 0;
                while (to > installedVersion)
                {
                    if (tag == null || chain.Count >= MaxDeltaSteps) return null;
                    var delta = await FindDeltaAsync(prefix + tag, tag, to, target, ct).ConfigureAwait(false);
                    if (delta == null || delta.FromLibraryVersion < installedVersion || delta.FromLibraryVersion >= to) return null;
                    bytes += delta.DownloadSize;
                    if (bytes > maxBytes) return null;
                    chain.Insert(0, delta);
                    to = delta.FromLibraryVersion;
                    if (to > installedVersion)
                    {
                        tag = LibraryTagFor(scanned, to);
                        while (tag == null && pages < MaxPages)
                        {
                            if (!(await GetJsonAsync($"{_apiBase}/releases?per_page=100&page={++pages}", ct).ConfigureAwait(false) is List<object?> list) || list.Count == 0) break;
                            scanned.AddRange(list);
                            tag = LibraryTagFor(list, to);
                        }
                    }
                }
                return chain;
            }
            catch (VectorReleaseException e) when (e.Kind == VectorFailure.ReleaseMalformed || e.Kind == VectorFailure.UnsupportedRelease
                || e.Kind == VectorFailure.ArtifactNotPublished || e.Kind == VectorFailure.ReleaseUnverified)
            {
                return null;
            }
            catch (FormatException) { return null; }
        }

        /// <summary>The library version of a tag such as <c>v30-20260930165019</c>, or null.</summary>
        public static int? VersionOfTag(string tag)
        {
            var m = LibraryTagRe.Match(tag);
            return m.Success && int.TryParse(m.Groups[1].Value, NumberStyles.None, CultureInfo.InvariantCulture, out var v) ? v : (int?)null;
        }

        public static string? LibraryTagFor(IEnumerable<object?> releases, int version)
        {
            foreach (var r in releases)
                if (r is Dictionary<string, object?> d && d.TryGetValue("tag_name", out var t) && t is string tag && VersionOfTag(tag) == version)
                    return tag;
            return null;
        }

        /// <summary>The manifest's SHA-256 in the release notes: the first after its name, else
        /// the only one in the notes.</summary>
        public static string? ParsePublishedManifestSha256(string? body, string manifest)
        {
            if (body == null) return null;
            var text = body.ToLowerInvariant();
            int at = text.IndexOf(manifest.ToLowerInvariant(), StringComparison.Ordinal);
            if (at >= 0)
            {
                var after = Sha256Re.Match(text.Substring(at + manifest.Length));
                if (after.Success) return after.Value;
            }
            var all = Sha256Re.Matches(text).Cast<Match>().Select(m => m.Value).Distinct().ToList();
            return all.Count == 1 ? all[0] : null;
        }

        private async Task<string?> FindLibraryTagAsync(int version, CancellationToken ct)
        {
            for (int page = 1; page <= MaxPages; page++)
            {
                if (!(await GetJsonAsync($"{_apiBase}/releases?per_page=100&page={page}", ct).ConfigureAwait(false) is List<object?> list) || list.Count == 0) return null;
                var tag = LibraryTagFor(list, version);
                if (tag != null) return tag;
            }
            return null;
        }

        private async Task<VectorRelease?> FindDeltaAsync(string releaseTag, string tag, int to, VectorRelease target, CancellationToken ct)
        {
            var release = await GetJsonAsync($"{_apiBase}/releases/tags/{Uri.EscapeDataString(releaseTag)}", ct).ConfigureAwait(false);
            if (release == null) return null;
            if (!(release is Dictionary<string, object?> r)) throw Malformed("release JSON");
            var delta = await ParseReleaseAsync(r, tag, releaseTag, to, target.Family, "delta", ct).ConfigureAwait(false);
            return delta.IdentityDigest == target.IdentityDigest && delta.ChunkingIdentity == target.ChunkingIdentity ? delta : null;
        }

        private async Task<VectorRelease?> FindCandidateAsync(VectorCandidate c, string tag, int libraryVersion, CancellationToken ct)
        {
            var releaseTag = c.Family.ReleaseTagPrefix + tag;
            var release = await GetJsonAsync($"{_apiBase}/releases/tags/{Uri.EscapeDataString(releaseTag)}", ct).ConfigureAwait(false);
            if (release == null) return null;
            if (!(release is Dictionary<string, object?> r)) throw Malformed("release JSON is not an object");
            var parsed = await ParseReleaseAsync(r, tag, releaseTag, libraryVersion, c.Family, "base", ct).ConfigureAwait(false);
            if (parsed.ModelFamilyId != c.ModelFamilyId)
                throw new VectorReleaseException(VectorFailure.ArtifactIncompatible, $"{releaseTag} is model {parsed.ModelFamilyId}, expected {c.ModelFamilyId}");
            if (parsed.ChunkingIdentity != c.ChunkingIdentity)
                throw new VectorReleaseException(VectorFailure.ArtifactIncompatible, $"{releaseTag} is chunking {parsed.ChunkingIdentity}, expected {c.ChunkingIdentity}");
            return parsed;
        }

        private async Task<VectorRelease> ParseReleaseAsync(Dictionary<string, object?> release, string libraryTag, string releaseTag,
            int libraryVersion, VectorFamily family, string kind, CancellationToken ct)
        {
            if (!(release.TryGetValue("assets", out var al) && al is List<object?> list)) throw Malformed("release assets");
            var assets = new Dictionary<string, Dictionary<string, object?>>();
            foreach (var a in list)
                if (a is Dictionary<string, object?> d && d.TryGetValue("name", out var n) && n is string name) assets[name] = d;
            var manifests = assets.Where(kv => kv.Key.StartsWith("otzaria-vectors-", StringComparison.Ordinal) && kv.Key.EndsWith(".manifest.json", StringComparison.Ordinal)).ToList();
            var manifestAsset = kind == "delta"
                ? manifests.FirstOrDefault(kv => DeltaManifestRe.IsMatch(kv.Key))
                : manifests.FirstOrDefault(kv => kv.Key.EndsWith("-base.manifest.json", StringComparison.Ordinal)) is var b && b.Key != null ? b : manifests.FirstOrDefault();
            if (manifestAsset.Key == null) throw Malformed("manifest asset missing");
            string manifestName = manifestAsset.Key;
            byte[] manifestBytes = await GetBytesAsync(Str(manifestAsset.Value, "browser_download_url", "manifest URL"), ct).ConfigureAwait(false);
            string manifestSha;
            using (var sha = SHA256.Create()) manifestSha = VectorJson.Hex(sha.ComputeHash(manifestBytes));
            string? assetDigest = manifestAsset.Value.TryGetValue("digest", out var dg) && dg is string ds && ds.StartsWith("sha256:", StringComparison.Ordinal)
                ? ds.Substring(7).ToLowerInvariant() : null;
            release.TryGetValue("body", out var bodyObj);
            if (bodyObj != null && !(bodyObj is string)) throw Malformed("release body");
            string? notesDigest = ParsePublishedManifestSha256(bodyObj as string, manifestName);
            string published = notesDigest ?? assetDigest
                ?? throw new VectorReleaseException(VectorFailure.ReleaseUnverified, $"no published SHA-256 for {manifestName}");
            foreach (var expected in new[] { assetDigest, notesDigest })
                if (expected != null && expected != manifestSha)
                    throw new VectorReleaseException(VectorFailure.ArtifactNotPublished, $"manifest {manifestName} is {manifestSha}, published {expected}");
            string manifestJson = new UTF8Encoding(false).GetString(manifestBytes);
            Dictionary<string, object?> m;
            try { m = Json.Parse(manifestJson) as Dictionary<string, object?> ?? throw Malformed("manifest shape"); }
            catch (FormatException e) { throw Malformed("manifest is not JSON: " + e.Message); }
            if (!(m.TryGetValue("files", out var fl) && fl is List<object?> files && files.Count > 0)) throw Malformed("manifest files");
            if ((m.TryGetValue("kind", out var k) ? k as string : null) != kind)
                throw new VectorReleaseException(VectorFailure.UnsupportedRelease, $"{releaseTag} holds a {k} manifest, expected {kind}");
            int toVersion = Int(m, "toLibraryVersion");
            int fromVersion = m.TryGetValue("fromLibraryVersion", out var fv) && fv is double fd ? (int)fd
                : kind == "base" ? 0 : throw Malformed("fromLibraryVersion missing");
            if (kind == "delta")
            {
                var named = DeltaManifestRe.Match(manifestName);
                if (fromVersion >= toVersion || int.Parse(named.Groups[1].Value, CultureInfo.InvariantCulture) != fromVersion
                    || int.Parse(named.Groups[2].Value, CultureInfo.InvariantCulture) != toVersion)
                    throw Malformed($"{manifestName} holds v{fromVersion}-v{toVersion}");
            }
            string? manifestTag = m.TryGetValue("libraryReleaseTag", out var lt) ? lt as string : null;
            if (toVersion != libraryVersion || manifestTag != libraryTag)
                throw Malformed($"manifest is for {manifestTag} (v{toVersion}), requested {libraryTag} (v{libraryVersion})");
            if (!(m.TryGetValue("segment", out var so) && so is Dictionary<string, object?> segment)) throw Malformed("manifest segment");
            var segmentId = m.TryGetValue("segmentId", out var sid) ? sid as string : null;
            if (segmentId == null || !Regex.IsMatch(segmentId, "^[0-9a-f]{32}$")) throw Malformed("segmentId");
            var model = m.TryGetValue("identity", out var idn) && idn is Dictionary<string, object?> identity
                && identity.TryGetValue("model", out var mo) ? mo as Dictionary<string, object?> : null;
            return new VectorRelease
            {
                LibraryTag = libraryTag,
                ReleaseTag = releaseTag,
                Kind = kind,
                ManifestJson = manifestJson,
                PublishedManifestSha256 = published,
                FromLibraryVersion = fromVersion,
                ToLibraryVersion = toVersion,
                Files = files.Select(e => FileOf(e, assets)).ToList(),
                SegmentId = segmentId,
                SegmentSha256 = Str(segment, "sha256", "segment.sha256").ToLowerInvariant(),
                SegmentSize = Long(segment, "size"),
                IdentityDigest = m.TryGetValue("identityDigest", out var dgs) && dgs is string dgst ? dgst : "",
                ModelFamilyId = model != null && model.TryGetValue("family_id", out var fam) && fam is string fs ? fs : "",
                ChunkingIdentity = model != null ? VectorJson.Chunking(manifestJson) : null,
                Family = family,
            };
        }

        private static VectorFile FileOf(object? e, Dictionary<string, Dictionary<string, object?>> assets)
        {
            if (!(e is Dictionary<string, object?> d) || !(d.TryGetValue("file", out var n) && n is string name)
                || !(d.TryGetValue("sha256", out var s) && s is string sha) || name.IndexOfAny(new[] { '/', '\\' }) >= 0 || name == "." || name == "..")
                throw Malformed("file entry");
            if (!assets.TryGetValue(name, out var asset)) throw Malformed($"asset {name} missing from release");
            return new VectorFile(name, Str(asset, "browser_download_url", name + " URL"), Long(d, "size"), sha.ToLowerInvariant(),
                asset.TryGetValue("id", out var id) && id != null ? Convert.ToString(id is double x ? (object)(long)x : id, CultureInfo.InvariantCulture) ?? "" : "");
        }

        private static string Str(Dictionary<string, object?> d, string key, string what) =>
            d.TryGetValue(key, out var v) && v is string s ? s : throw Malformed(what + " missing");
        private static long Long(Dictionary<string, object?> d, string key) =>
            d.TryGetValue(key, out var v) && v is double x && x == Math.Floor(x) ? (long)x : throw Malformed(key + " missing");
        private static int Int(Dictionary<string, object?> d, string key) => checked((int)Long(d, key));
        private static VectorReleaseException Malformed(string d) => new VectorReleaseException(VectorFailure.ReleaseMalformed, d);

        private async Task<object?> GetJsonAsync(string url, CancellationToken ct)
        {
            using var rq = new HttpRequestMessage(HttpMethod.Get, url);
            rq.Headers.TryAddWithoutValidation("User-Agent", UserAgent);
            rq.Headers.TryAddWithoutValidation("Accept", "application/vnd.github+json");
            using var rs = await Send(rq, ct).ConfigureAwait(false);
            int status = (int)rs.StatusCode;
            if (status == 404) return null;
            if (status < 200 || status >= 300)
                throw new VectorReleaseException(status == 403 || status == 429 ? VectorFailure.RateLimited : VectorFailure.Network, $"GitHub API returned {status} for {url}");
            return Json.Parse(await rs.Content.ReadAsStringAsync().ConfigureAwait(false));
        }

        private async Task<byte[]> GetBytesAsync(string url, CancellationToken ct)
        {
            using var rq = new HttpRequestMessage(HttpMethod.Get, url);
            rq.Headers.TryAddWithoutValidation("User-Agent", UserAgent);
            using var rs = await Send(rq, ct).ConfigureAwait(false);
            int status = (int)rs.StatusCode;
            if (status < 200 || status >= 300) throw new VectorReleaseException(VectorFailure.Network, $"manifest download returned {status}");
            return await rs.Content.ReadAsByteArrayAsync().ConfigureAwait(false);
        }

        private async Task<HttpResponseMessage> Send(HttpRequestMessage rq, CancellationToken ct)
        {
            try { return await _http.SendAsync(rq, ct).ConfigureAwait(false); }
            catch (HttpRequestException e) { throw new VectorReleaseException(VectorFailure.Network, $"{rq.RequestUri}: {e.Message}"); }
        }
    }

    /// <summary>Resumable, verified download: bytes go to <c>&lt;dest&gt;.partial</c> (resumed with
    /// a Range request when the same asset was half downloaded), are checked for size and SHA-256,
    /// then renamed to <c>&lt;dest&gt;</c>. A complete <c>&lt;dest&gt;</c> with the right hash is reused.</summary>
    public sealed class VectorDownloader
    {
        private readonly HttpClient _http;

        public VectorDownloader(HttpClient? http = null, string userAgent = "seforim-search")
        {
            _http = http ?? new HttpClient { Timeout = Timeout.InfiniteTimeSpan };
            UserAgent = userAgent;
        }

        public string UserAgent { get; }

        public async Task DownloadAsync(VectorFile file, string dest, IProgress<(string Stage, long Done, long? Total)>? progress = null, CancellationToken ct = default)
        {
            if (File.Exists(dest) && new FileInfo(dest).Length == file.Size && Sha256OfFile(dest, ct) == file.Sha256)
            {
                progress?.Report(("download", file.Size, file.Size));
                return;
            }
            string partial = dest + ".partial", idFile = dest + ".partial.id";
            string identity = $"{file.AssetId}:{file.Sha256}:{file.Size}";
            if (File.Exists(partial) && (!File.Exists(idFile) || File.ReadAllText(idFile) != identity)) File.Delete(partial);
            Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(dest))!);
            File.WriteAllText(idFile, identity);
            long have = File.Exists(partial) ? new FileInfo(partial).Length : 0;
            if (have > file.Size) { File.Delete(partial); have = 0; }
            if (have < file.Size)
            {
                using var rq = new HttpRequestMessage(HttpMethod.Get, file.DownloadUrl);
                rq.Headers.TryAddWithoutValidation("User-Agent", UserAgent);
                if (have > 0) rq.Headers.Range = new System.Net.Http.Headers.RangeHeaderValue(have, null);
                HttpResponseMessage rs;
                try { rs = await _http.SendAsync(rq, HttpCompletionOption.ResponseHeadersRead, ct).ConfigureAwait(false); }
                catch (HttpRequestException e) { throw new VectorReleaseException(VectorFailure.Network, $"{file.Name}: {e.Message}"); }
                using (rs)
                {
                    int status = (int)rs.StatusCode;
                    var range = rs.Content.Headers.ContentRange;
                    if (status == 416 || (status == 206 && range?.From != null && range.From != have))
                    {
                        File.Delete(partial);
                        throw new VectorReleaseException(VectorFailure.Network, $"{file.Name}: range not served (status {status}), the partial was dropped");
                    }
                    if (status != 200 && status != 206)
                        throw new VectorReleaseException(status == 403 || status == 429 ? VectorFailure.RateLimited : VectorFailure.Network, $"vector download returned {status}");
                    if (status == 200) have = 0;
                    using var input = await rs.Content.ReadAsStreamAsync().ConfigureAwait(false);
                    using var output = new FileStream(partial, have == 0 ? FileMode.Create : FileMode.Append, FileAccess.Write, FileShare.None, 1 << 16);
                    var buffer = new byte[1 << 16];
                    int n;
                    while ((n = await input.ReadAsync(buffer, 0, buffer.Length, ct).ConfigureAwait(false)) > 0)
                    {
                        int take = (int)Math.Min(n, file.Size - have);
                        await output.WriteAsync(buffer, 0, take, ct).ConfigureAwait(false);
                        have += take;
                        progress?.Report(("download", have, file.Size));
                        if (take < n) break; // more than declared: the size check below refuses it
                    }
                }
            }
            long size = new FileInfo(partial).Length;
            string sha = Sha256OfFile(partial, ct);
            if (size != file.Size || sha != file.Sha256)
            {
                File.Delete(partial);
                File.Delete(idFile);
                throw new VectorReleaseException(VectorFailure.Checksum, $"{file.Name} is {size} bytes sha256 {sha}, expected {file.Size} {file.Sha256}");
            }
            if (File.Exists(dest)) File.Delete(dest);
            File.Move(partial, dest);
            File.Delete(idFile);
        }

        /// <summary>SHA-256 of a file, hex.</summary>
        public static string Sha256OfFile(string path, CancellationToken ct = default)
        {
            using var sha = SHA256.Create();
            using var f = File.OpenRead(path);
            var buffer = new byte[1 << 20];
            int n;
            while ((n = f.Read(buffer, 0, buffer.Length)) > 0)
            {
                ct.ThrowIfCancellationRequested();
                sha.TransformBlock(buffer, 0, n, null, 0);
            }
            sha.TransformFinalBlock(buffer, 0, 0);
            return VectorJson.Hex(sha.Hash!);
        }
    }

    /// <summary>Finds, downloads, verifies, expands and installs the vector set of a library
    /// version into a set directory: a base, or a delta chain when a set of the same identity
    /// is installed and a short enough chain exists (applied in the engine).</summary>
    public sealed class VectorSetUpdater
    {
        public VectorSetUpdater(string setDir, VectorReleaseLocator? locator = null, VectorDownloader? downloader = null)
        {
            SetDir = setDir;
            Locator = locator ?? new VectorReleaseLocator();
            Downloader = downloader ?? new VectorDownloader();
        }

        public string SetDir { get; }
        public VectorReleaseLocator Locator { get; }
        public VectorDownloader Downloader { get; }
        public string Incoming => Path.Combine(SetDir, "incoming");

        /// <summary>Brings the set to <paramref name="libraryVersion"/>; null when no release of it
        /// is published for <paramref name="candidates"/>.</summary>
        public async Task<VectorInstallReport?> UpdateAsync(int libraryVersion, IReadOnlyList<VectorCandidate> candidates, string? libraryTag = null,
            IProgress<(string Stage, long Done, long? Total)>? progress = null, CancellationToken ct = default)
        {
            var target = await Locator.FindForLibraryVersionAsync(libraryVersion, candidates, libraryTag, ct).ConfigureAwait(false);
            if (target == null) return null;
            var current = VectorSet.CurrentSet(SetDir);
            int? installed = current != null && current.TryGetValue("library_version", out var lv) && lv is double d ? (int)d : (int?)null;
            bool same = current != null && current.TryGetValue("identity_digest", out var idg) && (idg as string) == target.IdentityDigest;
            if (installed != null && same && installed >= target.ToLibraryVersion)
                return new VectorInstallReport
                {
                    Kind = "base",
                    LibraryVersion = installed.Value,
                    Generation = (long)(double)current!["generation"]!,
                    Segments = ((List<object?>)current["segments"]!).Count,
                    AlreadyApplied = true,
                };
            IReadOnlyList<VectorRelease>? chain = null;
            if (installed != null && same) chain = await Locator.FindDeltaChainAsync(target, installed.Value, ct).ConfigureAwait(false);
            string? model = candidates.FirstOrDefault(c => c.Family == target.Family && c.ModelFamilyId == target.ModelFamilyId)?.ModelJson;
            VectorInstallReport? last = null;
            foreach (var r in chain == null || chain.Count == 0 ? new[] { target } : chain)
                last = await InstallReleaseAsync(r, progress, ct, model).ConfigureAwait(false);
            return last;
        }

        /// <summary>Downloads every file of <paramref name="release"/> to incoming/, joins parts,
        /// expands, checks the segment's size and SHA-256 and installs it in the engine.</summary>
        public async Task<VectorInstallReport> InstallReleaseAsync(VectorRelease release, IProgress<(string Stage, long Done, long? Total)>? progress = null,
            CancellationToken ct = default, string? expectedModelJson = null)
        {
            Directory.CreateDirectory(Incoming);
            var local = new List<string>();
            foreach (var f in release.Files)
            {
                string dest = Path.Combine(Incoming, f.Name);
                await Downloader.DownloadAsync(f, dest, progress, ct).ConfigureAwait(false);
                local.Add(dest);
            }
            string compressed = local[0];
            if (local.Count > 1)
            {
                compressed = Path.Combine(Incoming, release.SegmentFileName);
                using var output = File.Create(compressed);
                foreach (var part in local)
                    using (var input = File.OpenRead(part)) await input.CopyToAsync(output, 1 << 20, ct).ConfigureAwait(false);
            }
            ct.ThrowIfCancellationRequested();
            progress?.Report(("expand", 0, release.SegmentSize));
            string segment = Path.Combine(Incoming, release.SegmentId + ".oxv");
            long written = await Task.Run(() => SeforimSemantic.ExpandOxv(compressed, segment), ct).ConfigureAwait(false);
            if (written != release.SegmentSize)
            {
                File.Delete(segment);
                throw new VectorReleaseException(VectorFailure.Checksum, $"expanded segment is {written} bytes, expected {release.SegmentSize}");
            }
            progress?.Report(("install", 0, null));
            var report = await Task.Run(() => VectorSet.Install(SetDir, segment, release.ManifestJson, release.PublishedManifestSha256, expectedModelJson, ct), ct).ConfigureAwait(false);
            foreach (var p in local.Append(compressed).Distinct())
                if (File.Exists(p)) File.Delete(p);
            return report;
        }
    }
}
