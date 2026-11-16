using System;
using System.IO;
using SeforimSearch.Demo.SeforimDb;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// The default <see cref="ISeforimDbFtsDataSource"/>: seforim.db through this project's
    /// SQLite code (a <see cref="SeforimDbFtsCorpusPool"/> keeps it open across searches; the
    /// provenance stamp is <see cref="SeforimDbContentFingerprint"/>). Used by the demo CLI and
    /// the selftest; a host with its own seforim.db layer implements the interface on that instead.
    /// </summary>
    public sealed class SeforimDbSqliteDataSource : ISeforimDbFtsDataSource, IDisposable
    {
        private readonly SeforimDbFtsCorpusPool _pool;

        public SeforimDbSqliteDataSource(string databasePath)
        {
            DatabasePath = databasePath ?? throw new ArgumentNullException(nameof(databasePath));
            _pool = new SeforimDbFtsCorpusPool(databasePath);
        }

        public string DatabasePath { get; }

        public bool IsAvailable => !string.IsNullOrWhiteSpace(DatabasePath) && File.Exists(DatabasePath);

        public ISeforimDbFtsCorpus OpenCorpus() => _pool.Rent();

        public string ContentFingerprint(string prefix) => SeforimDbContentFingerprint.Compute(DatabasePath, prefix);

        public string? DescribeFingerprintChange(string? recorded, string current) =>
            SeforimDbContentFingerprint.DescribeChange(recorded, current);

        public void ReleaseIdle() => _pool.ReleaseIdle();

        public void Dispose() => _pool.Dispose();
    }
}
