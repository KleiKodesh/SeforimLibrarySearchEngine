using System;
using System.Collections.Generic;
using System.Threading;
using SeforimSearch.Engine;

namespace FtsLib.Fts5
{
    public sealed partial class TantivySeforimIndex
    {
        /// <summary>From this many ranges on, FtsLib runs one query over their span and assigns
        /// each row to its range (kept here: the row order and range assignment follow it).</summary>
        private const int ManyRanges = 16;

        /// <summary>
        /// FtsLib's <c>Fts5SeforimIndex.FindSubstringCandidates</c> (issue G-K1): the lines inside
        /// blocks of index positions (FtsLib's 1-based rowids) that may hold
        /// <paramref name="query"/> as a substring, as (range, position, line id). Every query
        /// word is cut into the indexer's terms and each term is the group of EVERY dictionary
        /// term containing it (the engine's substring query: FM sidecar, no affix budget); lines
        /// holding all groups, restricted to the ranges, come back in FtsLib's order: fewer than
        /// 16 ranges = range by range, each ascending; more = one ascending pass, each row given
        /// to the first range (by start) not ending before it. A superset: the caller confirms
        /// the substring on the text.
        ///
        /// Null when the index cannot answer exactly (FtsLib's null): a word with no indexable
        /// term, no FM sidecar, or a term contained in more than
        /// <paramref name="maxTermsPerFragment"/> dictionary terms (the engine's
        /// <c>decline_over_cap</c>).
        /// </summary>
        public List<(int Range, long Position, long LineId)>? FindSubstringCandidates(
            string query, IReadOnlyList<(long First, long Last)> ranges, int maxTermsPerFragment, CancellationToken ct)
        {
            if (!SeforimIndex.Exists(_indexPath)) return null;
            // FtsLib's positions start at 1, the engine's at 0 (same order).
            var engineRanges = new List<(long First, long Last)>(ranges.Count);
            foreach (var (first, last) in ranges)
                if (last >= 1 && last >= first) engineRanges.Add((Math.Max(first, 1) - 1, last - 1));
            var options = new SearchOptions(query)
            {
                Substring = true,
                DeclineOverCap = true,
                MaxExpandedTerms = maxTermsPerFragment,
                PositionRanges = engineRanges,
            };
            using var session = Engine().Search(options, ct);
            if (session.GetInfo().Declined) return null;
            var positions = new List<(long Position, long LineId)>((int)Math.Min(session.HitCount, int.MaxValue));
            for (long offset = 0; offset < session.HitCount; offset += 65536)
            {
                ct.ThrowIfCancellationRequested();
                foreach (var hit in session.GetHits(offset, 65536)) positions.Add((hit.Position + 1, hit.LineId));
            }
            // catalog order, as FtsLib's rowid order (the engine returns it so; kept explicit)
            positions.Sort((a, b) => a.Position.CompareTo(b.Position));

            var found = new List<(int, long, long)>();
            if (ranges.Count < ManyRanges)
            {
                for (int range = 0; range < ranges.Count; range++)
                {
                    int at = LowerBound(positions, ranges[range].First);
                    for (int i = at; i < positions.Count && positions[i].Position <= ranges[range].Last; i++)
                        found.Add((range, positions[i].Position, positions[i].LineId));
                }
                return found;
            }
            var order = new int[ranges.Count];
            for (int i = 0; i < order.Length; i++) order[i] = i;
            Array.Sort(order, (a, b) => ranges[a].First.CompareTo(ranges[b].First));
            int cursor = 0;   // rows and ranges both ascend: the cursor only moves forward
            foreach (var (position, lineId) in positions)
            {
                while (cursor < order.Length && ranges[order[cursor]].Last < position) cursor++;
                if (cursor == order.Length) break;
                if (ranges[order[cursor]].First <= position) found.Add((order[cursor], position, lineId));
            }
            return found;
        }

        private static int LowerBound(List<(long Position, long LineId)> sorted, long value)
        {
            int lo = 0, hi = sorted.Count;
            while (lo < hi)
            {
                int mid = (lo + hi) >> 1;
                if (sorted[mid].Position < value) lo = mid + 1; else hi = mid;
            }
            return lo;
        }
    }
}
