use std::{collections::HashSet, path::Path};

use crate::{cli::SharedArgs, read_files_parallel, LoadedEntry, PricingMap, Result};

use super::{
    parser::{read_model_usage_row, to_loaded_entry},
    paths::database_paths,
};

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::ZCode, shared.json, || {
        let tz = crate::parse_tz(shared.timezone.as_deref());
        let db_paths = database_paths()?;
        let loaded = read_files_parallel(&db_paths, shared.single_thread, |path| {
            load_database_entries(path, shared, tz.as_ref(), pricing)
        });
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for db_entries in loaded {
            for entry in db_entries {
                if entry
                    .data
                    .message
                    .id
                    .as_ref()
                    .is_some_and(|id| seen.insert(id.clone()))
                {
                    entries.push(entry);
                }
            }
        }
        entries.sort_by_key(|entry| entry.timestamp);
        Ok(entries)
    })
}

fn load_database_entries(
    db_path: &Path,
    shared: &SharedArgs,
    tz: Option<&jiff::tz::TimeZone>,
    pricing: &PricingMap,
) -> Vec<LoadedEntry> {
    let Ok(connection) =
        sqlite::Connection::open_with_flags(db_path, sqlite::OpenFlags::new().with_read_only())
    else {
        crate::debug_log(
            shared,
            format!("Failed to open ZCode database: {}", db_path.display()),
        );
        return Vec::new();
    };
    if connection.execute("PRAGMA query_only = ON").is_err() {
        crate::debug_log(
            shared,
            format!(
                "Failed to open ZCode database in query-only mode: {}",
                db_path.display()
            ),
        );
        return Vec::new();
    }
    let Ok(mut statement) = connection.prepare(
        "
            SELECT
                model_usage.id,
                model_usage.session_id,
                model_usage.provider_id,
                model_usage.model_id,
                model_usage.started_at,
                model_usage.input_tokens,
                model_usage.output_tokens,
                model_usage.cache_creation_input_tokens,
                model_usage.cache_read_input_tokens,
                session.directory,
                session.version
            FROM model_usage
            JOIN session ON session.id = model_usage.session_id
            WHERE model_usage.input_tokens > 0
                OR model_usage.output_tokens > 0
                OR model_usage.cache_creation_input_tokens > 0
                OR model_usage.cache_read_input_tokens > 0
            ORDER BY model_usage.started_at, model_usage.id
        ",
    ) else {
        crate::debug_log(
            shared,
            format!("Failed to read ZCode database: {}", db_path.display()),
        );
        return Vec::new();
    };
    let mut entries = Vec::new();
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                if let Some(entry) = read_model_usage_row(&statement) {
                    entries.push(to_loaded_entry(entry, tz, shared.mode, pricing));
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => {
                crate::debug_log(
                    shared,
                    format!("Failed to query ZCode database: {}", db_path.display()),
                );
                break;
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::fs_fixture;

    #[test]
    fn loads_zcode_model_usage_with_sessions_and_cache_tokens() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("db.sqlite");
        create_database(&db_path);
        let db = sqlite::open(&db_path).unwrap();
        db.execute(
            "INSERT INTO session (id, directory, version) VALUES ('sess_test', '/work/api', '0.15.0')",
        )
        .unwrap();
        db.execute(
            "
                INSERT INTO model_usage (
                    id, session_id, provider_id, model_id, started_at,
                    input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens
                ) VALUES (
                    'usage_test', 'sess_test', 'builtin:zai-coding-plan', 'GLM-5.2',
                    1783344730135, 100, 25, 20, 30
                )
            ",
        )
        .unwrap();
        drop(db);

        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_database_entries(
            &db_path,
            &shared,
            crate::parse_tz(shared.timezone.as_deref()).as_ref(),
            &PricingMap::load_embedded(),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "sess_test");
        assert_eq!(entries[0].project_path.as_ref(), "/work/api");
        assert_eq!(entries[0].model.as_deref(), Some("GLM-5.2"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 50);
        assert_eq!(
            entries[0].data.message.usage.cache_creation_input_tokens,
            20
        );
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 30);
        assert_eq!(entries[0].data.message.usage.output_tokens, 25);
        assert!(entries[0].cost > 0.0);
        assert_eq!(entries[0].message_count, Some(1));
        assert_eq!(entries[0].data.version.as_deref(), Some("0.15.0"));

        let rows =
            super::super::report::summarize_entries(&entries, crate::cli::AgentReportKind::Session)
                .unwrap();
        insta::assert_json_snapshot!(super::super::report::report_from_rows(
            &rows,
            crate::cli::AgentReportKind::Session,
        ));
    }

    #[test]
    #[ignore = "requires local ZCode session data"]
    fn loads_local_zcode_database() {
        let entries = load_entries(&SharedArgs::default(), &PricingMap::load_embedded()).unwrap();
        assert!(!entries.is_empty());
    }

    #[test]
    fn deduplicates_model_usage_across_zcode_roots() {
        let _guard = super::super::paths::ZCODE_DATA_DIR_LOCK.lock().unwrap();
        let first = fs_fixture!({ "cli/db/.keep": "" });
        let second = fs_fixture!({ "cli/db/.keep": "" });
        for fixture in [&first, &second] {
            let db_path = fixture.path("cli/db/db.sqlite");
            create_database(&db_path);
            let db = sqlite::open(db_path).unwrap();
            db.execute(
                "INSERT INTO session (id, directory, version) VALUES ('sess_test', '/work/api', '0.15.0')",
            )
            .unwrap();
            db.execute(
                "
                    INSERT INTO model_usage (
                        id, session_id, provider_id, model_id, started_at,
                        input_tokens, output_tokens,
                        cache_creation_input_tokens, cache_read_input_tokens
                    ) VALUES (
                        'usage_test', 'sess_test', 'builtin:zai-coding-plan', 'GLM-5.2',
                        1783344730135, 100, 25, 20, 30
                    )
                ",
            )
            .unwrap();
        }
        std::env::set_var(
            super::super::paths::ZCODE_DATA_DIR_ENV,
            format!("{},{}", first.root().display(), second.root().display()),
        );

        let entries = load_entries(&SharedArgs::default(), &PricingMap::load_embedded()).unwrap();

        std::env::remove_var(super::super::paths::ZCODE_DATA_DIR_ENV);
        assert_eq!(entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn loads_read_only_zcode_database() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = fs_fixture!({});
        let db_path = fixture.path("db.sqlite");
        create_database(&db_path);
        let db = sqlite::open(&db_path).unwrap();
        db.execute(
            "INSERT INTO session (id, directory, version) VALUES ('sess_test', '/work/api', '0.15.0')",
        )
        .unwrap();
        db.execute(
            "
                INSERT INTO model_usage (
                    id, session_id, provider_id, model_id, started_at,
                    input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens
                ) VALUES (
                    'usage_test', 'sess_test', 'builtin:zai-coding-plan', 'GLM-5.2',
                    1783344730135, 100, 25, 20, 30
                )
            ",
        )
        .unwrap();
        drop(db);
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let entries = load_database_entries(
            &db_path,
            &SharedArgs::default(),
            None,
            &PricingMap::load_embedded(),
        );

        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(entries.len(), 1);
    }

    fn create_database(path: &Path) {
        let db = sqlite::open(path).unwrap();
        db.execute(
            "
                CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    version TEXT NOT NULL
                );
                CREATE TABLE model_usage (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    provider_id TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    started_at INTEGER NOT NULL,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0
                );
            ",
        )
        .unwrap();
    }
}
