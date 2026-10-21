using System;
using System.Threading;
using System.Collections.Generic;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>
    /// A line index on disk. Thread safety: searches may run concurrently from
    /// several threads (each <see cref="Search(SearchOptions)"/> returns its own session);
    /// one writer at a time.
    /// </summary>
    public sealed unsafe class SeforimIndex : IDisposable
    {
        private readonly IndexHandle _handle;

        private SeforimIndex(IndexHandle handle) { _handle = handle; }

        internal IndexHandle Handle => _handle;

        /// <summary>The native library's version.</summary>
        public static string NativeVersion => Interop.FromUtf8Z(NativeMethods.se_version());

        /// <summary>
        /// Otzaria's query sanitizer step: every paired reading <c>(X) [Y]</c> becomes
        /// <c> Y </c>, as the engine's query parser does. A host that numbers the query's
        /// words itself (per-word options, per-pair spacing) runs it first.
        /// </summary>
        public static string ReducePairedReadings(string query)
        {
            byte[] q = Interop.ToUtf8(query);
            fixed (byte* p = q)
                return Interop.TakeString(NativeMethods.se_reduce_paired_readings(p, (UIntPtr)q.Length));
        }

        /// <summary>Otzaria's user-regex kind of <paramref name="query"/>: "none" (not
        /// /.../), "invalid" (searched as plain text), "word", "sequence", "line" (matched on
        /// each line's text: <see cref="SearchSession.GetLineRegexStatus"/>). Set <see cref="SearchOptions.UserRegex"/> to search /.../ as a regex.</summary>
        public static string UserRegexKind(string query)
        {
            byte[] q = Interop.ToUtf8(query);
            fixed (byte* p = q)
                return (string)Json.Parse(Interop.TakeString(NativeMethods.se_user_regex_kind(p, (UIntPtr)q.Length)))!;
        }

        /// <summary>Creates a fresh index in <paramref name="directory"/>, WIPING it.</summary>
        public static SeforimIndex Create(string directory, IndexOptions? options = null)
        {
            byte[] dir = Interop.ToUtf8(directory);
            byte[] opts = Interop.ToUtf8((options ?? new IndexOptions()).ToJson());
            fixed (byte* d = dir)
            fixed (byte* o = opts)
                return new SeforimIndex(Interop.Check(
                    NativeMethods.se_index_create(d, (UIntPtr)dir.Length, o, (UIntPtr)opts.Length)));
        }

        /// <summary>Opens an existing index.</summary>
        public static SeforimIndex Open(string directory)
        {
            byte[] dir = Interop.ToUtf8(directory);
            fixed (byte* d = dir)
                return new SeforimIndex(Interop.Check(NativeMethods.se_index_open(d, (UIntPtr)dir.Length)));
        }

        /// <summary>True when <paramref name="directory"/> holds an index.</summary>
        public static bool Exists(string directory)
        {
            byte[] dir = Interop.ToUtf8(directory);
            fixed (byte* d = dir)
                return NativeMethods.se_index_exists(d, (UIntPtr)dir.Length) == 1;
        }

        /// <summary>Line count, build options and the last committed payload.</summary>
        public IndexInfo GetInfo()
        {
            var obj = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_index_info(_handle)))!;
            return new IndexInfo
            {
                NumLines = (long)(double)obj["num_lines"]!,
                Positions = (bool)obj["positions"]!,
                KeepMarks = (bool)obj["keep_marks"]!,
                Payload = obj["payload"] as string,
            };
        }

        /// <summary>Builds the FM-index sidecar over the term dictionary (FtsLib's
        /// FM sidecar): leading-<c>*</c> wildcards and fuzzy words then run through
        /// it instead of walking the whole dictionary, with identical results.
        /// Needs a single segment: call after <see cref="SeforimIndexWriter.MergeAndClose"/>.
        /// Idempotent. Returns false (nothing built; searches use the walk) when the
        /// index has several segments, no terms, or a dictionary with more than 64
        /// distinct characters (a marked index with nikud and te'amim).</summary>
        public bool BuildFmSidecar()
        {
            int built = NativeMethods.se_index_build_fm_sidecar(_handle);
            if (built < 0) throw new SeforimEngineException(Interop.LastError());
            return built == 1;
        }

        /// <summary>True when a sidecar matching the current single segment exists.</summary>
        public bool HasFmSidecar => NativeMethods.se_index_has_fm_sidecar(_handle) == 1;

        /// <summary>Starts a writer. <paramref name="heapBytes"/> is the total
        /// indexing memory (at least 15 MB per thread; threads are capped by it).</summary>
        public SeforimIndexWriter OpenWriter(long heapBytes = 50_000_000, int threads = 0)
        {
            if (threads <= 0) threads = Math.Min(Environment.ProcessorCount, 4);
            return new SeforimIndexWriter(Interop.Check(
                NativeMethods.se_writer_open(_handle, (UIntPtr)(ulong)heapBytes, (UIntPtr)(uint)threads)));
        }

        /// <summary>Runs a search: every matching line in the search's order (catalog by default), plus the
        /// snippet session for locating their snippets.</summary>
        public SearchSession Search(SearchRequest request)
        {
            byte[] json = Interop.ToUtf8(request.ToJson());
            fixed (byte* j = json)
                return new SearchSession(Interop.Check(NativeMethods.se_search(_handle, j, (UIntPtr)json.Length)),
                                         request.ContextWords);
        }

        /// <summary>Runs a search given as <see cref="SearchOptions"/> (the public API):
        /// the query string is parsed by FtsLib's rules and each word becomes one walk
        /// with its options.</summary>
        public SearchSession Search(SearchOptions options)
        {
            byte[] json = Interop.ToUtf8(options.ToJson());
            fixed (byte* j = json)
                return new SearchSession(Interop.Check(NativeMethods.se_search(_handle, j, (UIntPtr)json.Length)),
                                         options.ContextWords);
        }

        /// <summary>
        /// <see cref="Search(SearchOptions)"/> with cancellation (Otzaria's cancel
        /// token): cancelling <paramref name="ct"/> stops the search (checked before
        /// the walks, per segment and every 4096 collected docs) and every later
        /// <see cref="SearchSession.LocateBatch"/> / <see cref="SearchSession.CountBatch"/>
        /// of the returned session (checked per chunk of work). A cancelled call
        /// throws <see cref="OperationCanceledException"/>.
        /// </summary>
        public SearchSession Search(SearchOptions options, CancellationToken ct)
        {
            if (!ct.CanBeCanceled) return Search(options);
            ct.ThrowIfCancellationRequested();
            var token = NativeMethods.se_cancel_token_new();
            var registration = ct.Register(s => NativeMethods.se_cancel_token_cancel(((CancelTokenHandle)s!).DangerousGetHandle()), token);
            try
            {
                byte[] json = Interop.ToUtf8(options.ToJson());
                fixed (byte* j = json)
                    return new SearchSession(
                        Interop.Check(NativeMethods.se_search_cancellable(_handle, j, (UIntPtr)json.Length, token)),
                        options.ContextWords, token, registration);
            }
            catch
            {
                registration.Dispose();
                token.Dispose();
                throw;
            }
        }

        public void Dispose() => _handle.Dispose();
    }

    /// <summary>Adds, deletes and commits lines. Not thread-safe.</summary>
    public sealed unsafe class SeforimIndexWriter : IDisposable
    {
        private readonly WriterHandle _handle;

        internal SeforimIndexWriter(WriterHandle handle) { _handle = handle; }

        /// <summary>Queues a line. Not visible to searches until <see cref="Commit"/>.</summary>
        public void Add(in LineDocument line)
        {
            byte[] text = Interop.ToUtf8(line.Text);
            uint flags = (line.NoCross ? 1u : 0u) | (line.SectionId.HasValue ? 2u : 0u);
            fixed (byte* t = text)
                Interop.Check(NativeMethods.se_writer_add_line(_handle, (ulong)line.LineId, (ulong)line.Position,
                    (ulong)line.BookId, (ulong)(line.SectionId ?? 0), flags, t, (UIntPtr)text.Length));
        }

        /// <summary>Queues deletion of a line. An update is Delete + Add.</summary>
        public void Delete(long lineId) => Interop.Check(NativeMethods.se_writer_delete(_handle, (ulong)lineId));

        /// <summary>Commits; <paramref name="payload"/> is stored atomically with the
        /// documents (read back via <see cref="SeforimIndex.GetInfo"/>).</summary>
        public void Commit(string? payload = null)
        {
            if (payload == null)
            {
                Interop.Check(NativeMethods.se_writer_commit(_handle, null, UIntPtr.Zero));
                return;
            }
            byte[] p = Interop.ToUtf8(payload);
            fixed (byte* pp = p.Length == 0 ? new byte[1] : p)
                Interop.Check(NativeMethods.se_writer_commit(_handle, pp, (UIntPtr)p.Length));
        }

        /// <summary>Drops everything queued since the last commit.</summary>
        public void Rollback() => Interop.Check(NativeMethods.se_writer_rollback(_handle));

        /// <summary>Merges all segments into one (for a shipped, read-mostly index)
        /// and closes the writer.</summary>
        public void MergeAndClose() => Interop.Check(NativeMethods.se_writer_merge_and_close(_handle.Detach()));

        /// <summary>Closes the writer, discarding anything uncommitted.</summary>
        public void Dispose() => _handle.Dispose();
    }
}
