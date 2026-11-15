using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Text.RegularExpressions;
using Microsoft.ML.OnnxRuntime;
using Microsoft.ML.OnnxRuntime.Tensors;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// The HOST side of semantic search (issue #18): embeds a query with Otzaria's Meivin model
    /// exactly as Otzaria does (otzaria-semantic-search, src/semantic/recipe.rs + onnx_backend.rs):
    /// text = "[QUERY] " + query.Trim() (normalization recipe 1 = as supplied, text recipe 2 =
    /// role prefix); tokenizer.json with special tokens; right truncation to query_max_tokens
    /// in total; input_ids + attention_mask (ones), int64 [1, len]; token_type_ids zeros only if
    /// the graph declares it; the first output [1, dim]; L2-normalized. The model never enters
    /// the engine: the engine receives the finished vector.
    /// </summary>
    internal sealed class MeivinEmbedder : IDisposable
    {
        public const string QueryPrefix = "[QUERY] ";
        private readonly InferenceSession _session;
        private readonly bool _tokenTypes;
        private readonly string _output;
        public UnigramTokenizer Tokenizer { get; }
        public int Dims { get; }
        public int MaxTokens { get; }

        public MeivinEmbedder(string modelDir)
        {
            using (var doc = JsonDocument.Parse(File.ReadAllText(Path.Combine(modelDir, "model.json"))))
            {
                Dims = doc.RootElement.GetProperty("embedding_dim").GetInt32();
                MaxTokens = doc.RootElement.TryGetProperty("query_max_tokens", out var q) ? q.GetInt32()
                          : doc.RootElement.GetProperty("max_tokens").GetInt32();
            }
            Tokenizer = UnigramTokenizer.Load(Path.Combine(modelDir, "tokenizer.json"));
            string graph = Path.Combine(modelDir, "query.int8.onnx");
            if (!File.Exists(graph)) graph = Path.Combine(modelDir, "query.fp32.onnx");
            using var opts = new SessionOptions { IntraOpNumThreads = 2 };
            _session = new InferenceSession(graph, opts);
            _tokenTypes = _session.InputMetadata.ContainsKey("token_type_ids");
            _output = _session.OutputMetadata.Keys.First();
        }

        /// <summary>The model text for a query (Otzaria's query_input).</summary>
        public static string QueryText(string query)
        {
            string t = (query ?? "").Trim();
            if (t.Length == 0) throw new ArgumentException("the query holds no text, nothing to embed");
            return QueryPrefix + t;
        }

        public long[] QueryIds(string query) => Tokenizer.Encode(QueryText(query), MaxTokens);

        public float[] EmbedQuery(string query) => Embed(QueryIds(query));

        public float[] Embed(long[] ids)
        {
            int len = ids.Length;
            var inputs = new List<NamedOnnxValue>
            {
                NamedOnnxValue.CreateFromTensor("input_ids", new DenseTensor<long>(ids, new[] { 1, len })),
                NamedOnnxValue.CreateFromTensor("attention_mask", new DenseTensor<long>(Enumerable.Repeat(1L, len).ToArray(), new[] { 1, len })),
            };
            if (_tokenTypes)
                inputs.Add(NamedOnnxValue.CreateFromTensor("token_type_ids", new DenseTensor<long>(new long[len], new[] { 1, len })));
            using var results = _session.Run(inputs, new[] { _output });
            float[] v = results.First().AsEnumerable<float>().Take(Dims).ToArray();
            if (v.Length != Dims) throw new InvalidOperationException($"model output has {v.Length} values, expected {Dims}");
            double norm = Math.Sqrt(v.Sum(x => (double)x * x));
            if (norm > 0) for (int i = 0; i < v.Length; i++) v[i] = (float)(v[i] / norm);
            return v;
        }

        public void Dispose() => _session.Dispose();
    }

    /// <summary>
    /// A small port of HuggingFace tokenizers' encode for a Unigram tokenizer.json, as Otzaria
    /// configures it (padding off, special tokens matched in the text): added tokens split out
    /// first (leftmost-longest on the raw text), the normalizer (NFKC + Replace steps read from
    /// the JSON), the Metaspace pre-tokenizer (replace ' ', prepend always, split before each
    /// replacement), Unigram Viterbi over the vocab scores (unk = min score - 10 per uncovered
    /// char, consecutive unk fused), then the template [CLS] A [SEP] with right truncation.
    /// </summary>
    internal sealed class UnigramTokenizer
    {
        private readonly Dictionary<string, (int Id, double Score)> _pieces = new Dictionary<string, (int, double)>(StringComparer.Ordinal);
        private readonly List<(string Content, int Id)> _added = new List<(string, int)>();
        private readonly List<Func<string, string>> _normalizers = new List<Func<string, string>>();
        private int _maxPieceLen;
        private double _unkScore;
        private int _unkId;
        private char _replacement = '▁';
        private bool _prependAlways = true;
        private bool _split = true;
        private int[] _prefixIds = Array.Empty<int>();
        private int[] _suffixIds = Array.Empty<int>();

        public static UnigramTokenizer Load(string path)
        {
            var t = new UnigramTokenizer();
            using var doc = JsonDocument.Parse(File.ReadAllText(path, Encoding.UTF8));
            var root = doc.RootElement;
            var model = root.GetProperty("model");
            if (model.GetProperty("type").GetString() != "Unigram") throw new NotSupportedException("tokenizer model is not Unigram");
            if (model.TryGetProperty("byte_fallback", out var bf) && bf.ValueKind == JsonValueKind.True)
                throw new NotSupportedException("byte_fallback is not supported");
            t._unkId = model.GetProperty("unk_id").GetInt32();
            double min = double.MaxValue;
            int id = 0;
            foreach (var p in model.GetProperty("vocab").EnumerateArray())
            {
                string piece = p[0].GetString()!;
                double score = p[1].GetDouble();
                if (!t._pieces.ContainsKey(piece)) t._pieces[piece] = (id, score);
                t._maxPieceLen = Math.Max(t._maxPieceLen, piece.Length);
                min = Math.Min(min, score);
                id++;
            }
            t._unkScore = min - 10.0;
            foreach (var a in root.GetProperty("added_tokens").EnumerateArray())
            {
                if (a.TryGetProperty("normalized", out var n) && n.ValueKind == JsonValueKind.True)
                    throw new NotSupportedException("normalized added tokens are not supported");
                t._added.Add((a.GetProperty("content").GetString()!, a.GetProperty("id").GetInt32()));
            }
            t._added.Sort((x, y) => y.Content.Length.CompareTo(x.Content.Length));   // longest first
            if (root.TryGetProperty("normalizer", out var norm) && norm.ValueKind == JsonValueKind.Object) t.AddNormalizer(norm);
            if (root.TryGetProperty("pre_tokenizer", out var pre) && pre.ValueKind == JsonValueKind.Object)
            {
                if (pre.GetProperty("type").GetString() != "Metaspace") throw new NotSupportedException("pre_tokenizer is not Metaspace");
                t._replacement = pre.GetProperty("replacement").GetString()![0];
                string scheme = pre.TryGetProperty("prepend_scheme", out var ps) ? ps.GetString()! : "always";
                if (scheme != "always") throw new NotSupportedException("prepend_scheme " + scheme);
                t._split = !pre.TryGetProperty("split", out var sp) || sp.ValueKind == JsonValueKind.True;
            }
            if (root.TryGetProperty("post_processor", out var post) && post.ValueKind == JsonValueKind.Object)
            {
                if (post.GetProperty("type").GetString() != "TemplateProcessing") throw new NotSupportedException("post_processor");
                var special = post.GetProperty("special_tokens");
                var before = new List<int>();
                var after = new List<int>();
                bool seenA = false;
                foreach (var item in post.GetProperty("single").EnumerateArray())
                {
                    if (item.TryGetProperty("Sequence", out _)) { seenA = true; continue; }
                    string sid = item.GetProperty("SpecialToken").GetProperty("id").GetString()!;
                    var ids = special.GetProperty(sid).GetProperty("ids").EnumerateArray().Select(e => e.GetInt32());
                    (seenA ? after : before).AddRange(ids);
                }
                t._prefixIds = before.ToArray();
                t._suffixIds = after.ToArray();
            }
            return t;
        }

        private void AddNormalizer(JsonElement n)
        {
            switch (n.GetProperty("type").GetString())
            {
                case "Sequence":
                    foreach (var c in n.GetProperty("normalizers").EnumerateArray()) AddNormalizer(c);
                    break;
                case "NFKC": _normalizers.Add(s => s.Normalize(NormalizationForm.FormKC)); break;
                case "NFC": _normalizers.Add(s => s.Normalize(NormalizationForm.FormC)); break;
                case "NFKD": _normalizers.Add(s => s.Normalize(NormalizationForm.FormKD)); break;
                case "NFD": _normalizers.Add(s => s.Normalize(NormalizationForm.FormD)); break;
                case "Replace":
                {
                    var pat = n.GetProperty("pattern");
                    string content = n.GetProperty("content").GetString()!;
                    if (pat.TryGetProperty("Regex", out var rx))
                    {
                        var re = new Regex(rx.GetString()!, RegexOptions.CultureInvariant);
                        _normalizers.Add(s => re.Replace(s, content.Replace("$", "$$")));
                    }
                    else
                    {
                        string from = pat.GetProperty("String").GetString()!;
                        _normalizers.Add(s => s.Replace(from, content));
                    }
                    break;
                }
                default: throw new NotSupportedException("normalizer " + n.GetProperty("type").GetString());
            }
        }

        public string Normalize(string s)
        {
            foreach (var f in _normalizers) s = f(s);
            return s;
        }

        /// <summary>Ids with the template's special tokens, the content truncated on the right so
        /// the total is at most <paramref name="maxTokens"/> (0 = no cap).</summary>
        public long[] Encode(string text, int maxTokens = 0)
        {
            var content = EncodeContent(text);
            int room = maxTokens > 0 ? Math.Max(0, maxTokens - _prefixIds.Length - _suffixIds.Length) : int.MaxValue;
            if (content.Count > room) content.RemoveRange(room, content.Count - room);
            return _prefixIds.Concat(content).Concat(_suffixIds).Select(i => (long)i).ToArray();
        }

        /// <summary>Content ids, no specials, no truncation.</summary>
        public List<int> EncodeContent(string text)
        {
            var ids = new List<int>();
            int start = 0, i = 0;
            while (i < text.Length)
            {
                int hit = -1;
                foreach (var (c, id) in _added)
                    if (i + c.Length <= text.Length && string.CompareOrdinal(text, i, c, 0, c.Length) == 0) { hit = id; EncodeSegment(text.Substring(start, i - start), ids); ids.Add(id); i += c.Length; start = i; break; }
                if (hit < 0) i++;
            }
            EncodeSegment(text.Substring(start), ids);
            return ids;
        }

        private void EncodeSegment(string segment, List<int> ids)
        {
            if (segment.Length == 0) return;
            string s = Normalize(segment);
            if (s.Length == 0) return;
            s = s.Replace(' ', _replacement);
            if (_prependAlways && s[0] != _replacement) s = _replacement + s;
            if (!_split) { Viterbi(s, ids); return; }
            int from = 0;
            for (int k = 1; k <= s.Length; k++)
                if (k == s.Length || s[k] == _replacement) { Viterbi(s.Substring(from, k - from), ids); from = k; }
        }

        private void Viterbi(string s, List<int> ids)
        {
            int n = s.Length;
            var best = new double[n + 1];
            var back = new int[n + 1];
            var piece = new int[n + 1];
            for (int e = 1; e <= n; e++) { best[e] = double.NegativeInfinity; back[e] = -1; }
            for (int b = 0; b < n; b++)
            {
                if (b > 0 && back[b] < 0) continue;   // unreachable (cannot happen: unk covers every char)
                int charLen = char.IsHighSurrogate(s[b]) && b + 1 < n && char.IsLowSurrogate(s[b + 1]) ? 2 : 1;
                bool single = false;
                int maxLen = Math.Min(_maxPieceLen, n - b);
                for (int len = 1; len <= maxLen; len++)
                {
                    if (!_pieces.TryGetValue(s.Substring(b, len), out var p)) continue;
                    if (len == charLen) single = true;
                    Relax(b, b + len, p.Score, p.Id);
                }
                if (!single) Relax(b, b + charLen, _unkScore, _unkId);
            }
            void Relax(int b, int e, double score, int id)
            {
                double sc = best[b] + score;
                // HF keeps the first best predecessor (earliest begin = longest piece): strict >
                // with begins visited in ascending order gives the same choice.
                if (back[e] < 0 || sc > best[e]) { best[e] = sc; back[e] = b; piece[e] = id; }
            }
            var path = new List<int>();
            for (int e = n; e > 0; e = back[e]) path.Add(piece[e]);
            path.Reverse();
            bool prevUnk = false;
            foreach (int id in path)
            {
                if (id == _unkId && prevUnk) continue;   // fuse_unk: consecutive unknowns are one token
                ids.Add(id);
                prevUnk = id == _unkId;
            }
        }
    }
}
