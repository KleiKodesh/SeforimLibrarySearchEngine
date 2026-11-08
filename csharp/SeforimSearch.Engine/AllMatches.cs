using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>What a session's locate calls return for a passing hit (issues #14 / #23;
    /// FtsLib <c>SnippetMatchMode</c>).</summary>
    public enum SnippetMatchMode
    {
        /// <summary>The best window and its occurrences' marks (the default).</summary>
        BestWindow = 0,
        /// <summary>The whole text (Start 0, End its length) and the marks of EVERY full
        /// occurrence in it (the best window's occurrence rule). Which hits pass does not
        /// change.</summary>
        AllMatches = 1,
    }

    /// <summary>Options of <see cref="TextMatcher.FindAllMatches"/>.</summary>
    [Flags]
    public enum FindMatchOptions
    {
        None = 0,
        /// <summary>Only runs of the words in order at consecutive positions (a phrase);
        /// else every word anywhere.</summary>
        Phrase = 1,
        /// <summary>Quotes (geresh / gershayim) ignored.</summary>
        IgnoreQuotes = 2,
        /// <summary>Nikud / te'amim must match as typed (by default both sides drop them).</summary>
        MatchMarks = 4,
    }

    /// <summary>One occurrence: UTF-16 start and length in the text, and the term ordinal
    /// (first-seen distinct query word).</summary>
    public readonly struct TextMatch
    {
        public readonly int Start, Length, Term;
        public TextMatch(int start, int length, int term) { Start = start; Length = length; Term = term; }
        public override string ToString() => $"[{Start}+{Length}] #{Term}";
    }

    /// <summary>Find in an open text, no index or session (issues #14 / #23): the engine's
    /// own locator with its word rules (HTML, entities, marks, quotes).</summary>
    public static unsafe class TextMatcher
    {
        /// <summary>Every occurrence of <paramref name="words"/> (each normalized by the line
        /// scanner, its words in order) in <paramref name="text"/>, document order.</summary>
        public static TextMatch[] FindAllMatches(string text, IReadOnlyList<string> words,
                                                 FindMatchOptions options = FindMatchOptions.None)
        {
            if (words == null) throw new ArgumentNullException(nameof(words));
            if (string.IsNullOrEmpty(text) || words.Count == 0) return Array.Empty<TextMatch>();
            int n = words.Count;
            var pins = new GCHandle[n];
            var ptrs = new IntPtr[n];
            var lens = new UIntPtr[n];
            uint[] data;
            try
            {
                for (int i = 0; i < n; i++)
                {
                    byte[] bytes = Interop.ToUtf8(words[i] ?? "");
                    pins[i] = GCHandle.Alloc(bytes, GCHandleType.Pinned);
                    ptrs[i] = pins[i].AddrOfPinnedObject();
                    lens[i] = (UIntPtr)bytes.Length;
                }
                byte[] t = Interop.ToUtf8(text);
                fixed (byte* tp = t)
                fixed (IntPtr* p = ptrs)
                fixed (UIntPtr* l = lens)
                    data = Interop.TakeBuffer(NativeMethods.se_find_all_matches(tp, (UIntPtr)t.Length, p, l, (UIntPtr)n, (uint)options));
            }
            finally
            {
                foreach (var pin in pins)
                    if (pin.IsAllocated) pin.Free();
            }
            var result = new TextMatch[data.Length / 3];
            for (int i = 0; i < result.Length; i++)
                result[i] = new TextMatch((int)data[3 * i], (int)data[3 * i + 1], (int)data[3 * i + 2]);
            return result;
        }
    }
}
