using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Threading;

namespace FtsLib.Fts5
{
    /// <summary>
    /// Peak process memory of one build stage (issue #20). The OS keeps only a LIFETIME peak
    /// (Windows <c>PROCESS_MEMORY_COUNTERS.PeakWorkingSetSize</c>, Linux <c>VmHWM</c>, both read
    /// by <see cref="Process.PeakWorkingSet64"/>), so a stage's own peak is measured as:
    /// the working set sampled every <see cref="SampleMs"/> ms on a timer thread during the
    /// stage, raised to the lifetime peak when that peak grew during the stage (then the stage
    /// itself set it, so it is exact). A spike shorter than the sample interval that stays below
    /// an earlier stage's peak can be missed; that is the method's only blind spot.
    /// </summary>
    public sealed class StageMemorySampler : IDisposable
    {
        public const int SampleMs = 50;
        private readonly Process _self = Process.GetCurrentProcess();
        private readonly Timer _timer;
        private readonly object _gate = new object();
        private long _startWs, _startLifetimePeak, _maxSampled;

        public StageMemorySampler()
        {
            Restart();
            _timer = new Timer(_ => Sample(), null, SampleMs, SampleMs);
        }

        private (long Ws, long Peak) Read()
        {
            lock (_gate)
            {
                _self.Refresh();
                return (_self.WorkingSet64, _self.PeakWorkingSet64);
            }
        }

        private void Sample()
        {
            long ws = Read().Ws;
            long cur;
            while (ws > (cur = Interlocked.Read(ref _maxSampled)) &&
                   Interlocked.CompareExchange(ref _maxSampled, ws, cur) != cur) { }
        }

        /// <summary>Starts a new stage: its baseline is the working set now.</summary>
        public void Restart()
        {
            var (ws, peak) = Read();
            _startWs = ws;
            _startLifetimePeak = peak;
            Interlocked.Exchange(ref _maxSampled, ws);
        }

        /// <summary>The stage's peak and its rise over the stage's start, then a new stage starts.</summary>
        public StageMemory Take()
        {
            Sample();
            var (_, peak) = Read();
            long stagePeak = Interlocked.Read(ref _maxSampled);
            if (peak > _startLifetimePeak && peak > stagePeak) stagePeak = peak;
            var m = new StageMemory(stagePeak, stagePeak - _startWs, _startWs);
            Restart();
            return m;
        }

        public void Dispose() { _timer.Dispose(); _self.Dispose(); }
    }

    /// <summary>Peak working set of a build stage (bytes) and its rise over the working set
    /// at the stage's start.</summary>
    public readonly struct StageMemory
    {
        public StageMemory(long peak, long delta, long start) { Peak = peak; Delta = delta; Start = start; }
        public long Peak { get; }
        public long Delta { get; }
        public long Start { get; }
        public override string ToString() => $"peak {Peak / 1e6:F1} MB ({(Delta >= 0 ? "+" : "")}{Delta / 1e6:F1} MB over the stage start)";
    }

    /// <summary>
    /// The build's read queue (issue #20): lines read from seforim.db by the reader thread wait
    /// here for the indexing thread. Bounded by line count AND by text bytes (UTF-16, 2 per
    /// char), so a book of huge lines cannot balloon memory; a single line larger than the byte
    /// cap is still admitted, alone (the queue must be empty first). Both sides' waits are timed.
    /// </summary>
    public sealed class BoundedLineQueue<T>
    {
        private readonly Queue<(T Item, long Bytes)> _items = new Queue<(T, long)>();
        private readonly int _maxLines;
        private readonly long _maxBytes;
        private long _bytes;
        private bool _completed;
        private Exception? _error;

        public BoundedLineQueue(int maxLines, long maxBytes)
        {
            _maxLines = Math.Max(1, maxLines);
            _maxBytes = Math.Max(1, maxBytes);
        }

        public int MaxLines => _maxLines;
        public long MaxBytes => _maxBytes;
        /// <summary>Time the reader waited for room (the indexer was the bottleneck).</summary>
        public TimeSpan ProducerWait { get; private set; }
        /// <summary>Time the indexer waited for lines (the reader was the bottleneck).</summary>
        public TimeSpan ConsumerWait { get; private set; }
        public int PeakLines { get; private set; }
        public long PeakBytes { get; private set; }

        /// <summary>Adds a line, waiting while the queue is full. False when the consumer closed it.</summary>
        public bool Add(T item, long bytes)
        {
            lock (_items)
            {
                if (_completed) return false;
                if (Full(bytes))
                {
                    var sw = Stopwatch.StartNew();
                    while (!_completed && Full(bytes)) Monitor.Wait(_items);
                    ProducerWait += sw.Elapsed;
                    if (_completed) return false;
                }
                _items.Enqueue((item, bytes));
                _bytes += bytes;
                if (_items.Count > PeakLines) PeakLines = _items.Count;
                if (_bytes > PeakBytes) PeakBytes = _bytes;
                Monitor.PulseAll(_items);
                return true;
            }
        }

        private bool Full(long bytes) =>
            _items.Count > 0 && (_items.Count >= _maxLines || _bytes + bytes > _maxBytes);

        /// <summary>Takes the next line, waiting while empty. False at the end (rethrows the
        /// reader's error, if it failed).</summary>
        public bool TryTake(out T item)
        {
            lock (_items)
            {
                if (_items.Count == 0 && !_completed)
                {
                    var sw = Stopwatch.StartNew();
                    while (_items.Count == 0 && !_completed) Monitor.Wait(_items);
                    ConsumerWait += sw.Elapsed;
                }
                if (_items.Count == 0)
                {
                    item = default!;
                    if (_error != null) System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(_error).Throw();
                    return false;
                }
                var (it, b) = _items.Dequeue();
                _bytes -= b;
                Monitor.PulseAll(_items);
                item = it;
                return true;
            }
        }

        /// <summary>No more lines (reader done, failed, or the consumer stopped early).</summary>
        public void Complete(Exception? error = null)
        {
            lock (_items)
            {
                _completed = true;
                _error ??= error;
                Monitor.PulseAll(_items);
            }
        }

        public override string ToString() =>
            $"read queue (cap {_maxLines:N0} lines / {_maxBytes / 1e6:F0} MB): peak {PeakLines:N0} lines / {PeakBytes / 1e6:F1} MB; " +
            $"waits: reader {ProducerWait.TotalMilliseconds:F0} ms (queue full), indexer {ConsumerWait.TotalMilliseconds:F0} ms (queue empty)";
    }
}
