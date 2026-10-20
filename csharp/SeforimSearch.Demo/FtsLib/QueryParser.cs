using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using SeforimSearch.Engine;

namespace FtsLib.Search
{
    /// <summary>
    /// Port of FtsLib's QueryParser, emitting engine <see cref="QueryGroup"/>s.
    ///
    /// Rules (FtsLib's, unchanged):
    ///   - Tokens split on whitespace, after every index word-separator becomes a space.
    ///   - A bare '|' token joins its neighbours into one OR group: "a | b c" = {a,b} AND c.
    ///   - '*' or '?' → wildcard (overrides '~' and '%').
    ///   - '~' / '~N' suffix → fuzzy, N clamped to the engine's 2 (FtsLib allowed 3).
    ///   - '%' at the start / end → grammatical prefixes / suffixes.
    ///   - A plain literal of fewer than 2 or 30+ letters is dropped (the index has none).
    ///   - Nikud/te'amim stripped, ASCII lowercased, intra-word quotes kept as the
    ///     index keeps them (canonical '"'), other chars dropped.
    ///
    /// <see cref="Parse"/> takes the walk options the user picked (Otzaria's search
    /// options) and applies them to every literal word.
    /// </summary>
    public static class QueryParser
    {
        public const int MaxFuzzyDistance = 2;

        /// <summary>Parses <paramref name="query"/> into AND groups. Returns the groups
        /// and the original group count (for word distance).</summary>
        public static List<QueryGroup> Parse(string query, WalkOptions? wordOptions = null)
        {
            var groups = new List<QueryGroup>();
            if (string.IsNullOrWhiteSpace(query)) return groups;

            query = SplitOnIndexSeparators(query).Replace("|", " | ");

            var pending = new List<QueryGroup>();
            bool lastWasPipe = false;
            foreach (var raw in query.Split(new[] { ' ', '\t', '\r', '\n' }, StringSplitOptions.RemoveEmptyEntries))
            {
                if (IsPipeToken(raw)) { lastWasPipe = true; continue; }
                var g = ParseToken(raw, wordOptions);
                if (g == null) continue;
                if (!lastWasPipe && pending.Count > 0)
                {
                    groups.Add(pending.Count == 1 ? pending[0] : QueryGroup.Any(pending.ToArray()));
                    pending = new List<QueryGroup>();
                }
                pending.Add(g);
                lastWasPipe = false;
            }
            if (pending.Count > 0)
                groups.Add(pending.Count == 1 ? pending[0] : QueryGroup.Any(pending.ToArray()));
            return groups;
        }

        private static bool IsLetter(char c) => (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= 'א' && c <= 'ת');
        private static bool IsStrippableMark(char c) => c >= '֑' && c <= 'ׇ' && c != '׀' && c != '׃' && c != '׆';
        private static bool IsIntraWordQuote(char c) => c == '"' || c == '״' || c == '\'' || c == '׳';

        /// <summary>Every char the indexer treats as a word separator becomes a space
        /// (FtsLib SplitOnIndexSeparators, verbatim rules).</summary>
        private static string SplitOnIndexSeparators(string query)
        {
            StringBuilder? sb = null;
            char prevLetter = '\0';
            for (int i = 0; i < query.Length; i++)
            {
                char c = query[i];
                bool keep = c != '־' && (
                    char.IsWhiteSpace(c) || IsLetter(c) || IsStrippableMark(c) || IsIntraWordQuote(c)
                    || c == '*' || c == '?' || c == '~' || c == '%' || c == '|'
                    || (c >= '0' && c <= '9' && i > 0 && query[i - 1] == '~')
                    || (c > 127 && CharUnicodeInfo.GetUnicodeCategory(c) == UnicodeCategory.NonSpacingMark));
                if (keep)
                {
                    if (IsLetter(c))
                    {
                        if (prevLetter != '\0' && (c >= 'א') != (prevLetter >= 'א'))
                        {
                            sb ??= new StringBuilder(query, 0, i, query.Length + 8);
                            sb.Append(' ');
                        }
                        prevLetter = c;
                    }
                    else if (!(IsStrippableMark(c) || IsIntraWordQuote(c)
                               || (c > 127 && CharUnicodeInfo.GetUnicodeCategory(c) == UnicodeCategory.NonSpacingMark)))
                    {
                        prevLetter = '\0';
                    }
                    sb?.Append(c);
                }
                else
                {
                    sb ??= new StringBuilder(query, 0, i, query.Length + 8);
                    sb.Append(' ');
                    prevLetter = '\0';
                }
            }
            return sb == null ? query : sb.ToString();
        }

        private static bool IsPipeToken(string raw)
        {
            foreach (char c in raw) if (c != '|') return false;
            return true;
        }

        private static QueryGroup? ParseToken(string raw, WalkOptions? wordOptions)
        {
            bool grammarPrefix = raw.StartsWith("%", StringComparison.Ordinal);
            bool grammarSuffix = raw.EndsWith("%", StringComparison.Ordinal);
            if (grammarPrefix || grammarSuffix) raw = raw.Trim('%');

            string tokenText = raw;
            bool isFuzzy = false;
            int fuzzyDist = 1;
            int tilde = raw.LastIndexOf('~');
            if (tilde >= 0)
            {
                string suffix = raw.Substring(tilde + 1);
                if (suffix.Length == 0 || (suffix.Length == 1 && suffix[0] >= '1' && suffix[0] <= '9'))
                {
                    isFuzzy = true;
                    fuzzyDist = Math.Min(suffix.Length == 0 ? 1 : suffix[0] - '0', MaxFuzzyDistance);
                    tokenText = raw.Substring(0, tilde);
                }
            }

            string normalised = Normalise(tokenText);
            int letters = 0;
            foreach (char c in normalised) if (IsLetter(c)) letters++;
            if (letters == 0 && normalised.IndexOf('*') < 0 && normalised.IndexOf('?') < 0) return null;

            bool isWildcard = normalised.IndexOf('*') >= 0 || normalised.IndexOf('?') >= 0;
            if (isFuzzy && isWildcard) isFuzzy = false;
            if (isWildcard) grammarPrefix = grammarSuffix = false;

            if (isWildcard) return QueryGroup.Wildcard(normalised);
            if (isFuzzy) return QueryGroup.Fuzzy(normalised, fuzzyDist);

            if (!grammarPrefix && !grammarSuffix && (letters < 2 || letters >= 30)) return null;

            var options = Clone(wordOptions);
            if (grammarPrefix) options.GrammarPrefixes = true;
            if (grammarSuffix) options.GrammarSuffixes = true;
            return QueryGroup.Word(normalised, options);
        }

        /// <summary>Strips marks, lowercases ASCII, keeps '*' '?' and intra-word quotes
        /// (as the canonical '"' the index stores); drops everything else.</summary>
        private static string Normalise(string token)
        {
            var sb = new StringBuilder(token.Length);
            foreach (char c in token)
            {
                if (IsStrippableMark(c)) continue;
                if (c == '*' || c == '?') { sb.Append(c); continue; }
                if (IsLetter(c)) { sb.Append(c >= 'A' && c <= 'Z' ? (char)(c | 32) : c); continue; }
                if (IsIntraWordQuote(c) && sb.Length > 0) { sb.Append('"'); continue; }
            }
            // a trailing quote is quotation punctuation, not part of the word
            while (sb.Length > 0 && sb[sb.Length - 1] == '"') sb.Length--;
            return sb.ToString();
        }

        private static WalkOptions Clone(WalkOptions? o)
        {
            if (o == null) return new WalkOptions();
            return new WalkOptions
            {
                GrammarPrefixes = o.GrammarPrefixes,
                GrammarSuffixes = o.GrammarSuffixes,
                AramaicPrefixes = o.AramaicPrefixes,
                AramaicSuffixes = o.AramaicSuffixes,
                KtivMaleHaser = o.KtivMaleHaser,
                PartOfWord = o.PartOfWord,
                AdvancedPrefixes = o.AdvancedPrefixes,
                AdvancedSuffixes = o.AdvancedSuffixes,
                IgnoreQuotes = o.IgnoreQuotes,
                SpellingErrors = o.SpellingErrors,
                IgnoreNikud = o.IgnoreNikud,
                IgnoreTeamim = o.IgnoreTeamim,
                PrefixMaxLetters = o.PrefixMaxLetters,
                SuffixMaxLetters = o.SuffixMaxLetters,
                PartOfWordMaxLetters = o.PartOfWordMaxLetters,
                GrammarPrefixMaxLetters = o.GrammarPrefixMaxLetters,
            };
        }
    }
}
