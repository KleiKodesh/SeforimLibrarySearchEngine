using System;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>
    /// Puts the CALLING OS thread into the OS background mode (low CPU, and where the OS
    /// supports it disk and memory priority) for the scope's lifetime, set natively by the
    /// engine (Windows THREAD_MODE_BACKGROUND_BEGIN, Linux SCHED_IDLE + idle I/O class,
    /// Apple background QoS, Android background nice), unlike Thread.Priority, which only
    /// touches the CPU on Windows and does nothing on Linux. For the host thread that reads
    /// the corpus and feeds the writer:
    /// <code>using (BackgroundPriority.Enter()) { ... writer.Add(...) ... }</code>
    /// Nesting-safe. MUST be disposed on the thread that entered it (no await inside the
    /// scope). Commits and the FM sidecar build leave the mode for their duration by
    /// themselves, so a reader waiting on them is never throttled.
    /// </summary>
    public sealed class BackgroundPriority : IDisposable
    {
        private bool _disposed;

        private BackgroundPriority()
        {
            NativeAbi.EnsureLoaded();
            NativeMethods.se_thread_background_begin();
        }

        /// <summary>Enters the background mode on the calling thread.</summary>
        public static BackgroundPriority Enter() => new BackgroundPriority();

        /// <summary>Leaves what <see cref="Enter"/> entered.</summary>
        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            NativeMethods.se_thread_background_end();
        }
    }
}
