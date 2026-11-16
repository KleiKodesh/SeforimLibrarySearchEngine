using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using SeforimSearch.Engine;

namespace SeforimSearch.Demo
{
    internal static partial class SelfTest
    {
        /// <summary>A fake GitHub (releases API + asset host) on a local HttpListener.</summary>
        private sealed class FakeGithub : IDisposable
        {
            private readonly HttpListener _listener = new HttpListener();
            public readonly Dictionary<string, byte[]> Assets = new Dictionary<string, byte[]>();
            public readonly Dictionary<string, string> Json = new Dictionary<string, string>();
            public readonly List<string> Ranges = new List<string>();
            public string Base { get; }

            public FakeGithub()
            {
                var probe = new TcpListener(IPAddress.Loopback, 0);
                probe.Start();
                int port = ((IPEndPoint)probe.LocalEndpoint).Port;
                probe.Stop();
                Base = $"http://localhost:{port}";
                _listener.Prefixes.Add(Base + "/");
                _listener.Start();
                Task.Run(Serve);
            }

            private async Task Serve()
            {
                while (_listener.IsListening)
                {
                    HttpListenerContext c;
                    try { c = await _listener.GetContextAsync(); } catch (Exception) { return; }
                    string path = c.Request.Url!.PathAndQuery;
                    var rs = c.Response;
                    byte[] body;
                    if (Json.TryGetValue(path, out var j)) body = Encoding.UTF8.GetBytes(j);
                    else if (Assets.TryGetValue(path, out var a))
                    {
                        body = a;
                        string? range = c.Request.Headers["Range"];
                        if (range != null)
                        {
                            lock (Ranges) Ranges.Add(range);
                            int from = int.Parse(range.Substring(6, range.Length - 7));
                            body = a.Skip(from).ToArray();
                            rs.StatusCode = 206;
                            rs.AddHeader("Content-Range", $"bytes {from}-{a.Length - 1}/{a.Length}");
                        }
                    }
                    else { rs.StatusCode = 404; body = Array.Empty<byte>(); }
                    rs.ContentLength64 = body.Length;
                    try { await rs.OutputStream.WriteAsync(body, 0, body.Length); rs.Close(); } catch (Exception) { }
                }
            }

            public void Dispose()
            {
                try { _listener.Stop(); _listener.Close(); } catch (Exception) { }
            }

            private static string Sha(byte[] b)
            {
                using var s = SHA256.Create();
                return string.Concat(s.ComputeHash(b).Select(x => x.ToString("x2")));
            }

            /// <summary>Publishes a fixture (seforim-vectors/tests/data/install) as a release,
            /// its compressed bytes in <paramref name="parts"/> files.</summary>
            public void Publish(string fixtures, string name, int parts = 1, int pad = 0)
            {
                string manifest = File.ReadAllText(Path.Combine(fixtures, name + ".manifest.json"));
                byte[] zst = File.ReadAllBytes(Path.Combine(fixtures, name + ".oxv.zst"));
                if (pad > 0)
                {
                    // a zstd skippable frame: a bigger download that expands to nothing
                    var b = new List<byte>(zst);
                    b.AddRange(BitConverter.GetBytes(0x184D2A50u));
                    b.AddRange(BitConverter.GetBytes((uint)pad));
                    b.AddRange(new byte[pad]);
                    zst = b.ToArray();
                }
                string tag = System.Text.RegularExpressions.Regex.Match(manifest, "\"libraryReleaseTag\":\"([^\"]+)\"").Groups[1].Value;
                bool delta = manifest.Contains("\"kind\":\"delta\"");
                string releaseTag = (delta ? "vectors-qa491-windows-delta-" : "vectors-qa491-windows-") + tag;
                string stem = "otzaria-vectors-aa-" + (delta ? name.Substring("delta-".Length) : name.Substring("base-".Length) + "-base");
                int step = (zst.Length + parts - 1) / parts;
                var files = new List<string>();
                var assets = new List<string>();
                for (int i = 0; i < parts; i++)
                {
                    byte[] chunk = zst.Skip(i * step).Take(step).ToArray();
                    string f = parts == 1 ? stem + ".oxv.zst" : $"{stem}.oxv.zst.part-{i:000}";
                    string path = $"/dl/{releaseTag}/{f}";
                    Assets[path] = chunk;
                    files.Add($"{{\"file\":\"{f}\",\"sha256\":\"{Sha(chunk)}\",\"size\":{chunk.Length}}}");
                    assets.Add($"{{\"name\":\"{f}\",\"id\":{i + 1},\"browser_download_url\":\"{Base}{path}\"}}");
                }
                manifest = manifest.Replace("\"files\":[]", "\"files\":[" + string.Join(",", files) + "]");
                byte[] mb = Encoding.UTF8.GetBytes(manifest);
                string mName = stem + ".manifest.json";
                Assets[$"/dl/{releaseTag}/{mName}"] = mb;
                assets.Add($"{{\"name\":\"{mName}\",\"browser_download_url\":\"{Base}/dl/{releaseTag}/{mName}\",\"digest\":\"sha256:{Sha(mb)}\"}}");
                Json[$"/releases/tags/{releaseTag}"] = $"{{\"tag_name\":\"{releaseTag}\",\"body\":\"{mName} {Sha(mb)}\",\"assets\":[{string.Join(",", assets)}]}}";
            }
        }

        /// <summary>Vector set download / update (issue #18): fake releases on a local HttpListener,
        /// the Rust-written fixtures (base v1, deltas v1-v2 / v2-v3, base v3): a resumed download,
        /// base install, a delta chain applied in the engine, a corrupted block refused.</summary>
        private static void VectorDownload(string db, string dir)
        {
            string? fixtures = null;
            for (var d0 = new DirectoryInfo(AppContext.BaseDirectory); d0 != null && fixtures == null; d0 = d0.Parent)
            {
                string f = Path.Combine(d0.FullName, "seforim-vectors", "tests", "data", "install");
                if (Directory.Exists(f)) fixtures = f;
            }
            Check(fixtures != null, "fixtures found");
            using var gh = new FakeGithub();
            gh.Json["/releases?per_page=100&page=1"] = "[{\"tag_name\":\"v3-20261103\"},{\"tag_name\":\"v2-20261102\"},{\"tag_name\":\"v1-20261101\"}]";
            gh.Publish(fixtures!, "base-v1", parts: 2);
            gh.Publish(fixtures!, "delta-v1-v2");
            gh.Publish(fixtures!, "delta-v2-v3");
            gh.Publish(fixtures!, "base-v3", pad: 8192);
            var candidates = new[] { VectorCandidate.FromModelJson(VectorFamily.Qa491Windows, "{\"family_id\":\"f\",\"chunking_identity\":7}") };
            Check(VectorCandidate.FromModelJson(VectorFamily.Qa491, "{\"family_id\":\"x\",\"chunking_identity\":320260894818558297}").ChunkingIdentity
                  == 320260894818558297UL, "model.json chunking identity exact");

            string setDir = Path.Combine(dir, "vectors");
            var locator = new VectorReleaseLocator(apiBase: gh.Base);
            var updater = new VectorSetUpdater(setDir, locator);
            var target = locator.FindForLibraryVersionAsync(1, candidates).GetAwaiter().GetResult()!;
            Check(target != null && target.Files.Count == 2, "release found, two parts");
            // a half download of the first part is resumed
            var p0 = target!.Files[0];
            Directory.CreateDirectory(updater.Incoming);
            File.WriteAllBytes(Path.Combine(updater.Incoming, p0.Name + ".partial"), gh.Assets[$"/dl/vectors-qa491-windows-v1-20261101/{p0.Name}"].Take(10).ToArray());
            File.WriteAllText(Path.Combine(updater.Incoming, p0.Name + ".partial.id"), $"{p0.AssetId}:{p0.Sha256}:{p0.Size}");
            var r1 = updater.UpdateAsync(1, candidates).GetAwaiter().GetResult()!;
            Check(gh.Ranges.SequenceEqual(new[] { "bytes=10-" }), "resumed with a Range request");
            Check(r1.Kind == "base" && r1.Generation == 1 && r1.LibraryVersion == 1, "base installed");
            Check(!Directory.EnumerateFileSystemEntries(updater.Incoming).Any(), "incoming emptied");
            Check(updater.UpdateAsync(1, candidates).GetAwaiter().GetResult()!.AlreadyApplied, "already applied");

            var r3 = updater.UpdateAsync(3, candidates).GetAwaiter().GetResult()!;
            Check(r3.Kind == "delta" && r3.Generation == 3 && r3.LibraryVersion == 3 && r3.Segments == 3 && r3.TombstonesApplied == 1,
                $"delta chain applied ({r3.Kind} gen {r3.Generation} v{r3.LibraryVersion})");
            Check(File.Exists(Path.Combine(setDir, "gen-000003", new string('3', 32) + ".links")) && !Directory.Exists(Path.Combine(setDir, "gen-000001")),
                "generation 3 with links; generation 1 collected");
            var set = VectorSet.CurrentSet(setDir)!;
            Check((double)set["library_version"]! == 3, "set at v3");

            // a corrupted block is refused
            string plain = Path.Combine(dir, "b.oxv");
            SeforimSemantic.ExpandOxv(Path.Combine(fixtures!, "base-v1.oxv.zst"), plain);
            string manifest = File.ReadAllText(Path.Combine(fixtures!, "base-v1.manifest.json"));
            Check(VectorSet.Verify(plain, manifest).ManifestChecked, "verified against its manifest");
            byte[] bytes = File.ReadAllBytes(plain);
            int sections = BitConverter.ToInt32(bytes, 28);
            int at = Enumerable.Range(0, sections).Select(i => 224 + i * 40).First(e => BitConverter.ToUInt16(bytes, e) == 0x0012);
            bytes[BitConverter.ToInt64(bytes, at + 8)] ^= 0x40;
            string bad = Path.Combine(dir, "bad.oxv");
            File.WriteAllBytes(bad, bytes);
            Check(Throws<SeforimEngineException>(() => VectorSet.Verify(bad)), "corrupted block refused");

            // the release must be of the query model; a cancelled install leaves nothing
            string other = Path.Combine(dir, "other-set");
            Check(Throws<SeforimEngineException>(() => VectorSet.Install(other, plain, manifest, null, "{\"family_id\":\"g\",\"chunking_identity\":7}")),
                "another query model refused");
            using (var cts = new CancellationTokenSource())
            {
                cts.Cancel();
                Check(Throws<OperationCanceledException>(() => VectorSet.Install(other, plain, manifest, null, null, cts.Token)), "cancelled");
            }
            Check(!File.Exists(Path.Combine(other, "CURRENT")) && !Directory.Exists(Path.Combine(other, "staging")), "nothing installed");
        }
    }
}
