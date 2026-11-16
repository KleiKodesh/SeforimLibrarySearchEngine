using System;
using System.Collections.Generic;
using FtsLib.Fts5;
using FtsLib.Snippets;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// Index with positions: turns hits' word-level descriptors (<see cref="FtsHit.Window"/>, the
    /// window the engine decided from the positions during the search) plus their line texts
    /// into the final snippet descriptors, in place (<c>se_search_finish_windows</c>: the window
    /// is not searched again; only context, marks and UTF-16 offsets are computed). One per
    /// search; it keeps that search's native session alive. Marks get ids in the search's
    /// <see cref="FtsTermTable"/> (new ones: <see cref="FtsTermTable.TakeNewTerms"/>).
    /// </summary>
    public sealed class FtsWindowFinisher
    {
        private readonly SearchSessionRef _session;
        private readonly FtsTermTable _terms;
        private readonly int _contextWords;
        private readonly object _lock = new object();

        internal FtsWindowFinisher(SearchSessionRef session, FtsTermTable terms, int contextWords)
        {
            _session = session;
            _terms = terms;
            _contextWords = contextWords;
        }

        /// <summary>The search's term table (marks' term ids index it).</summary>
        public FtsTermTable Terms => _terms;

        /// <summary>Finishes the pending hits among <paramref name="hits"/> with their
        /// <paramref name="texts"/> (aligned): offsets, marks, ellipsis and short flags, word
        /// distance. A text that disagrees with the positions (the line changed since indexing)
        /// leaves an empty window. Hits already final are left alone.</summary>
        public void Finish(IReadOnlyList<FtsHit> hits, IReadOnlyList<string> texts)
        {
            if (hits.Count != texts.Count) throw new ArgumentException("one text per hit");
            var pending = new List<int>();
            for (int i = 0; i < hits.Count; i++) if (hits[i].Window is { IsNone: false }) pending.Add(i);
            if (pending.Count == 0) return;
            var windows = new SeforimSearch.Engine.WordWindow[pending.Count];
            var lineTexts = new string[pending.Count];
            for (int k = 0; k < pending.Count; k++)
            {
                windows[k] = hits[pending[k]].Window!.Value;
                lineTexts[k] = texts[pending[k]] ?? "";
            }
            var finished = _session.FinishWindows(windows, lineTexts);
            lock (_lock)
                for (int k = 0; k < pending.Count; k++)
                {
                    var hit = hits[pending[k]];
                    var s = finished[k];
                    hit.Window = null;
                    hit.Finisher = null;
                    if (s == null) { hit.SnippetStart = hit.SnippetEnd = 0; hit.SnippetMarks = Array.Empty<int>(); continue; }
                    var window = new SnippetDescriptor(s, _session.TermAt);
                    hit.Score = window.Score;
                    hit.WordDistance = window.WordDistance;
                    SeforimDbFtsSearcher.CopySnippetDescriptor(hit, in window, _terms);
                    if (window.WindowWordCount < _contextWords) hit.SnippetFlags |= FtsHit.SnippetFlagShort;
                }
        }
    }
}
