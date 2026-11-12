using System;
using System.Threading;
using System.Collections.Generic;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>
    /// A line index on disk. Thread safety: searches may run concurrently from
    /// several threads (each <see cref="Search(SearchOptions)"/> returns its own session);
    /// one writer per index directory (a second <see cref="OpenWriter()"/>, here, on another
    /// SeforimIndex of the same directory or in another process, throws
    /// <see cref="SeforimEngineException"/> with the engine's LockBusy error). See README
    /// "Threading and writers".
    /// </summary>
    public sealed unsafe class SeforimIndex : IDisposable
    {
        private readonly IndexHandle _handle;

        private SeforimIndex(IndexHandle handle) { _handle = handle; }

        internal IndexHandle Handle => _handle;

        /// <summary>The native library's version.</summary>
        public static string NativeVersion { get { NativeAbi.EnsureLoaded(); return Interop.FromUtf8Z(NativeMethods.se_version()); } }

        /// <summary>
        /// Otzaria's query sanitizer step: every paired reading <c>(X) [Y]</c> becomes
        /// <c> Y </c>, as the engine's query parser does. A host that numbers the query's
        /// words itself (per-word options, per-pair spacing) runs it first.
        /// </summary>
        public static string ReducePairedReadings(string query)
        {
            NativeAbi.EnsureLoaded();
            byte[] q = Interop.ToUtf8(query);
            fixed (byte* p = q)
            {
                IntPtr r = NativeMethods.se_reduce_paired_readings(p, (UIntPtr)q.Length, out UIntPtr n);
                return Interop.TakeBytes(r, n);
            }
        }

        /// <summary>Otzaria's user-regex kind of <paramref name="query"/>: "none" (not
        /// /.../), "invalid" (searched as plain text), "word", "sequence", "line" (matched on
        /// each line's text: <see cref="SearchSession.GetLineRegexStatus"/>). Set <see cref="SearchOptions.UserRegex"/> to search /.../ as a regex.</summary>
        public static string UserRegexKind(string query)
        {
            NativeAbi.EnsureLoaded();
            byte[] q = Interop.ToUtf8(query);
            fixed (byte* p = q)
                return (string)Json.Parse(Interop.TakeString(NativeMethods.se_user_regex_kind(p, (UIntPtr)q.Length)))!;
        }

        /// <summary>Creates a fresh index in <paramref name="directory"/>, WIPING it.</summary>
        public static SeforimIndex Create(string directory, IndexOptions? options = null)
        {
            NativeAbi.EnsureLoaded();
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
            NativeAbi.EnsureLoaded();
            byte[] dir = Interop.ToUtf8(directory);
            fixed (byte* d = dir)
                return new SeforimIndex(Interop.Check(NativeMethods.se_index_open(d, (UIntPtr)dir.Length)));
        }

        /// <summary><see cref="Open(string)"/> on the thread pool (issue #34): opening reads the
        /// segment metadata and maps the FM sidecar, so a UI thread should not wait on it. A token
        /// cancelled before the open starts returns a cancelled task; a running open is not interrupted.</summary>
        public static System.Threading.Tasks.Task<SeforimIndex> OpenAsync(string directory, CancellationToken ct = default)
        {
            if (directory == null) throw new ArgumentNullException(nameof(directory));
            if (ct.IsCancellationRequested) return System.Threading.Tasks.Task.FromCanceled<SeforimIndex>(ct);
            return System.Threading.Tasks.Task.Run(() => Open(directory), ct);
        }

        /// <summary><see cref="Create(string, IndexOptions)"/> on the thread pool (issue #34).</summary>
        public static System.Threading.Tasks.Task<SeforimIndex> CreateAsync(string directory, IndexOptions? options = null,
                                                                            CancellationToken ct = default)
        {
            if (directory == null) throw new ArgumentNullException(nameof(directory));
            if (ct.IsCancellationRequested) return System.Threading.Tasks.Task.FromCanceled<SeforimIndex>(ct);
            return System.Threading.Tasks.Task.Run(() => Create(directory, options), ct);
        }

        /// <summary>True when <paramref name="directory"/> holds an index.</summary>
        public static bool Exists(string directory)
        {
            NativeAbi.EnsureLoaded();
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
        /// index has several segments or no terms. Throws <see cref="SeforimEngineException"/>
        /// (with the reason) when the sidecar cannot be built; a failure is never silent.</summary>
        public bool BuildFmSidecar()
        {
            int built = NativeMethods.se_index_build_fm_sidecar(_handle);
            if (built < 0) throw new SeforimEngineException(Interop.LastError());
            return built == 1;
        }

        /// <summary>True when a sidecar matching the current single segment exists.</summary>
        public bool HasFmSidecar => NativeMethods.se_index_has_fm_sidecar(_handle) == 1;

        /// <summary>The engine's automatic FM sidecar rebuild (default on): a search that finds the
        /// sidecar of a one-segment index missing or unusable starts one background rebuild.</summary>
        public void SetFmAutoRepair(bool on) => Interop.Check(NativeMethods.se_index_set_fm_auto_repair(_handle, on ? 1 : 0));

        /// <summary>True while the engine's background FM sidecar rebuild runs.</summary>
        public bool FmSidecarRebuilding => NativeMethods.se_index_fm_sidecar_rebuilding(_handle) == 1;

        /// <summary>Why the FM sidecar file cannot be used (the reason behind the search
        /// notice <see cref="SearchInfo.FmSidecarUnusable"/>), "rebuilding: ..." while the engine's
        /// background rebuild runs, "FM sidecar rebuild failed: ..." after it failed, or null
        /// when it is usable or absent with nothing tried. Rebuild it with <see cref="BuildFmSidecar"/>.</summary>
        public string? FmSidecarProblem
        {
            get
            {
                string s = Interop.TakeString(NativeMethods.se_index_fm_sidecar_problem(_handle));
                return s.Length == 0 ? null : s;
            }
        }

        /// <summary>Number of lines in the index (a cheap count: no JSON, no payload), as of
        /// the last commit through this index's writers or the last <see cref="Reload"/>.</summary>
        public long NumLines
        {
            get
            {
                long n = NativeMethods.se_index_num_lines(_handle);
                if (n < 0) throw new SeforimEngineException(Interop.LastError());
                return n;
            }
        }

        /// <summary>Reloads the index's shared reader to the last commit on disk. Needed only
        /// when ANOTHER process (or another <see cref="SeforimIndex"/> on the same directory)
        /// wrote the index; this index's own writers reload it themselves. Running searches
        /// keep their snapshot.</summary>
        public void Reload()
        {
            if (NativeMethods.se_index_reload(_handle) != 0)
                throw new SeforimEngineException(Interop.LastError());
        }

        /// <summary>Starts the background full merge after an update (issue #29): its own thread
        /// at background priority merges every segment into one and rebuilds the FM sidecar;
        /// searches go on and switch to the merged segment when it is published. <paramref name="bookOrder"/>
        /// null = a plain merge (positions kept; nothing to do on one segment); not null = a REPACK
        /// in that book order (every position renumbered densely, the re-sort's rules), needed
        /// after an update that changed a book's line count or added / removed a book. The
        /// request is recorded beside the index and restarted by <see cref="Open"/> when it did not
        /// finish. Opening a writer stops it first. Returns false when there is nothing to do.</summary>
        public bool StartBackgroundMerge(long[]? bookOrder = null) => BackgroundMerge(bookOrder, 0) == 1;

        /// <summary>Records a background merge WITHOUT starting it: a host about to write an update
        /// that needs a repack records it first, so a process that ends mid-update still repacks at
        /// the next open or <see cref="StartBackgroundMerge"/>. null keeps a recorded order.</summary>
        public void RecordBackgroundMerge(long[]? bookOrder) => BackgroundMerge(bookOrder, 1);

        private int BackgroundMerge(long[]? bookOrder, uint flags)
        {
            int r;
            if (bookOrder == null)
                r = NativeMethods.se_index_start_background_merge(_handle, null, UIntPtr.Zero, flags);
            else
            {
                var ids = new ulong[Math.Max(1, bookOrder.Length)];
                for (int i = 0; i < bookOrder.Length; i++) ids[i] = (ulong)bookOrder[i];
                fixed (ulong* p = ids) r = NativeMethods.se_index_start_background_merge(_handle, p, (UIntPtr)bookOrder.Length, flags);
            }
            if (r < 0) throw new SeforimEngineException(Interop.LastError());
            return r;
        }

        /// <summary>True while the background merge runs.</summary>
        public bool BackgroundMergeRunning => (BackgroundMergeState() & 1) != 0;

        /// <summary>True while a background merge is recorded and not finished (running,
        /// stopped, failed or interrupted).</summary>
        public bool BackgroundMergePending => (BackgroundMergeState() & 2) != 0;

        private int BackgroundMergeState()
        {
            int s = NativeMethods.se_index_background_merge_state(_handle);
            if (s < 0) throw new SeforimEngineException(Interop.LastError());
            return s;
        }

        /// <summary>Waits for the background merge (blocking); throws its error when it failed.</summary>
        public void WaitBackgroundMerge()
        {
            if (NativeMethods.se_index_wait_background_merge(_handle) != 0)
                throw new SeforimEngineException(Interop.LastError());
        }

        /// <summary>Stops the background merge (a repack at once, a plain merge already running
        /// is waited for); its record stays, so the next open or start restarts it.</summary>
        public void StopBackgroundMerge()
        {
            if (NativeMethods.se_index_stop_background_merge(_handle) != 0)
                throw new SeforimEngineException(Interop.LastError());
        }

        /// <summary>Starts a writer in the AUTO mode (the default, issue #19): one thread per
        /// core in the OS background mode, and a memory budget that follows the machine
        /// (large while idle; the minimum under memory pressure or while the user is active,
        /// the workers flushing their segments to free the buffer).</summary>
        public SeforimIndexWriter OpenWriter()
        {
            return new SeforimIndexWriter(Interop.Check(NativeMethods.se_writer_open_ex(
                _handle, UIntPtr.Zero, UIntPtr.Zero, NativeMethods.SE_WRITER_AUTO)));
        }

        /// <summary>Starts a writer with FIXED resources. <paramref name="heapBytes"/> is the
        /// total indexing memory (at least 15 MB per thread; threads are capped by it);
        /// 0 gives 50 MB per thread, so <paramref name="threads"/> (0 = cores, at most 4)
        /// are always granted. With <paramref name="lowPriority"/>, the indexing threads and
        /// the background merges run in the OS background mode.</summary>
        public SeforimIndexWriter OpenWriter(long heapBytes, int threads = 0, bool lowPriority = false)
        {
            if (threads <= 0) threads = Math.Min(Environment.ProcessorCount, 4);
            if (heapBytes <= 0) heapBytes = threads * 50_000_000L;
            return new SeforimIndexWriter(Interop.Check(NativeMethods.se_writer_open_ex(
                _handle, (UIntPtr)(ulong)heapBytes, (UIntPtr)(uint)threads,
                lowPriority ? NativeMethods.SE_WRITER_LOW_PRIORITY : 0u)));
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

        /// <summary>
        /// <see cref="Search(SearchOptions, CancellationToken)"/> that returns as soon as
        /// the first hits are decided (issue #21): on an index with positions the rest
        /// stream in, in the search's order; take them in with
        /// <see cref="SearchSession.Pull"/> (or page through
        /// <see cref="SearchSession.AllHits"/> / <c>HitBatchesAsync</c>, which wait for
        /// each page). Every session call works on the hits taken in so far; the counts
        /// are exact once <see cref="SearchSession.IsComplete"/>. Cancelling
        /// <paramref name="ct"/> stops the stream. On an index without positions the
        /// session is complete at once.
        /// </summary>
        public SearchSession SearchStreaming(SearchOptions options, CancellationToken ct = default)
        {
            ct.ThrowIfCancellationRequested();
            var token = NativeMethods.se_cancel_token_new();
            var registration = ct.CanBeCanceled
                ? ct.Register(s => NativeMethods.se_cancel_token_cancel(((CancelTokenHandle)s!).DangerousGetHandle()), token)
                : default;
            SearchSession? session = null;
            try
            {
                byte[] json = Interop.ToUtf8(options.ToJson());
                fixed (byte* j = json)
                    session = new SearchSession(
                        Interop.Check(NativeMethods.se_search_stream(_handle, j, (UIntPtr)json.Length, token)),
                        options.ContextWords, token, registration);
                return session.Streaming();
            }
            catch
            {
                if (session != null) session.Dispose();
                else
                {
                    registration.Dispose();
                    token.Dispose();
                }
                throw;
            }
        }

        public void Dispose() => _handle.Dispose();
    }

    /// <summary>What a re-sort (<see cref="SeforimIndexWriter.ResortAndClose"/>) is doing.</summary>
    public enum ResortStage
    {
        /// <summary>Not started yet.</summary>
        NotStarted = 0,
        /// <summary>Reading the lines' books and positions, building the new order.</summary>
        Plan = 1,
        /// <summary>Writing the postings (Done / Total terms).</summary>
        Postings = 2,
        /// <summary>Writing the doc store.</summary>
        Store = 3,
        /// <summary>Writing the per-line fields (position rewritten).</summary>
        FastFields = 4,
        /// <summary>Publishing the new segment, removing the old one.</summary>
        Publish = 5,
        /// <summary>Rebuilding the FM-index sidecar.</summary>
        FmSidecar = 6,
        /// <summary>Finished (Done = lines).</summary>
        Done = 7,
    }

    /// <summary>A re-sort progress report.</summary>
    public readonly struct ResortProgress
    {
        /// <summary>A report.</summary>
        public ResortProgress(ResortStage stage, long done, long total) { Stage = stage; Done = done; Total = total; }
        /// <summary>The stage.</summary>
        public ResortStage Stage { get; }
        /// <summary>Work done in the stage.</summary>
        public long Done { get; }
        /// <summary>The stage's total work (an upper bound for postings).</summary>
        public long Total { get; }
    }

    /// <summary>Adds, deletes and commits lines. Not thread-safe.</summary>
    public sealed unsafe class SeforimIndexWriter : IDisposable
    {
        private readonly WriterHandle _handle;

        internal SeforimIndexWriter(WriterHandle handle) { _handle = handle; }

        /// <summary>The current memory budget per indexing thread, in bytes (it moves in
        /// the auto mode).</summary>
        public long MemoryBudgetPerThread => (long)NativeMethods.se_writer_memory_budget(_handle);

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

        /// <summary>Queues deletion of every line of a book (issue #29, a per-book update: delete
        /// the book, add its new lines, then ONE commit). Lines added after this call are kept.</summary>
        public void DeleteBook(long bookId) => Interop.Check(NativeMethods.se_writer_delete_book(_handle, (ulong)bookId));

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

        /// <summary>Commits pending adds and deletes (keeping the last payload), merges
        /// all segments into one (for a shipped, read-mostly index) and closes the writer.</summary>
        public void MergeAndClose() => Interop.Check(NativeMethods.se_writer_merge_and_close(_handle.Detach()));

        /// <summary>
        /// Re-sorts the index into a new catalog order and closes the writer (optional; a
        /// build never needs it). <paramref name="bookOrder"/> = every indexed book id in the
        /// new catalog order (books without lines may appear; indexed books not listed go
        /// last in their old order; a book listed twice throws). Each book keeps its lines in their order and spacing; books
        /// are packed from position 0 in the new order. One reordering merge, published
        /// atomically (a resume from a page of the old order then fails as stale); the FM
        /// sidecar is rebuilt. Cancelling <paramref name="ct"/> before the publish leaves the
        /// old index and throws <see cref="OperationCanceledException"/>.
        /// <paramref name="progress"/> gets a report about every 250 ms. Blocking (see
        /// <see cref="ResortAndCloseAsync"/>); runs at background priority when the writer
        /// does. Disk: about twice the index size at the peak. Returns the number of lines.
        /// A null or negative book id, or <paramref name="ct"/> cancelled before the call, throws
        /// without starting and leaves the writer open.
        /// </summary>
        public long ResortAndClose(IReadOnlyList<long> bookOrder, IProgress<ResortProgress>? progress = null,
                                   CancellationToken ct = default)
        {
            if (bookOrder == null) throw new ArgumentNullException(nameof(bookOrder));
            ct.ThrowIfCancellationRequested();
            var ids = new ulong[bookOrder.Count];
            for (int i = 0; i < ids.Length; i++) ids[i] = checked((ulong)bookOrder[i]);
            using var token = NativeMethods.se_cancel_token_new();
            using var registration = ct.Register(s => NativeMethods.se_cancel_token_cancel(((CancelTokenHandle)s!).DangerousGetHandle()), token);
            IntPtr cell = NativeMethods.se_resort_progress_new();
            Timer? timer = null;
            try
            {
                if (progress != null)
                    timer = new Timer(_ => Report(cell, progress), null, 250, 250);
                long lines;
                fixed (ulong* p = ids)
                    lines = NativeMethods.se_writer_resort_and_close(_handle.Detach(), p, (UIntPtr)ids.Length, token, cell);
                if (lines < 0) throw Interop.Fail(Interop.LastError());
                return lines;
            }
            finally
            {
                if (timer != null)
                {
                    using var stopped = new ManualResetEvent(false);
                    if (timer.Dispose(stopped)) stopped.WaitOne();
                    if (progress != null) Report(cell, progress);
                }
                NativeMethods.se_resort_progress_free(cell);
            }
        }

        /// <summary><see cref="ResortAndClose"/> on the thread pool.</summary>
        public System.Threading.Tasks.Task<long> ResortAndCloseAsync(IReadOnlyList<long> bookOrder,
            IProgress<ResortProgress>? progress = null, CancellationToken ct = default) =>
            System.Threading.Tasks.Task.Run(() => ResortAndClose(bookOrder, progress, ct));

        private static void Report(IntPtr cell, IProgress<ResortProgress> progress)
        {
            if (NativeMethods.se_resort_progress_get(cell, out uint stage, out ulong done, out ulong total) == 0)
                progress.Report(new ResortProgress((ResortStage)stage, (long)done, (long)total));
        }

        /// <summary>Closes the writer, discarding anything uncommitted.</summary>
        public void Dispose() => _handle.Dispose();
    }
}
