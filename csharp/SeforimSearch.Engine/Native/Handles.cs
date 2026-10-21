using System;
using System.Runtime.InteropServices;
using System.Text;

namespace SeforimSearch.Engine.Native
{
    /// <summary>Owns a native index pointer; freed exactly once.</summary>
    internal sealed class IndexHandle : SafeHandle
    {
        public IndexHandle() : base(IntPtr.Zero, true) { }
        public override bool IsInvalid => handle == IntPtr.Zero;
        protected override bool ReleaseHandle() { NativeMethods.se_index_close(handle); return true; }
    }

    /// <summary>Owns a native writer pointer; freed exactly once.</summary>
    internal sealed class WriterHandle : SafeHandle
    {
        public WriterHandle() : base(IntPtr.Zero, true) { }
        public override bool IsInvalid => handle == IntPtr.Zero;
        protected override bool ReleaseHandle() { NativeMethods.se_writer_close(handle); return true; }

        /// <summary>Hands the pointer to a native call that consumes it.</summary>
        public IntPtr Detach()
        {
            IntPtr h = handle;
            SetHandleAsInvalid();
            return h;
        }
    }

    /// <summary>Owns a native search pointer; freed exactly once.</summary>
    internal sealed class SearchHandle : SafeHandle
    {
        public SearchHandle() : base(IntPtr.Zero, true) { }
        public override bool IsInvalid => handle == IntPtr.Zero;
        protected override bool ReleaseHandle() { NativeMethods.se_search_close(handle); return true; }
    }

    /// <summary>Owns a native cancel token; freed exactly once (a search holding
    /// it keeps its own reference inside the engine).</summary>
    internal sealed class CancelTokenHandle : SafeHandle
    {
        public CancelTokenHandle() : base(IntPtr.Zero, true) { }
        public override bool IsInvalid => handle == IntPtr.Zero;
        protected override bool ReleaseHandle() { NativeMethods.se_cancel_token_free(handle); return true; }
    }

    /// <summary>UTF-8 marshalling and native-error helpers.</summary>
    internal static class Interop
    {
        private static readonly UTF8Encoding Utf8 = new UTF8Encoding(false);

        public static byte[] ToUtf8(string? s) => s == null ? Array.Empty<byte>() : Utf8.GetBytes(s);

        public static unsafe string FromUtf8Z(IntPtr p)
        {
            if (p == IntPtr.Zero) return "";
            byte* b = (byte*)p;
            int len = 0;
            while (b[len] != 0) len++;
            return Utf8.GetString(b, len);
        }

        /// <summary>The native library's last error for this thread.</summary>
        public static string LastError()
        {
            string message = FromUtf8Z(NativeMethods.se_last_error());
            return message.Length == 0 ? "unknown native error" : message;
        }

        /// <summary>The exception for the last native error: an
        /// <see cref="OperationCanceledException"/> when the search was cancelled.</summary>
        public static Exception Fail(string message) =>
            NativeMethods.se_last_error_cancelled() != 0
                ? new OperationCanceledException(message)
                : new SeforimEngineException(message);

        /// <summary>Throws when a status call failed.</summary>
        public static void Check(int status)
        {
            if (status != 0) throw Fail(LastError());
        }

        /// <summary>Throws when a handle-returning call failed.</summary>
        public static T Check<T>(T handle) where T : SafeHandle
        {
            if (handle.IsInvalid) throw Fail(LastError());
            return handle;
        }

        /// <summary>Reads and frees an engine-owned JSON string.</summary>
        public static string TakeString(IntPtr p)
        {
            if (p == IntPtr.Zero) throw Fail(LastError());
            try { return FromUtf8Z(p); }
            finally { NativeMethods.se_string_free(p); }
        }

        /// <summary>Copies and frees an engine-owned u32 buffer.</summary>
        public static uint[] TakeBuffer(SeBuffer buffer)
        {
            if (buffer.Data == IntPtr.Zero && (ulong)buffer.Len == 0 && (ulong)buffer.Cap == 0)
            {
                // Either an empty result or an error; errors leave a message.
                string error = FromUtf8Z(NativeMethods.se_last_error());
                if (error.Length != 0) throw Fail(error);
                return Array.Empty<uint>();
            }
            try
            {
                int len = checked((int)(ulong)buffer.Len);
                var result = new uint[len];
                if (len > 0)
                {
                    var asInts = new int[len];
                    Marshal.Copy(buffer.Data, asInts, 0, len);
                    Buffer.BlockCopy(asInts, 0, result, 0, len * 4);
                }
                return result;
            }
            finally
            {
                NativeMethods.se_buffer_free(buffer);
            }
        }
    }
}
