using System;
using System.Runtime.InteropServices;

namespace SeforimSearch.Engine.Native
{
    /// <summary>A u32 buffer owned by the native library (see se_buffer_free).</summary>
    [StructLayout(LayoutKind.Sequential)]
    internal struct SeBuffer
    {
        public IntPtr Data;
        public UIntPtr Len;
        public UIntPtr Cap;
    }

    /// <summary>
    /// The C ABI of seforim_search_native (engine-ffi/src/lib.rs). Strings
    /// cross as UTF-8 pointer + length; status calls return 0 / -1 with the
    /// message in se_last_error.
    /// </summary>
    internal static unsafe class NativeMethods
    {
        private const string Lib = "seforim_search_native";

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_last_error();

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_string_free(IntPtr s);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_version();

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_reduce_paired_readings(byte* query, UIntPtr queryLen, out UIntPtr outLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_bytes_free(IntPtr data, UIntPtr len);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_user_regex_kind(byte* query, UIntPtr queryLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_buffer_free(SeBuffer buffer);

        // ── Index ──
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IndexHandle se_index_create(byte* dir, UIntPtr dirLen, byte* options, UIntPtr optionsLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IndexHandle se_index_open(byte* dir, UIntPtr dirLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_index_exists(byte* dir, UIntPtr dirLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_index_close(IntPtr index);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_index_info(IndexHandle index);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_index_build_fm_sidecar(IndexHandle index);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_index_has_fm_sidecar(IndexHandle index);

        // ── Writer ──
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern WriterHandle se_writer_open(IndexHandle index, UIntPtr heapBytes, UIntPtr numThreads);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_writer_add(WriterHandle writer, ulong lineId, ulong position, ulong bookId,
                                               byte* text, UIntPtr textLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_writer_add_line(WriterHandle writer, ulong lineId, ulong position, ulong bookId,
                                                    ulong sectionId, uint flags, byte* text, UIntPtr textLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_writer_delete(WriterHandle writer, ulong lineId);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_writer_commit(WriterHandle writer, byte* payload, UIntPtr payloadLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_writer_rollback(WriterHandle writer);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_writer_merge_and_close(IntPtr writer);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_writer_close(IntPtr writer);

        // ── Search ──
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern SearchHandle se_search(IndexHandle index, byte* request, UIntPtr requestLen);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern ulong se_search_hit_count(SearchHandle search);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern ulong se_search_copy_hits(SearchHandle search, ulong offset, ulong count, ulong* output);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern SeBuffer se_search_locate_batch(SearchHandle search, IntPtr* texts, UIntPtr* lens, UIntPtr count);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern SeBuffer se_search_locate_lines(SearchHandle search, long* hitIndexes, UIntPtr* lineCounts,
                                                             UIntPtr hitCount, ulong* lineIds, IntPtr* texts, UIntPtr* lens);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern ulong se_search_copy_next_lines(SearchHandle search, ulong offset, ulong count, ulong* output);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern SeBuffer se_search_locate_pairs(SearchHandle search, IntPtr* texts, UIntPtr* lens,
                                                             IntPtr* nextTexts, UIntPtr* nextLens, UIntPtr count);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern long se_search_count_batch_pairs(SearchHandle search, IntPtr* texts, UIntPtr* lens,
                                                              IntPtr* nextTexts, UIntPtr* nextLens, UIntPtr count);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_search_take_new_terms(SearchHandle search);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern SearchHandle se_search_cancellable(IndexHandle index, byte* request, UIntPtr requestLen, CancelTokenHandle token);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_last_error_cancelled();

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern CancelTokenHandle se_cancel_token_new();

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_cancel_token_cancel(IntPtr token);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_cancel_token_free(IntPtr token);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern int se_search_count_needs_text(SearchHandle search);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern long se_search_count_batch(SearchHandle search, IntPtr* texts, UIntPtr* lens, UIntPtr count);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_search_counts(SearchHandle search);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_search_info(SearchHandle search);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr se_search_line_status(SearchHandle search);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern SeBuffer se_search_line_match_batch(SearchHandle search, IntPtr* texts, UIntPtr* lens, UIntPtr count);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern SeBuffer se_search_line_scan_batch(SearchHandle search, ulong* lines, IntPtr* texts, UIntPtr* lens, UIntPtr count);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern ulong se_search_copy_line_hits(SearchHandle search, ulong offset, ulong count, ulong* output);

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        public static extern void se_search_close(IntPtr search);
    }
}
