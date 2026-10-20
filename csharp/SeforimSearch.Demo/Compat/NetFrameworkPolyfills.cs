// Language-feature and BCL shims so the code ported from KitveiHakodesh (written
// for .NET 10) compiles unchanged on .NET Framework 4.8. Each is compiled only
// for .NET Framework; on .NET the real BCL types are used.
#if NETFRAMEWORK
using System;
using System.Collections.Generic;

namespace System.Runtime.CompilerServices
{
    /// <summary>Enables records and init-only setters.</summary>
    internal static class IsExternalInit { }
}

namespace System
{
    /// <summary>Index type for the ^n syntax.</summary>
    internal readonly struct Index
    {
        private readonly int _value;
        public Index(int value, bool fromEnd = false) { _value = fromEnd ? ~value : value; }
        public static implicit operator Index(int value) => new Index(value);
        public bool IsFromEnd => _value < 0;
        public int Value => _value < 0 ? ~_value : _value;
        public int GetOffset(int length) => IsFromEnd ? length - Value : Value;
        public static Index Start => new Index(0);
        public static Index End => new Index(0, true);
    }

    /// <summary>Range type for the a..b syntax.</summary>
    internal readonly struct Range
    {
        public Index Start { get; }
        public Index End { get; }
        public Range(Index start, Index end) { Start = start; End = end; }
        public static Range All => new Range(Index.Start, Index.End);
        public static Range StartAt(Index start) => new Range(start, Index.End);
        public static Range EndAt(Index end) => new Range(Index.Start, end);
        public (int Offset, int Length) GetOffsetAndLength(int length)
        {
            int start = Start.GetOffset(length);
            int end = End.GetOffset(length);
            return (start, end - start);
        }
    }
}

namespace SeforimSearch.Demo.Compat
{
    /// <summary>String overloads .NET Framework lacks.</summary>
    internal static class StringCompat
    {
        public static bool EndsWith(this string s, char c) => s.Length > 0 && s[s.Length - 1] == c;
        public static bool StartsWith(this string s, char c) => s.Length > 0 && s[0] == c;
        public static bool Contains(this string s, char c) => s.IndexOf(c) >= 0;
        public static string[] Split(this string s, char separator, StringSplitOptions options) =>
            s.Split(new[] { separator }, options);
    }

    /// <summary>Dictionary / collection helpers .NET Framework lacks.</summary>
    internal static class CollectionCompat
    {
        public static bool TryAdd<TKey, TValue>(this Dictionary<TKey, TValue> d, TKey key, TValue value)
        {
            if (d.ContainsKey(key)) return false;
            d.Add(key, value);
            return true;
        }

        public static TValue GetValueOrDefault<TKey, TValue>(this IReadOnlyDictionary<TKey, TValue> d, TKey key)
            => d.TryGetValue(key, out var v) ? v : default!;
    }
}
#endif
