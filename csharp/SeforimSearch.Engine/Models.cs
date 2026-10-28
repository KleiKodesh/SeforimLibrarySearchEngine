using System;
using System.Collections.Generic;
using System.Text;

namespace SeforimSearch.Engine
{
    /// <summary>Thrown when the native engine reports an error.</summary>
    public sealed class SeforimEngineException : Exception
    {
        public SeforimEngineException(string message) : base(message) { }
    }

    /// <summary>How an index is built. Fixed at creation.</summary>
    public sealed class IndexOptions
    {
        /// <summary>Store word positions. Off: a smaller index; word distance is
        /// still validated, from the line text by the snippet locator.</summary>
        public bool Positions { get; set; } = true;

        /// <summary>Keep nikud/te'amim in indexed terms (canonical order) and flag the lines
        /// that carry a mark. Every search ignores the marks unless
        /// <see cref="WalkOptions.MatchNikud"/> / <see cref="WalkOptions.MatchTeamim"/> asks
        /// for them (Otzaria's vocalized search), so default results equal the
        /// mark-stripped index's.</summary>
        public bool KeepMarks { get; set; }

        internal string ToJson() =>
            "{\"positions\":" + (Positions ? "true" : "false") +
            ",\"keep_marks\":" + (KeepMarks ? "true" : "false") + "}";
    }

    /// <summary>The state of an open index.</summary>
    public sealed class IndexInfo
    {
        public long NumLines { get; internal set; }
        public bool Positions { get; internal set; }
        public bool KeepMarks { get; internal set; }

        /// <summary>The payload committed with the last commit (resume watermark,
        /// provenance stamp, …), or null.</summary>
        public string? Payload { get; internal set; }
    }

    /// <summary>One line to index. Bare minimum: the index works in tandem with
    /// seforim.db, so it keeps only the line id, catalog position and book id (for
    /// filtered search); placement such as line index or section is read from SQLite
    /// by line id.</summary>
    public struct LineDocument
    {
        public long LineId;
        /// <summary>Rank in the library's catalog order: results are returned in it by default.</summary>
        public long Position;
        public long BookId;
        /// <summary>The raw line text (HTML allowed).</summary>
        public string Text;
        /// <summary>The host's section of the line (same-section scope; seforim.db: its TOC
        /// entry). Ids may repeat across books. Null: the line is a section of its own.</summary>
        public long? SectionId;
        /// <summary>No phrase continues into or out of this line (cross-line matching): set
        /// for headings (Otzaria's rule). The line's word count is computed by the engine.</summary>
        public bool NoCross;
    }

    /// <summary>How the typo option (שגיאות כתיב) expands a word.</summary>
    public enum TypoMode
    {
        /// <summary>Exactly Otzaria: edit distance 1 (its Levenshtein-1 scan) for a
        /// lone word of 4+ letters with typo as its only option; otherwise
        /// Otzaria's sampled list (confusions, transpositions, deletions,
        /// insertions of 13 letters; 3 letters: confusions only).</summary>
        Otzaria = 0,
        /// <summary>Edit distance 1 for every 4+-letter word in every combination
        /// (confusions only for 3 letters, nothing for 2 or fewer).</summary>
        FullEdit1 = 1,
    }

    /// <summary>Which rules the ktiv male / haser option uses.</summary>
    public enum KtivRules
    {
        /// <summary>Otzaria's vav / yod rules (default).</summary>
        Otzaria = 0,
        /// <summary>FtsLib's KetivExpander rules (cap 40), the walk's core forms.</summary>
        FtsLib = 1,
    }

    /// <summary>Hebrew walk options for a word (Otzaria's search options).</summary>
    public sealed class WalkOptions
    {
        /// <summary>קידומות דקדוקיות (Otzaria's slots: ו מ דא כש כ ב ש ל ה ד, then כש כ ב ש ל ה ד, then ה).</summary>
        public bool GrammarPrefixes { get; set; }
        /// <summary>סיומות דקדוקיות.</summary>
        public bool GrammarSuffixes { get; set; }
        /// <summary>קידומות ארמיות (ד/דא/א).</summary>
        public bool AramaicPrefixes { get; set; }
        /// <summary>סיומות ארמיות (ה↔א, ם↔ן).</summary>
        public bool AramaicSuffixes { get; set; }
        /// <summary>כתיב מלא/חסר.</summary>
        public bool KtivMaleHaser { get; set; }
        /// <summary>Which rules <see cref="KtivMaleHaser"/> uses. Default
        /// <see cref="KtivRules.Otzaria"/>; the FtsLib facade sets <see cref="KtivRules.FtsLib"/>.</summary>
        public KtivRules KtivRules { get; set; }
        /// <summary>חלק ממילה.</summary>
        public bool PartOfWord { get; set; }
        /// <summary>קידומות (מתקדם): any letters before.</summary>
        public bool AdvancedPrefixes { get; set; }
        /// <summary>סיומות (מתקדם): any letters after.</summary>
        public bool AdvancedSuffixes { get; set; }
        /// <summary>התעלם מגרשיים.</summary>
        public bool IgnoreQuotes { get; set; }
        /// <summary>Index terms must match as written. Off (default, Otzaria):
        /// a term also matches by its quote-free form, so רמבם finds רמב"ם.</summary>
        public bool ExactQuotes { get; set; }
        /// <summary>שגיאות כתיב, expanded per <see cref="TypoMode"/>.</summary>
        public bool SpellingErrors { get; set; }
        /// <summary>Which typo behaviour <see cref="SpellingErrors"/> uses. Default
        /// <see cref="TypoMode.Otzaria"/>.</summary>
        public TypoMode TypoMode { get; set; }
        /// <summary>Otzaria's match_nikud: typed nikud required on its letter, untyped marks
        /// free, only lines that carry a mark. Needs a KeepMarks index (else an error).</summary>
        public bool MatchNikud { get; set; }
        /// <summary>Otzaria's match_taamim: the same for te'amim.</summary>
        public bool MatchTeamim { get; set; }

        /// <summary>קידומות (מתקדם): max letters before the word. Null = default
        /// (3, or 5 for a word of ≤3 letters).</summary>
        public int? PrefixMaxLetters { get; set; }

        /// <summary>סיומות (מתקדם): max letters after the word. Null = default
        /// (5, or 7 for a word of ≤3 letters).</summary>
        public int? SuffixMaxLetters { get; set; }

        /// <summary>חלק ממילה: max letters on each side. Null = default
        /// (3, or 2 for a word of ≥4 letters).</summary>
        public int? PartOfWordMaxLetters { get; set; }

        /// <summary>קידומות דקדוקיות: max total letters of a stacked prefix.
        /// Null = no cap (Otzaria's full set).</summary>
        public int? GrammarPrefixMaxLetters { get; set; }

        internal void WriteJson(StringBuilder sb)
        {
            sb.Append('{');
            bool first = true;
            void Flag(string name, bool value)
            {
                if (!value) return;
                if (!first) sb.Append(',');
                first = false;
                sb.Append('"').Append(name).Append("\":true");
            }
            void Limit(string name, int? value)
            {
                if (!value.HasValue) return;
                if (!first) sb.Append(',');
                first = false;
                sb.Append('"').Append(name).Append("\":")
                  .Append(Math.Max(0, value.Value).ToString(System.Globalization.CultureInfo.InvariantCulture));
            }
            Limit("prefix_max_letters", PrefixMaxLetters);
            Limit("suffix_max_letters", SuffixMaxLetters);
            Limit("part_of_word_max_letters", PartOfWordMaxLetters);
            Limit("grammar_prefix_max_letters", GrammarPrefixMaxLetters);
            Flag("grammar_prefixes", GrammarPrefixes);
            Flag("grammar_suffixes", GrammarSuffixes);
            Flag("aramaic_prefixes", AramaicPrefixes);
            Flag("aramaic_suffixes", AramaicSuffixes);
            Flag("ktiv_male_haser", KtivMaleHaser);
            Flag("part_of_word", PartOfWord);
            Flag("advanced_prefixes", AdvancedPrefixes);
            Flag("advanced_suffixes", AdvancedSuffixes);
            Flag("ignore_quotes", IgnoreQuotes);
            Flag("exact_quotes", ExactQuotes);
            Flag("spelling_errors", SpellingErrors);
            if (TypoMode == TypoMode.FullEdit1)
            {
                if (!first) sb.Append(',');
                first = false;
                sb.Append("\"typo_mode\":\"full_edit1\"");
            }
            if (KtivRules == KtivRules.FtsLib)
            {
                if (!first) sb.Append(',');
                first = false;
                sb.Append("\"ktiv_rules\":\"ftslib\"");
            }
            Flag("match_nikud", MatchNikud);
            Flag("match_teamim", MatchTeamim);
            sb.Append('}');
        }
    }

    /// <summary>One AND slot of a query.</summary>
    public abstract class QueryGroup
    {
        internal abstract void WriteJson(StringBuilder sb);

        /// <summary>A word with walk options.</summary>
        public static QueryGroup Word(string word, WalkOptions? options = null) => new WordGroup(word, options ?? new WalkOptions());

        /// <summary>A wildcard pattern (* and ?). A rejected pattern (anchor shorter
        /// than 2, more than 4 '?') skips its slot. <paramref name="ignoreQuotes"/>:
        /// the quote is transparent (FtsLib's quote-free terms).</summary>
        public static QueryGroup Wildcard(string pattern, bool ignoreQuotes = false) => new WildcardGroup(pattern, ignoreQuotes);

        /// <summary>FtsLib's %word / word% / %word%: its GrammarExpander tables
        /// (34 stacked prefixes, 32 suffixes).</summary>
        public static QueryGroup Grammar(string word, bool prefixes, bool suffixes, bool ignoreQuotes = false)
            => new GrammarGroup(word, prefixes, suffixes, ignoreQuotes);

        private sealed class GrammarGroup : QueryGroup
        {
            private readonly string _word;
            private readonly bool _prefixes, _suffixes, _ignoreQuotes;
            public GrammarGroup(string word, bool prefixes, bool suffixes, bool ignoreQuotes)
            {
                _word = word; _prefixes = prefixes; _suffixes = suffixes; _ignoreQuotes = ignoreQuotes;
            }
            internal override void WriteJson(StringBuilder sb)
            {
                sb.Append("{\"kind\":\"grammar\",\"word\":");
                Json.WriteString(sb, _word);
                sb.Append(",\"prefixes\":").Append(_prefixes ? "true" : "false")
                  .Append(",\"suffixes\":").Append(_suffixes ? "true" : "false")
                  .Append(",\"ignore_quotes\":").Append(_ignoreQuotes ? "true" : "false").Append('}');
            }
        }

        /// <summary>A slot any of the alternatives satisfies (a | b).</summary>
        public static QueryGroup Any(params QueryGroup[] alternatives) => new AnyGroup(alternatives);

        /// <summary>Levenshtein neighbours of a word (word~N), closest first,
        /// capped at 2000. Distance 1..3 (FtsLib's MaxAllowedDistance; 3 needs the
        /// FM-index sidecar, the walk path clamps to 2). With
        /// <paramref name="ignoreQuotes"/> distances are measured on the quote-free
        /// forms, as FtsLib's quote-free terms.</summary>
        public static QueryGroup Fuzzy(string word, int distance, bool ignoreQuotes = false) =>
            new FuzzyGroup(word, distance, ignoreQuotes);

        private sealed class FuzzyGroup : QueryGroup
        {
            private readonly string _word;
            private readonly int _distance;
            private readonly bool _ignoreQuotes;
            public FuzzyGroup(string word, int distance, bool ignoreQuotes)
            {
                _word = word; _distance = distance; _ignoreQuotes = ignoreQuotes;
            }
            internal override void WriteJson(StringBuilder sb)
            {
                sb.Append("{\"kind\":\"fuzzy\",\"word\":");
                Json.WriteString(sb, _word);
                sb.Append(",\"distance\":").Append(Math.Max(1, Math.Min(3, _distance)).ToString(System.Globalization.CultureInfo.InvariantCulture));
                sb.Append(",\"ignore_quotes\":").Append(_ignoreQuotes ? "true" : "false");
                sb.Append('}');
            }
        }

        private sealed class WordGroup : QueryGroup
        {
            private readonly string _word;
            private readonly WalkOptions _options;
            public WordGroup(string word, WalkOptions options) { _word = word; _options = options; }
            internal override void WriteJson(StringBuilder sb)
            {
                sb.Append("{\"kind\":\"word\",\"word\":");
                Json.WriteString(sb, _word);
                sb.Append(",\"options\":");
                _options.WriteJson(sb);
                sb.Append('}');
            }
        }

        private sealed class WildcardGroup : QueryGroup
        {
            private readonly string _pattern;
            private readonly bool _ignoreQuotes;
            public WildcardGroup(string pattern, bool ignoreQuotes) { _pattern = pattern; _ignoreQuotes = ignoreQuotes; }
            internal override void WriteJson(StringBuilder sb)
            {
                sb.Append("{\"kind\":\"wildcard\",\"pattern\":");
                Json.WriteString(sb, _pattern);
                sb.Append(",\"ignore_quotes\":").Append(_ignoreQuotes ? "true" : "false").Append('}');
            }
        }

        private sealed class AnyGroup : QueryGroup
        {
            private readonly QueryGroup[] _alternatives;
            public AnyGroup(QueryGroup[] alternatives) { _alternatives = alternatives; }
            internal override void WriteJson(StringBuilder sb)
            {
                sb.Append("{\"kind\":\"any\",\"alternatives\":[");
                for (int i = 0; i < _alternatives.Length; i++)
                {
                    if (i > 0) sb.Append(',');
                    _alternatives[i].WriteJson(sb);
                }
                sb.Append("]}");
            }
        }
    }

    /// <summary>Otzaria's <c>WordMatchMode</c> for <see cref="SearchOptions.WordMatch"/>: how
    /// many slots a line needs, counted by the engine over the slots it searches (a word it
    /// drops is not counted). All = every slot, AnyWord = 1, MostWords = n/2 + 1, AtLeast =
    /// count (at least 1).</summary>
    public sealed class WordMatch
    {
        private readonly string _mode;
        private readonly int _count;

        private WordMatch(string mode, int count)
        {
            _mode = mode;
            _count = count;
        }

        public static WordMatch All() => new WordMatch("all", 0);
        public static WordMatch AnyWord() => new WordMatch("any_word", 0);
        public static WordMatch MostWords() => new WordMatch("most_words", 0);
        public static WordMatch AtLeast(int count) => new WordMatch("at_least", count < 0 ? 0 : count);

        internal void WriteJson(StringBuilder sb)
        {
            sb.Append("{\"mode\":\"").Append(_mode).Append('"');
            if (_mode == "at_least")
                sb.Append(",\"count\":").Append(_count.ToString(System.Globalization.CultureInfo.InvariantCulture));
            sb.Append('}');
        }
    }

    /// <summary>Otzaria's word proximity, an engine rule of its own (see the engine's
    /// <c>ProximitySpec</c>). With positions it runs in the query; without, the snippet
    /// locator validates it on the line text: both return the same lines.</summary>
    public sealed class Proximity
    {
        private readonly List<int>? _gaps;
        private readonly int _distance;

        private Proximity(List<int>? gaps, int distance)
        {
            _gaps = gaps;
            _distance = distance;
        }

        /// <summary>Groups in query order, at most gaps[i] words between group i and
        /// group i+1 (0 = adjacent); one entry per consecutive pair.</summary>
        public static Proximity Ordered(IEnumerable<int> gaps) => new Proximity(new List<int>(gaps), 0);

        /// <summary>Groups in any order, each at its own word, all inside one window of
        /// (k - 1) * (distance + 1) positions for k groups.</summary>
        public static Proximity AnyOrder(int distance) => new Proximity(null, distance);

        internal void WriteJson(StringBuilder sb)
        {
            var inv = System.Globalization.CultureInfo.InvariantCulture;
            if (_gaps == null)
            {
                sb.Append("{\"mode\":\"any_order\",\"distance\":").Append(Math.Max(0, _distance).ToString(inv)).Append('}');
                return;
            }
            sb.Append("{\"mode\":\"ordered\",\"gaps\":[");
            for (int i = 0; i < _gaps.Count; i++)
            {
                if (i > 0) sb.Append(',');
                sb.Append(Math.Max(0, _gaps[i]).ToString(inv));
            }
            sb.Append("]}");
        }
    }

    /// <summary>A search.</summary>
    public sealed class SearchRequest
    {
        /// <summary>AND across groups, OR within each group's expansion.</summary>
        public List<QueryGroup> Groups { get; } = new List<QueryGroup>();

        /// <summary>Drop hits with more words than this between the matched slots
        /// (מרחק בין מילים). Null = no limit.</summary>
        public int? MaxWordDistance { get; set; }

        /// <summary>Only windows where the groups appear in query order.</summary>
        public bool RequireOrdered { get; set; }

        /// <summary>Words of context per side of the match.</summary>
        public int ContextWords { get; set; } = 8;

        /// <summary>Per-pair spacing (Otzaria's customSpacing): Gaps[i] = max words
        /// between group i and group i+1 (0 = adjacent). Needs an index with
        /// positions; one entry per consecutive pair. Null = not used.</summary>
        public List<int>? Gaps { get; set; }

        /// <summary>Partial matching (Otzaria's wordMatchMode): a line needs only this
        /// many of the groups (anyWord = 1, mostWords = n/2 + 1, atLeast = N; clamped
        /// to 1..n). Null = all groups. Below the group count, MaxWordDistance,
        /// RequireOrdered and Gaps are not applied; the snippet is the tightest window
        /// holding that many groups.</summary>
        public int? MinGroups { get; set; }

        /// <summary>Otzaria's word proximity. When set it is the main query's distance
        /// rule: MaxWordDistance, RequireOrdered and Gaps are ignored for it (they are
        /// FtsLib's rule). MinGroups below the group count wins over it. Null = not used.</summary>
        public Proximity? Proximity { get; set; }

        /// <summary>Otzaria's negative query: lines carrying these words are dropped.
        /// Null = none.</summary>
        public ExcludeSpec? Exclude { get; set; }

        /// <summary>Whole-phrase alternatives ORed with <see cref="Groups"/> (Otzaria's
        /// acronym expansion: רמב"ם ↔ רבי משה בן מיימון). Each phrase's words are adjacent,
        /// in order and all required, whatever the main query's settings.</summary>
        public List<List<QueryGroup>> PhraseAlternatives { get; } = new List<List<QueryGroup>>();

        /// <summary>Filtered search by book: only lines whose book id is in this list come
        /// back (empty list = no hits). Null = every book.</summary>
        public List<long>? Books { get; set; }

        internal string ToJson()
        {
            var sb = new StringBuilder();
            sb.Append("{\"groups\":[");
            for (int i = 0; i < Groups.Count; i++)
            {
                if (i > 0) sb.Append(',');
                Groups[i].WriteJson(sb);
            }
            sb.Append("],\"max_word_distance\":");
            sb.Append(MaxWordDistance.HasValue ? MaxWordDistance.Value.ToString(System.Globalization.CultureInfo.InvariantCulture) : "null");
            sb.Append(",\"require_ordered\":").Append(RequireOrdered ? "true" : "false");
            sb.Append(",\"context_words\":").Append(ContextWords.ToString(System.Globalization.CultureInfo.InvariantCulture));
            if (Gaps != null)
            {
                sb.Append(",\"gaps\":[");
                for (int i = 0; i < Gaps.Count; i++)
                {
                    if (i > 0) sb.Append(',');
                    sb.Append(Math.Max(0, Gaps[i]).ToString(System.Globalization.CultureInfo.InvariantCulture));
                }
                sb.Append(']');
            }
            if (MinGroups.HasValue)
                sb.Append(",\"min_groups\":").Append(MinGroups.Value.ToString(System.Globalization.CultureInfo.InvariantCulture));
            if (Proximity != null)
            {
                sb.Append(",\"proximity\":");
                Proximity.WriteJson(sb);
            }
            WritePhrases(sb, PhraseAlternatives);
            if (Books != null)
            {
                sb.Append(",\"books\":[");
                for (int i = 0; i < Books.Count; i++)
                {
                    if (i > 0) sb.Append(',');
                    sb.Append(Books[i].ToString(System.Globalization.CultureInfo.InvariantCulture));
                }
                sb.Append(']');
            }
            if (Exclude != null)
            {
                sb.Append(",\"exclude\":");
                Exclude.WriteJson(sb);
            }
            sb.Append('}');
            return sb.ToString();
        }

        /// <summary>Writes ",\"phrase_alternatives\":[[group, ...], ...]" when there are any.</summary>
        internal static void WritePhrases(StringBuilder sb, List<List<QueryGroup>> phrases)
        {
            if (phrases.Count == 0) return;
            sb.Append(",\"phrase_alternatives\":[");
            for (int p = 0; p < phrases.Count; p++)
            {
                if (p > 0) sb.Append(',');
                sb.Append('[');
                for (int i = 0; i < phrases[p].Count; i++)
                {
                    if (i > 0) sb.Append(',');
                    phrases[p][i].WriteJson(sb);
                }
                sb.Append(']');
            }
            sb.Append(']');
        }
    }

    /// <summary>Otzaria's negative query: the words to exclude, run as a query of their
    /// own with its own distance, order and spacing, always with every word required.
    /// A line matching it is dropped. Several words bound by
    /// <see cref="MaxWordDistance"/> / <see cref="RequireOrdered"/> (without Gaps) are
    /// checked on the line text when a snippet is located, so such lines are still
    /// among the hits and locate as null. Section-scoped exclusion is the host's job:
    /// search each negative word on its own, map the returned line ids to sections with
    /// SQLite (line_toc), intersect, and drop the positive hits in those sections.</summary>
    public sealed class ExcludeSpec
    {
        /// <summary>AND across groups, OR within each group's expansion.</summary>
        public List<QueryGroup> Groups { get; } = new List<QueryGroup>();

        /// <summary>Words allowed between the excluded words. Null = anywhere in the line.</summary>
        public int? MaxWordDistance { get; set; }

        /// <summary>The excluded words must appear in this order.</summary>
        public bool RequireOrdered { get; set; }

        /// <summary>Per-pair spacing for the excluded words (needs positions).</summary>
        public List<int>? Gaps { get; set; }

        /// <summary>Otzaria's proximity for the excluded words; when set, MaxWordDistance,
        /// RequireOrdered and Gaps are ignored. Null = not used.</summary>
        public Proximity? Proximity { get; set; }

        /// <summary>Whole phrases ORed with the negative query (Otzaria's acronym expansion),
        /// each as <see cref="SearchRequest.PhraseAlternatives"/>: adjacent, in order, all
        /// words. A line carrying one is dropped.</summary>
        public List<List<QueryGroup>> PhraseAlternatives { get; } = new List<List<QueryGroup>>();

        internal void WriteJson(StringBuilder sb)
        {
            sb.Append("{\"groups\":[");
            for (int i = 0; i < Groups.Count; i++)
            {
                if (i > 0) sb.Append(',');
                Groups[i].WriteJson(sb);
            }
            sb.Append("],\"max_word_distance\":");
            sb.Append(MaxWordDistance.HasValue ? MaxWordDistance.Value.ToString(System.Globalization.CultureInfo.InvariantCulture) : "null");
            sb.Append(",\"require_ordered\":").Append(RequireOrdered ? "true" : "false");
            if (Gaps != null)
            {
                sb.Append(",\"gaps\":[");
                for (int i = 0; i < Gaps.Count; i++)
                {
                    if (i > 0) sb.Append(',');
                    sb.Append(Math.Max(0, Gaps[i]).ToString(System.Globalization.CultureInfo.InvariantCulture));
                }
                sb.Append(']');
            }
            if (Proximity != null)
            {
                sb.Append(",\"proximity\":");
                Proximity.WriteJson(sb);
            }
            SearchRequest.WritePhrases(sb, PhraseAlternatives);
            sb.Append('}');
        }
    }

    /// <summary>The order of the hits (Otzaria's ResultsOrder).</summary>
    public enum ResultsOrder
    {
        /// <summary>Catalog order, as FtsLib (the default; costs nothing).</summary>
        Catalog = 0,
        /// <summary>Otzaria's relevance: best score first, ties in catalog order. Phrase,
        /// proximity and multi-word fuzzy queries need an index with positions; without,
        /// they come back in catalog order with the notice
        /// <see cref="SearchInfo.RelevanceNeedsPositions"/> (<see cref="SearchSession.GetInfo"/>).</summary>
        Relevance = 1,
        /// <summary>Otzaria's generation order: by each book's generation order
        /// (<see cref="SearchOptions.BookGenerations"/>), then catalog order.</summary>
        Generation = 2,
    }

    /// <summary>
    /// THE search API: the query string (FtsLib syntax: words AND-ed, <c>a | b</c> one
    /// slot, <c>*</c> <c>?</c> wildcard, <c>word~N</c> fuzzy, <c>%word</c> / <c>word%</c>
    /// FtsLib's grammar tables) and the word options, which apply to every word unless
    /// <see cref="Words"/> overrides them for one word (Otzaria's wordOptions /
    /// alternativeWords). The engine builds one walk per word with all its options.
    /// <see cref="SearchRequest"/> is the low-level form it lowers to.
    /// </summary>
    public sealed class SearchOptions
    {
        public SearchOptions() { }
        public SearchOptions(string query) { Query = query; }

        public string Query { get; set; } = "";

        // word options (every word)
        /// <summary>grammar prefixes (קידומות דקדוקיות).</summary>
        public bool GrammarPrefixes { get; set; }
        /// <summary>סיומות דקדוקיות.</summary>
        public bool GrammarSuffixes { get; set; }
        /// <summary>קידומות ארמיות.</summary>
        public bool AramaicPrefixes { get; set; }
        /// <summary>סיומות ארמיות.</summary>
        public bool AramaicSuffixes { get; set; }
        /// <summary>כתיב מלא/חסר.</summary>
        public bool KtivMaleHaser { get; set; }
        /// <summary>Which rules <see cref="KtivMaleHaser"/> uses. Default
        /// <see cref="KtivRules.Otzaria"/>; the FtsLib facade sets <see cref="KtivRules.FtsLib"/>.</summary>
        public KtivRules KtivRules { get; set; }
        /// <summary>חלק ממילה.</summary>
        public bool PartOfWord { get; set; }
        /// <summary>קידומות (any letters before).</summary>
        public bool AdvancedPrefixes { get; set; }
        /// <summary>סיומות (any letters after).</summary>
        public bool AdvancedSuffixes { get; set; }
        /// <summary>התעלם מגרשיים (also for wildcard / fuzzy / % words).</summary>
        public bool IgnoreQuotes { get; set; }
        /// <summary>Index terms must match as written (off: also by their quote-free form).</summary>
        public bool ExactQuotes { get; set; }
        /// <summary>שגיאות כתיב, expanded per <see cref="TypoMode"/>.</summary>
        public bool SpellingErrors { get; set; }
        /// <summary>Otzaria's match_nikud: typed nikud required, untyped marks free, only
        /// lines that carry a mark (needs a KeepMarks index).</summary>
        public bool MatchNikud { get; set; }
        /// <summary>Otzaria's match_taamim: the same for te'amim.</summary>
        public bool MatchTeamim { get; set; }
        public TypoMode TypoMode { get; set; }
        /// <summary>Limits (Otzaria). Null = the walk's defaults.</summary>
        public int? PrefixMaxLetters { get; set; }
        public int? SuffixMaxLetters { get; set; }
        public int? PartOfWordMaxLetters { get; set; }
        public int? GrammarPrefixMaxLetters { get; set; }

        /// <summary>Per-word overrides by word index: the place, from 0, of the word as typed
        /// (<see cref="Query"/> cut on whitespace and <c>|</c>; <c>|</c> is not a word; a word
        /// the engine drops keeps its place). A typed word the separators split (maqaf,
        /// punctuation, Hebrew/Latin) gives its override to every part.</summary>
        public Dictionary<int, WordOptions> Words { get; } = new Dictionary<int, WordOptions>();

        // query level
        /// <summary>FtsLib's word distance. Null = no limit.</summary>
        public int? MaxWordDistance { get; set; }
        /// <summary>FtsLib: the words in query order.</summary>
        public bool RequireOrdered { get; set; }
        /// <summary>Partial matching as a slot count: slots a line needs. Null = all.
        /// WordMatch wins when set.</summary>
        public int? MinGroups { get; set; }
        /// <summary>Partial matching (Otzaria's wordMatchMode), counted by the engine over the
        /// slots it searches.</summary>
        public WordMatch? WordMatch { get; set; }
        /// <summary>Words of context per side of the snippet.</summary>
        public int ContextWords { get; set; } = 8;
        /// <summary>Otzaria's proximity, per TYPED word (gaps[i] between typed words i and i+1,
        /// words the engine drops included: it cuts them out and merges the gaps around them).
        /// When set, MaxWordDistance / RequireOrdered are not used.</summary>
        public Proximity? Proximity { get; set; }
        /// <summary>Otzaria's negative query: its words take the word options above as their
        /// default and have their own per-word overrides (<see cref="ExcludeOptions.Words"/>).</summary>
        public ExcludeOptions? Exclude { get; set; }
        /// <summary>Filtered search: only these book ids. Null = every book.</summary>
        public List<long>? Books { get; set; }
        /// <summary>Whole phrases ORed with the query (Otzaria's acronym expansion).</summary>
        public List<PhraseOptions> PhraseAlternatives { get; } = new List<PhraseOptions>();
        /// <summary>Otzaria's user regex: a query wrapped in /.../ is a regex. Off (the
        /// default, FtsLib's behaviour): the query is plain text.</summary>
        public bool UserRegex { get; set; }
        /// <summary>The order of the hits. Catalog by default.</summary>
        public ResultsOrder Order { get; set; }
        /// <summary>For <see cref="ResultsOrder.Generation"/>: book id to its generation order
        /// (Otzaria's generationOrder, from the host). Books absent come last; negative
        /// ids (no such book) are not sent.</summary>
        public Dictionary<long, int> BookGenerations { get; } = new Dictionary<long, int>();
        /// <summary>Cross-line matching: a match may continue from the end of a line onto the
        /// start of the next line of the same book (one break at most). Such a hit has
        /// <see cref="PlacedHit.NextLineId"/>; locate it with
        /// <see cref="SearchSession.LocatePairs"/>. Off by default (FtsLib).</summary>
        public bool CrossLine { get; set; }
        /// <summary>Otzaria's SameSection scope: every word (or WordMatch's count) anywhere in
        /// one section; the hits are that section's lines holding a word.</summary>
        public bool SameSection { get; set; }
        /// <summary>Resume (FtsLib's afterPosition): only lines at catalog positions strictly
        /// after this one (<see cref="PlacedHit.Position"/>, 0-based). Null: from the start.
        /// Totals and counts are then those of the rest.</summary>
        public long? AfterPosition { get; set; }

        internal string ToJson()
        {
            var inv = System.Globalization.CultureInfo.InvariantCulture;
            var sb = new StringBuilder("{\"query\":");
            Json.WriteString(sb, Query ?? "");
            if (AfterPosition.HasValue) sb.Append(",\"after_position\":").Append(AfterPosition.Value.ToString(inv));
            if (UserRegex) sb.Append(",\"user_regex\":true");
            if (CrossLine) sb.Append(",\"cross_line\":true");
            if (SameSection) sb.Append(",\"same_section\":true");
            WordFields(sb, GrammarPrefixes, GrammarSuffixes, AramaicPrefixes, AramaicSuffixes, KtivMaleHaser,
                PartOfWord, AdvancedPrefixes, AdvancedSuffixes, IgnoreQuotes, ExactQuotes, SpellingErrors,
                MatchNikud, MatchTeamim, TypoMode, PrefixMaxLetters, SuffixMaxLetters, PartOfWordMaxLetters,
                GrammarPrefixMaxLetters, KtivRules);
            WriteWords(sb, Words);
            if (MaxWordDistance.HasValue) sb.Append(",\"max_word_distance\":").Append(MaxWordDistance.Value.ToString(inv));
            if (RequireOrdered) sb.Append(",\"require_ordered\":true");
            if (MinGroups.HasValue) sb.Append(",\"min_groups\":").Append(MinGroups.Value.ToString(inv));
            if (WordMatch != null) { sb.Append(",\"word_match\":"); WordMatch.WriteJson(sb); }
            sb.Append(",\"context_words\":").Append(ContextWords.ToString(inv));
            if (Proximity != null) { sb.Append(",\"proximity\":"); Proximity.WriteJson(sb); }
            if (Exclude != null) { sb.Append(",\"exclude\":"); Exclude.WriteJson(sb); }
            if (Books != null)
            {
                sb.Append(",\"books\":[");
                for (int i = 0; i < Books.Count; i++)
                {
                    if (i > 0) sb.Append(',');
                    sb.Append(Books[i].ToString(inv));
                }
                sb.Append(']');
            }
            if (PhraseAlternatives.Count > 0)
            {
                sb.Append(",\"phrase_alternatives\":[");
                for (int i = 0; i < PhraseAlternatives.Count; i++)
                {
                    if (i > 0) sb.Append(',');
                    PhraseAlternatives[i].WriteJson(sb);
                }
                sb.Append(']');
            }
            if (Order == ResultsOrder.Relevance) sb.Append(",\"order\":\"relevance\"");
            else if (Order == ResultsOrder.Generation) sb.Append(",\"order\":\"generation\"");
            if (BookGenerations.Count > 0)
            {
                sb.Append(",\"book_generations\":[");
                bool first = true;
                foreach (var kv in BookGenerations)
                {
                    if (kv.Key < 0) continue; // book ids are unsigned in the engine
                    if (!first) sb.Append(',');
                    first = false;
                    sb.Append('[').Append(kv.Key.ToString(inv)).Append(',')
                        .Append(Math.Max(0, kv.Value).ToString(inv)).Append(']');
                }
                sb.Append(']');
            }
            sb.Append('}');
            return sb.ToString();
        }

        /// <summary>Writes ",\"words\":{\"i\":{...}, ...}" when there are any.</summary>
        internal static void WriteWords(StringBuilder sb, Dictionary<int, WordOptions> words)
        {
            if (words.Count == 0) return;
            var inv = System.Globalization.CultureInfo.InvariantCulture;
            sb.Append(",\"words\":{");
            bool first = true;
            foreach (var kv in words)
            {
                if (!first) sb.Append(',');
                first = false;
                sb.Append('"').Append(kv.Key.ToString(inv)).Append("\":");
                kv.Value.WriteJson(sb);
            }
            sb.Append('}');
        }

        /// <summary>Writes the word-option fields (",name":value), skipping nulls.</summary>
        internal static void WordFields(StringBuilder sb, bool? grammarPrefixes, bool? grammarSuffixes,
            bool? aramaicPrefixes, bool? aramaicSuffixes, bool? ktivMaleHaser, bool? partOfWord,
            bool? advancedPrefixes, bool? advancedSuffixes, bool? ignoreQuotes, bool? exactQuotes,
            bool? spellingErrors, bool? matchNikud, bool? matchTeamim, TypoMode? typoMode,
            int? prefixMaxLetters, int? suffixMaxLetters, int? partOfWordMaxLetters, int? grammarPrefixMaxLetters,
            KtivRules? ktivRules = null)
        {
            var inv = System.Globalization.CultureInfo.InvariantCulture;
            void Flag(string name, bool? v)
            {
                if (v.HasValue) sb.Append(",\"").Append(name).Append("\":").Append(v.Value ? "true" : "false");
            }
            void Limit(string name, int? v)
            {
                if (v.HasValue) sb.Append(",\"").Append(name).Append("\":").Append(Math.Max(0, v.Value).ToString(inv));
            }
            Flag("grammar_prefixes", grammarPrefixes);
            Flag("grammar_suffixes", grammarSuffixes);
            Flag("aramaic_prefixes", aramaicPrefixes);
            Flag("aramaic_suffixes", aramaicSuffixes);
            Flag("ktiv_male_haser", ktivMaleHaser);
            Flag("part_of_word", partOfWord);
            Flag("advanced_prefixes", advancedPrefixes);
            Flag("advanced_suffixes", advancedSuffixes);
            Flag("ignore_quotes", ignoreQuotes);
            Flag("exact_quotes", exactQuotes);
            Flag("spelling_errors", spellingErrors);
            Flag("match_nikud", matchNikud);
            Flag("match_teamim", matchTeamim);
            if (typoMode.HasValue)
                sb.Append(",\"typo_mode\":").Append(typoMode.Value == TypoMode.FullEdit1 ? "\"full_edit1\"" : "\"otzaria\"");
            Limit("prefix_max_letters", prefixMaxLetters);
            Limit("suffix_max_letters", suffixMaxLetters);
            Limit("part_of_word_max_letters", partOfWordMaxLetters);
            Limit("grammar_prefix_max_letters", grammarPrefixMaxLetters);
            if (ktivRules.HasValue)
                sb.Append(",\"ktiv_rules\":").Append(ktivRules.Value == KtivRules.FtsLib ? "\"ftslib\"" : "\"otzaria\"");
        }
    }

    /// <summary>One word's overrides: the same fields as <see cref="SearchOptions"/>'
    /// word options, null = inherit, plus extra words for the word's slot.</summary>
    public sealed class WordOptions
    {
        public bool? GrammarPrefixes { get; set; }
        public bool? GrammarSuffixes { get; set; }
        public bool? AramaicPrefixes { get; set; }
        public bool? AramaicSuffixes { get; set; }
        public bool? KtivMaleHaser { get; set; }
        public KtivRules? KtivRules { get; set; }
        public bool? PartOfWord { get; set; }
        public bool? AdvancedPrefixes { get; set; }
        public bool? AdvancedSuffixes { get; set; }
        public bool? IgnoreQuotes { get; set; }
        public bool? ExactQuotes { get; set; }
        public bool? SpellingErrors { get; set; }
        public bool? MatchNikud { get; set; }
        public bool? MatchTeamim { get; set; }
        public TypoMode? TypoMode { get; set; }
        public int? PrefixMaxLetters { get; set; }
        public int? SuffixMaxLetters { get; set; }
        public int? PartOfWordMaxLetters { get; set; }
        public int? GrammarPrefixMaxLetters { get; set; }
        /// <summary>Extra words for this word's slot (Otzaria's alternativeWords), in
        /// query syntax, searched with this word's options.</summary>
        public List<string> Alternatives { get; } = new List<string>();

        internal void WriteJson(StringBuilder sb)
        {
            sb.Append("{\"alternatives\":[");
            for (int i = 0; i < Alternatives.Count; i++)
            {
                if (i > 0) sb.Append(',');
                Json.WriteString(sb, Alternatives[i]);
            }
            sb.Append(']');
            SearchOptions.WordFields(sb, GrammarPrefixes, GrammarSuffixes, AramaicPrefixes, AramaicSuffixes,
                KtivMaleHaser, PartOfWord, AdvancedPrefixes, AdvancedSuffixes, IgnoreQuotes, ExactQuotes,
                SpellingErrors, MatchNikud, MatchTeamim, TypoMode, PrefixMaxLetters, SuffixMaxLetters,
                PartOfWordMaxLetters, GrammarPrefixMaxLetters, KtivRules);
            sb.Append('}');
        }
    }

    /// <summary>The negative query of <see cref="SearchOptions"/> (Otzaria's negativeQuery):
    /// its words, all required, together. Options are per word: a negative word takes the
    /// global word options of <see cref="SearchOptions"/> as its default and has its own
    /// overrides and alternatives in <see cref="Words"/> (Otzaria's negativeSearchOptions /
    /// negativeAlternativeWords). There is no separate global set for the negative query: a
    /// host with one writes it onto every negative word.</summary>
    public sealed class ExcludeOptions
    {
        public ExcludeOptions() { }
        public ExcludeOptions(string query) { Query = query; }

        public string Query { get; set; } = "";
        /// <summary>Per-word overrides of <see cref="Query"/>'s words, by typed-word index,
        /// with the same rule and fields as <see cref="SearchOptions.Words"/>.</summary>
        public Dictionary<int, WordOptions> Words { get; } = new Dictionary<int, WordOptions>();
        /// <summary>Whole phrases ORed with the negative query (Otzaria's acronym expansion
        /// of the negative words): a line carrying one, as a phrase, is dropped too.</summary>
        public List<PhraseOptions> PhraseAlternatives { get; } = new List<PhraseOptions>();
        public int? MaxWordDistance { get; set; }
        public bool RequireOrdered { get; set; }
        public Proximity? Proximity { get; set; }
        /// <summary>Otzaria's SameSection negative scope: a section holding every negative
        /// word is dropped whole.</summary>
        public bool SameSection { get; set; }

        internal void WriteJson(StringBuilder sb)
        {
            sb.Append("{\"query\":");
            if (SameSection) { Json.WriteString(sb, Query ?? ""); sb.Append(",\"same_section\":true"); }
            else Json.WriteString(sb, Query ?? "");
            SearchOptions.WriteWords(sb, Words);
            if (PhraseAlternatives.Count > 0)
            {
                sb.Append(",\"phrase_alternatives\":[");
                for (int i = 0; i < PhraseAlternatives.Count; i++)
                {
                    if (i > 0) sb.Append(',');
                    PhraseAlternatives[i].WriteJson(sb);
                }
                sb.Append(']');
            }
            if (MaxWordDistance.HasValue)
                sb.Append(",\"max_word_distance\":").Append(MaxWordDistance.Value.ToString(System.Globalization.CultureInfo.InvariantCulture));
            if (RequireOrdered) sb.Append(",\"require_ordered\":true");
            if (Proximity != null) { sb.Append(",\"proximity\":"); Proximity.WriteJson(sb); }
            sb.Append('}');
        }
    }

    /// <summary>A whole-phrase alternative: words adjacent, in order, all required,
    /// matched as written (plus the quote option).</summary>
    public sealed class PhraseOptions
    {
        public string Query { get; set; } = "";
        public bool IgnoreQuotes { get; set; }

        internal void WriteJson(StringBuilder sb)
        {
            sb.Append("{\"query\":");
            Json.WriteString(sb, Query ?? "");
            sb.Append(",\"ignore_quotes\":").Append(IgnoreQuotes ? "true" : "false").Append('}');
        }
    }

    /// <summary>
    /// Exact totals of a search (Otzaria's count / countByBook): the lines that
    /// pass every filter, in all and per book id.
    /// </summary>
    public sealed class HitCounts
    {
        public HitCounts(long total, IReadOnlyDictionary<long, long> byBook, long validated, bool exact)
        {
            Total = total; ByBook = byBook; Validated = validated; Exact = exact;
        }
        /// <summary>Lines that passed, among the hits counted so far.</summary>
        public long Total { get; }
        /// <summary>Lines per book id (books with none absent).</summary>
        public IReadOnlyDictionary<long, long> ByBook { get; }
        /// <summary>Hits counted so far, from the first in hit order.</summary>
        public long Validated { get; }
        /// <summary>True when every hit is counted: the numbers are final.</summary>
        public bool Exact { get; }
    }

    /// <summary>One hit for <see cref="SearchSession.LocateLines"/>: its index in the search's hits
    /// (-1 when unknown) and its lines, document order: the matched line, and for a short
    /// window its neighbour lines (empty texts are skipped).</summary>
    public readonly struct HitLines
    {
        public HitLines(long hitIndex, IReadOnlyList<(long LineId, string Text)> lines)
        {
            HitIndex = hitIndex;
            Lines = lines;
        }

        public long HitIndex { get; }
        public IReadOnlyList<(long LineId, string Text)> Lines { get; }
    }

    /// <summary>A matching line (in the search's order, catalog by default): only the ids the index keeps. Any
    /// other placement (line index, section) comes from SQLite by line id.</summary>
    public readonly struct PlacedHit
    {
        public PlacedHit(long position, long lineId, long bookId) : this(position, lineId, bookId, null) { }
        public PlacedHit(long position, long lineId, long bookId, long? nextLineId)
        {
            Position = position; LineId = lineId; BookId = bookId; NextLineId = nextLineId;
        }
        public long Position { get; }
        public long LineId { get; }
        public long BookId { get; }
        /// <summary>Cross-line matching: the next line's id when the match may continue onto
        /// it. Locate the hit with both texts (<see cref="SearchSession.LocatePairs"/>).</summary>
        public long? NextLineId { get; }
    }

    /// <summary>
    /// A located snippet (FtsLib's SnippetDescriptor fields as copied onto FtsHit).
    /// Offsets are UTF-16 units into the text that was located — .NET string
    /// indices.
    /// </summary>
    public sealed class Snippet
    {
        public const int FlagLeadingEllipsis = 1;
        public const int FlagTrailingEllipsis = 2;

        /// <summary>Raw span of the tightest window (smaller = closer).</summary>
        public int Score { get; internal set; }
        /// <summary>Words between the matched slots in the tightest window.</summary>
        public int WordDistance { get; internal set; }
        /// <summary>Words the window spans after context expansion.</summary>
        public int WindowWordCount { get; internal set; }
        public int Start { get; internal set; }
        public int End { get; internal set; }
        /// <summary>(start, length, termId) triples; termId indexes the session's term table.</summary>
        public int[] Marks { get; internal set; } = Array.Empty<int>();
        /// <summary><see cref="FlagLeadingEllipsis"/> | <see cref="FlagTrailingEllipsis"/>.</summary>
        public int Flags { get; internal set; }

        /// <summary>A cross-line snippet: offsets are into <c>line + "\n" + nextLine</c> and this
        /// is the UTF-16 offset of that "\n" (render it as a line break). Null: in-line.</summary>
        public int? BreakAt { get; internal set; }

        public bool LeadingEllipsis => (Flags & FlagLeadingEllipsis) != 0;
        public bool TrailingEllipsis => (Flags & FlagTrailingEllipsis) != 0;

        /// <summary>Fewer words than the per-side context ("short"): the host may
        /// locate again over a longer text, e.g. the line joined with its neighbours.</summary>
        public bool IsShort(int contextWords) => WindowWordCount < contextWords;

        /// <summary>Reads one record of the native flat format: header + marks.</summary>
        internal static Snippet Read(uint[] data, ref int i)
        {
            var s = new Snippet
            {
                Score = Clamp(data[i]),
                WordDistance = Clamp(data[i + 1]),
                WindowWordCount = (int)data[i + 2],
                Start = (int)data[i + 3],
                End = (int)data[i + 4],
                Flags = (int)data[i + 5],
            };
            int markCount = (int)data[i + 6];
            i += 7;
            var marks = new int[markCount * 3];
            for (int m = 0; m < marks.Length; m++) marks[m] = (int)data[i + m];
            i += marks.Length;
            s.Marks = marks;
            return s;
        }

        private static int Clamp(uint v) => v > int.MaxValue ? int.MaxValue : (int)v;
    }
}
