using System;
using System.Collections.Generic;
using System.IO;
using Microsoft.Data.Sqlite;
using SeforimSearch.Demo.SeforimDb;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// The default <see cref="ISeforimDbRelatedFormLexicon"/>: the related-forms artifact
    /// (expansion-routed.db) read with SQLite. Schema: fold(surface, lemma, weight, source),
    /// exp(lemma, rank, form, channel, source); older artifacts have no fold.weight (one reading
    /// per surface at weight 1, detected once per lexicon).
    /// </summary>
    public sealed class SeforimDbSqliteRelatedFormLexicon : ISeforimDbRelatedFormLexicon
    {
        // Two one-line statements — consts here, not a SQL file (rule 9 threshold).
        private const string LookupLemmasSql =
            "SELECT lemma, weight FROM fold WHERE surface = @s ORDER BY weight DESC, lemma";
        private const string LookupLemmasNoWeightSql =
            "SELECT lemma, 1.0 FROM fold WHERE surface = @s ORDER BY lemma";
        private const string LookupFormsSql =
            "SELECT form, channel, source FROM exp WHERE lemma = @l ORDER BY rank";

        private readonly string? _databasePath;

        /// <summary>fold has a weight column. Read from the schema once, on first use.</summary>
        private bool? _foldHasWeight;

        /// <param name="databasePath">The artifact, or null for "not installed".</param>
        public SeforimDbSqliteRelatedFormLexicon(string? databasePath)
        {
            _databasePath = databasePath;
        }

        /// <summary>Core's probe: <c>SearchExpansion/expansion-routed.db</c> next to the app.</summary>
        public static SeforimDbSqliteRelatedFormLexicon FindNextToApp()
        {
            string path = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "SearchExpansion", "expansion-routed.db");
            return new SeforimDbSqliteRelatedFormLexicon(File.Exists(path) ? path : null);
        }

        public bool IsAvailable => !string.IsNullOrWhiteSpace(_databasePath) && File.Exists(_databasePath);

        public ISeforimDbRelatedFormLookup Open()
        {
            var connection = Common.SqliteConnectionFactory.OpenCorpusRead(_databasePath!);
            try
            {
                _foldHasWeight ??= FoldHasWeightColumn(connection);
                return new Lookup(connection, _foldHasWeight.Value);
            }
            catch
            {
                connection.Dispose();
                throw;
            }
        }

        private static bool FoldHasWeightColumn(SqliteConnection connection)
        {
            using var command = connection.CreateCommand();
            command.CommandText = "SELECT 1 FROM pragma_table_info('fold') WHERE name = 'weight'";
            return command.ExecuteScalar() != null;
        }

        private sealed class Lookup : ISeforimDbRelatedFormLookup
        {
            private readonly SqliteConnection _connection;
            private readonly SqliteCommand _lemmas, _forms;
            private readonly SqliteParameter _surface, _lemma;

            public Lookup(SqliteConnection connection, bool foldHasWeight)
            {
                _connection = connection;
                _lemmas = connection.CreateCommand();
                _lemmas.CommandText = foldHasWeight ? LookupLemmasSql : LookupLemmasNoWeightSql;
                _surface = _lemmas.Parameters.Add("@s", SqliteType.Text);
                _forms = connection.CreateCommand();
                _forms.CommandText = LookupFormsSql;
                _lemma = _forms.Parameters.Add("@l", SqliteType.Text);
            }

            public List<(string? Lemma, double? Weight)> Lemmas(string surface)
            {
                _surface.Value = surface;
                var rows = new List<(string? Lemma, double? Weight)>(2);
                using var reader = _lemmas.ExecuteReader();
                while (reader.Read())
                    rows.Add((reader.IsDBNull(0) ? null : reader.GetString(0), reader.IsDBNull(1) ? (double?)null : reader.GetDouble(1)));
                return rows;
            }

            public IEnumerable<(string Form, string Channel, string Source)> Forms(string lemma)
            {
                _lemma.Value = lemma;
                using var reader = _forms.ExecuteReader();
                while (reader.Read())
                    yield return (reader.GetString(0), reader.GetString(1), reader.GetString(2));
            }

            public void Dispose()
            {
                _lemmas.Dispose();
                _forms.Dispose();
                _connection.Dispose();
            }
        }
    }
}
