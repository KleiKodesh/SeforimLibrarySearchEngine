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

        /// <summary>Keep nikud/te'amim in indexed terms (for vocalized text searched
        /// with <see cref="WalkOptions.IgnoreNikud"/> / <see cref="WalkOptions.IgnoreTeamim"/>).</summary>
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

    /// <summary>One line to index.</summary>
    public struct LineDocument
    {
        public long LineId;
        /// <summary>Rank in the library's catalog order: results are returned in it.</summary>
        public long Position;
        public long BookId;
        public long LineIndex;
        public long TocEntryId;
        /// <summary>The raw line text (HTML allowed).</summary>
        public string Text;
    }

    /// <summary>Hebrew walk options for a word (Otzaria's search options).</summary>
    public sealed class WalkOptions
    {
        /// <summary>קידומות דקדוקיות (ו מ כש כ ב ש ל ה, combos up to 3 letters).</summary>
        public bool GrammarPrefixes { get; set; }
        /// <summary>סיומות דקדוקיות.</summary>
        public bool GrammarSuffixes { get; set; }
        /// <summary>קידומות ארמיות (ד/דא/א).</summary>
        public bool AramaicPrefixes { get; set; }
        /// <summary>סיומות ארמיות (ה↔א, ם↔ן).</summary>
        public bool AramaicSuffixes { get; set; }
        /// <summary>כתיב מלא/חסר.</summary>
        public bool KtivMaleHaser { get; set; }
        /// <summary>חלק ממילה.</summary>
        public bool PartOfWord { get; set; }
        /// <summary>קידומות (מתקדם): any letters before.</summary>
        public bool AdvancedPrefixes { get; set; }
        /// <summary>סיומות (מתקדם): any letters after.</summary>
        public bool AdvancedSuffixes { get; set; }
        /// <summary>התעלם מגרשיים.</summary>
        public bool IgnoreQuotes { get; set; }
        /// <summary>שגיאות כתיב / OCR מצויות.</summary>
        public bool SpellingErrors { get; set; }
        /// <summary>Ignore nikud (needs a KeepMarks index to matter).</summary>
        public bool IgnoreNikud { get; set; }
        /// <summary>Ignore te'amim (needs a KeepMarks index to matter).</summary>
        public bool IgnoreTeamim { get; set; }

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
        /// Null = default (3).</summary>
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
            Flag("spelling_errors", SpellingErrors);
            Flag("ignore_nikud", IgnoreNikud);
            Flag("ignore_teamim", IgnoreTeamim);
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
        /// capped at 2000. Distance 1..2.</summary>
        public static QueryGroup Fuzzy(string word, int distance) => new FuzzyGroup(word, distance);

        private sealed class FuzzyGroup : QueryGroup
        {
            private readonly string _word;
            private readonly int _distance;
            public FuzzyGroup(string word, int distance) { _word = word; _distance = distance; }
            internal override void WriteJson(StringBuilder sb)
            {
                sb.Append("{\"kind\":\"fuzzy\",\"word\":");
                Json.WriteString(sb, _word);
                sb.Append(",\"distance\":").Append(Math.Max(1, Math.Min(2, _distance)).ToString(System.Globalization.CultureInfo.InvariantCulture));
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

        /// <summary>Otzaria's negative query: lines (or whole sections) carrying these
        /// words are dropped. Null = none.</summary>
        public ExcludeSpec? Exclude { get; set; }

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
            if (Exclude != null)
            {
                sb.Append(",\"exclude\":");
                Exclude.WriteJson(sb);
            }
            sb.Append('}');
            return sb.ToString();
        }
    }

    /// <summary>Otzaria's negative query: the words to exclude, run as a query of their
    /// own with its own distance, order and spacing, always with every word required.
    /// A line matching it is dropped; with <see cref="SameSection"/> every line of a
    /// section (tocEntryId) holding a match is dropped. Several words bound by
    /// <see cref="MaxWordDistance"/> / <see cref="RequireOrdered"/> (without Gaps) are
    /// checked on the line text when a snippet is located, so such lines are still
    /// among the hits and locate as null. With SameSection the sections holding every
    /// excluded word (each anywhere in the section) are dropped whole; distance and
    /// order do not apply there, and lines without a section are never dropped.</summary>
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

        /// <summary>Drop every line of a section that holds a match, not just the line.</summary>
        public bool SameSection { get; set; }

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
            sb.Append(",\"same_section\":").Append(SameSection ? "true" : "false");
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
            sb.Append('}');
        }
    }

    /// <summary>A matching line, in catalog order.</summary>
    public readonly struct PlacedHit
    {
        public PlacedHit(long position, long lineId, long bookId, long lineIndex, long tocEntryId)
        {
            Position = position; LineId = lineId; BookId = bookId; LineIndex = lineIndex; TocEntryId = tocEntryId;
        }
        public long Position { get; }
        public long LineId { get; }
        public long BookId { get; }
        public long LineIndex { get; }
        public long TocEntryId { get; }
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
