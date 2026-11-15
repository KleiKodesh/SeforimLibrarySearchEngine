using System;

namespace SeforimSearch.Demo.Common
{
    /// <summary>
    /// Core's IndexBuildReason (KitveiHakodesh.Core.Common), same shape: why the index last
    /// started building (no index yet, a stale stamp in words, a reset) and when. The
    /// coordinator records one per build, logs it and reports it in its status.
    /// </summary>
    public sealed class IndexBuildReason
    {
        public string Reason { get; set; } = "";
        public DateTime AtUtc { get; set; }

        public static IndexBuildReason Now(string reason) => new IndexBuildReason { Reason = reason, AtUtc = DateTime.UtcNow };
    }
}
