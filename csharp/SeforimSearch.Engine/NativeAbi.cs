using System;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>
    /// The C ABI check: the native library's <c>se_abi_version()</c> must equal
    /// <see cref="Expected"/> (engine-ffi <c>SE_ABI_VERSION</c>; tools/release/check_sync.py
    /// keeps them in sync). Every public entry point runs <see cref="EnsureLoaded"/> once.
    /// </summary>
    public static class NativeAbi
    {
        /// <summary>The C ABI version this package was built for.</summary>
        public const int Expected = 5;

        private static readonly object Gate = new object();
        private static volatile bool _checked;

        /// <summary>The loaded library's ABI version (0 = older than ABI versioning).</summary>
        public static int Loaded
        {
            get
            {
                try { return (int)NativeMethods.se_abi_version(); }
                catch (EntryPointNotFoundException) { return 0; }
            }
        }

        /// <summary>The path the native library was loaded from ("" when unknown).</summary>
        public static string LoadedPath
        {
            get
            {
                try { return Interop.FromUtf8Z(NativeMethods.se_native_library_path()); }
                catch (EntryPointNotFoundException) { return ""; }
            }
        }

        /// <summary>Checks the loaded library once; throws <see cref="SeforimEngineException"/>
        /// naming both versions and the library path on a mismatch.</summary>
        public static void EnsureLoaded()
        {
            if (_checked) return;
            lock (Gate)
            {
                if (_checked) return;
                Verify(Loaded, LoadedPath, Expected);
                _checked = true;
            }
        }

        /// <summary>Throws unless <paramref name="loaded"/> == <paramref name="expected"/>.</summary>
        public static void Verify(int loaded, string path, int expected = Expected)
        {
            if (loaded == expected) return;
            string where = string.IsNullOrEmpty(path) ? "seforim_search_native (path unknown)" : path;
            throw new SeforimEngineException(
                "seforim_search_native ABI mismatch: the .NET package SeforimSearch.Engine needs ABI " + expected +
                ", the library at \"" + where + "\" has ABI " +
                (loaded == 0 ? "0 (older than ABI versioning)" : loaded.ToString()) +
                ". Use the native library built for this package version.");
        }
    }
}
