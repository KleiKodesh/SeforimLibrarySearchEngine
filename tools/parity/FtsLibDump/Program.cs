using System.Text;
using KitveiHakodesh.Core.SeforimDbFullTextSearch;

// Usage: FtsLibDump <seforim.db> <ftslib index dir> <queries file> <out dir> [maxHitsPerQuery]
// Queries file: one query per line; optional "\t<maxWordDistance>" suffix.
// Writes <out dir>/<n>.jsonl per query: one hit per line, catalog order.

Console.OutputEncoding = Encoding.UTF8;

// FtsLibDump build <seforim.db> <new index dir>
//   Builds an FtsLib index with Core's own indexer (the app's path: background
//   priority for the lines phase, as the app runs it) and prints each stage's
//   time and the index size per file, in the same shape as SeforimSearch.Demo build.
if (args.Length > 0 && args[0] == "build")
{
    string bdb = args[1], bindex = args[2];
    var total = System.Diagnostics.Stopwatch.StartNew();
    var stage = System.Diagnostics.Stopwatch.StartNew();
    long lastShown = 0;
    using (var bengine = new SeforimDbFtsEngine(bindex, bdb))
    {
        var indexer = new SeforimDbFtsIndexer(bindex, bdb);
        indexer.Build(bengine,
            onProgress: (done, all) =>
            {
                if (done - lastShown >= 1_000_000 || done == all)
                {
                    Console.Error.WriteLine($"  indexed {done:N0} / {all:N0}  ({total.Elapsed.TotalSeconds:F1}s)");
                    lastShown = done;
                }
            },
            onMergeProgress: (doneSteps, steps) =>
            {
                string name = doneSteps switch
                {
                    0 => "lines (read DB + tokenize + FTS5 insert, background priority)",
                    1 => "compact (FTS5 optimize + VACUUM)",
                    _ => "term dictionary (terms.db + FM sidecar + trigram sidecar)",
                };
                Console.Error.WriteLine($"  stage: {name}: {stage.Elapsed.TotalSeconds:F1}s");
                stage.Restart();
            });
    }
    Console.Error.WriteLine($"build: complete in {total.Elapsed.TotalSeconds:F1}s");
    var files = new DirectoryInfo(bindex).GetFiles();
    Console.Error.WriteLine($"index size: {files.Sum(f => f.Length) / 1e6:F1} MB in {files.Length} files");
    foreach (var f in files.OrderByDescending(f => f.Length))
        Console.Error.WriteLine($"  {f.Name,-16} {f.Length / 1e6,9:F1} MB");
    return;
}

// FtsLibDump search <seforim.db> <index dir> [--distance N] <query words...>
//   Times one search through Core's searcher, reading EVERY hit (the same loop as
//   SeforimSearch.Demo search), and prints "search: N hits in X ms (first batch Y ms)".
if (args.Length > 0 && args[0] == "search")
{
    string sdb = args[1], sindex = args[2];
    int dist = int.MaxValue;
    var words = new List<string>();
    for (int i = 3; i < args.Length; i++)
    {
        if (args[i] == "--distance") { dist = int.Parse(args[++i]); continue; }
        words.Add(args[i]);
    }
    var open = System.Diagnostics.Stopwatch.StartNew();
    using var sengine = new SeforimDbFtsEngine(sindex, sdb);
    var sw = System.Diagnostics.Stopwatch.StartNew();
    TimeSpan? first = null;
    int hits = 0;
    foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(sengine, string.Join(" ", words), dist, false,
                 SeforimDbFtsSearcher.DefaultContextWords, false, new FtsTermTable()))
    {
        first ??= sw.Elapsed;
        hits += batch.Hits.Count;
    }
    Console.Error.WriteLine($"search: {hits:N0} hits in {sw.Elapsed.TotalMilliseconds:F0} ms " +
                            $"(first batch {first?.TotalMilliseconds:F0} ms)");
    return;
}

string db = args[0], index = args[1], queries = args[2], outDir = args[3];
int maxHits = args.Length > 4 ? int.Parse(args[4]) : 2000;
Directory.CreateDirectory(outDir);

using var engine = new SeforimDbFtsEngine(index, db);
var searcher = new SeforimDbFtsSearcher();
int n = 0;
foreach (var raw in File.ReadAllLines(queries, Encoding.UTF8))
{
    if (string.IsNullOrWhiteSpace(raw) || raw.StartsWith("#")) continue;
    var parts = raw.Split('\t');
    string query = parts[0];
    int distance = parts.Length > 1 ? int.Parse(parts[1]) : int.MaxValue;
    var terms = new FtsTermTable();
    var sw = System.Diagnostics.Stopwatch.StartNew();
    int count = 0;
    using var w = new StreamWriter(Path.Combine(outDir, $"{n}.jsonl"), false, new UTF8Encoding(false));
    w.WriteLine(Header(query, distance));
    foreach (var batch in searcher.SearchInBatches(engine, query, distance, false,
                 SeforimDbFtsSearcher.DefaultContextWords, false, terms))
    {
        var all = terms.All();
        foreach (var hit in batch.Hits)
        {
            if (count++ >= maxHits) break;
            w.WriteLine(HitJson(hit, all));
        }
        if (count >= maxHits) break;
    }
    Console.Error.WriteLine($"[ftslib] #{n} \"{query}\" dist={distance}: {count} hits in {sw.ElapsedMilliseconds} ms");
    n++;
}

static string Header(string q, int d) => "{\"query\":" + Str(q) + ",\"distance\":" + d + "}";

static string HitJson(FtsHit h, List<string> terms)
{
    var sb = new StringBuilder();
    sb.Append("{\"line\":").Append(h.LineId)
      .Append(",\"book\":").Append(h.BookId)
      .Append(",\"toc\":").Append(h.TocEntryId)
      .Append(",\"dist\":").Append(h.WordDistance)
      .Append(",\"start\":").Append(h.SnippetStart)
      .Append(",\"end\":").Append(h.SnippetEnd)
      .Append(",\"flags\":").Append(h.SnippetFlags)
      .Append(",\"lines\":[").Append(string.Join(",", h.SnippetLineIds)).Append("]")
      .Append(",\"marks\":[");
    for (int i = 0; i + 2 < h.SnippetMarks.Length; i += 3)
    {
        if (i > 0) sb.Append(',');
        sb.Append('[').Append(h.SnippetMarks[i]).Append(',').Append(h.SnippetMarks[i + 1]).Append(',')
          .Append(Str(terms[h.SnippetMarks[i + 2]])).Append(']');
    }
    return sb.Append("]}").ToString();
}

static string Str(string s)
{
    var sb = new StringBuilder("\"");
    foreach (char c in s)
        sb.Append(c switch { '"' => "\\\"", '\\' => "\\\\", _ => c < 0x20 ? $"\\u{(int)c:x4}" : c.ToString() });
    return sb.Append('"').ToString();
}
