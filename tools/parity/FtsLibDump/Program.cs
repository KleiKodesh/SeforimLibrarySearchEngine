using System.Text;
using KitveiHakodesh.Core.SeforimDbFullTextSearch;

// Usage: FtsLibDump <seforim.db> <ftslib index dir> <queries file> <out dir> [maxHitsPerQuery]
// Queries file: one query per line; optional "\t<maxWordDistance>" suffix.
// Writes <out dir>/<n>.jsonl per query: one hit per line, catalog order.

Console.OutputEncoding = Encoding.UTF8;
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
