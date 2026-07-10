#[cfg(not(test))]
use std::time::UNIX_EPOCH;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::CodexTokenUsageEvent;

const CACHE_DIRECTORY_NAME: &str = "ccusage";
const SESSION_INDEX_SUBDIR: &str = "indexes";
const SESSION_INDEX_FILE_NAME: &str = "codex-session-index.json";
const SESSION_INDEX_SCHEMA_VERSION: u64 = 3;
#[cfg(test)]
const TEST_ENABLE_SESSION_INDEX_ENV: &str = "CCUSAGE_TEST_ENABLE_CODEX_SESSION_INDEX";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionIndexEntry {
    pub(super) file: String,
    pub(super) session_id: String,
    pub(super) size: u64,
    pub(super) mtime_ms: u64,
    pub(super) events: Vec<CodexTokenUsageEvent>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionIndexEnvelope {
    schema_version: u64,
    updated_at: String,
    entries: Vec<SessionIndexEntry>,
}

pub(super) fn read_session_index() -> BTreeMap<String, SessionIndexEntry> {
    let Some(path) = session_index_path() else {
        return BTreeMap::new();
    };
    let Ok(bytes) = fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(envelope) = serde_json::from_slice::<SessionIndexEnvelope>(&bytes) else {
        return BTreeMap::new();
    };
    if envelope.schema_version != SESSION_INDEX_SCHEMA_VERSION {
        return BTreeMap::new();
    }
    envelope
        .entries
        .into_iter()
        .map(|entry| (entry.file.clone(), entry))
        .collect()
}

pub(super) fn write_session_index(entries: &BTreeMap<String, SessionIndexEntry>) {
    let Some(path) = session_index_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp_path = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let envelope = SessionIndexEnvelope {
        schema_version: SESSION_INDEX_SCHEMA_VERSION,
        updated_at: crate::format_rfc3339_millis(crate::utc_now()),
        entries: entries.values().cloned().collect(),
    };
    let Ok(bytes) = serde_json::to_vec(&envelope) else {
        return;
    };
    if fs::write(&temp_path, bytes).is_ok() {
        let _ = fs::rename(&temp_path, path);
    }
    let _ = fs::remove_file(temp_path);
}

#[cfg(not(test))]
pub(super) fn file_state(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let mtime_ms = modified
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Some((metadata.len(), mtime_ms))
}

#[cfg(not(test))]
pub(super) fn cache_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(not(test))]
pub(super) fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string()
}

fn session_index_path() -> Option<PathBuf> {
    #[cfg(test)]
    env::var_os(TEST_ENABLE_SESSION_INDEX_ENV)?;

    let cache_home = match env::var_os("XDG_CACHE_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ if cfg!(test) => return None,
        _ => crate::home::home_dir()?.join(".cache"),
    };
    Some(
        cache_home
            .join(CACHE_DIRECTORY_NAME)
            .join(SESSION_INDEX_SUBDIR)
            .join(SESSION_INDEX_FILE_NAME),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::fs_fixture;
    use std::sync::Mutex;

    static XDG_CACHE_HOME_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let previous = env::var_os(key);
            env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            if let Some(value) = self.previous.take() {
                env::set_var(self.key, value);
            } else {
                env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn round_trips_cached_entries() {
        let _guard = XDG_CACHE_HOME_LOCK.lock().unwrap();
        let fixture = fs_fixture!({});
        let _env = EnvRestore::set_path("XDG_CACHE_HOME", fixture.root());
        let _enabled = EnvRestore::set_path(TEST_ENABLE_SESSION_INDEX_ENV, fixture.root());
        let mut entries = BTreeMap::new();
        entries.insert(
            "/tmp/session.jsonl".to_string(),
            SessionIndexEntry {
                file: "/tmp/session.jsonl".to_string(),
                session_id: "session".to_string(),
                size: 123,
                mtime_ms: 456,
                events: vec![CodexTokenUsageEvent {
                    timestamp: "2026-01-01T00:00:00.000Z".to_string(),
                    session_id: "session".to_string(),
                    model: Some("gpt-5".to_string()),
                    input_tokens: 1,
                    cached_input_tokens: 2,
                    cache_write_tokens: 0,
                    output_tokens: 3,
                    reasoning_output_tokens: 4,
                    total_tokens: 8,
                    is_fallback_model: false,
                }],
            },
        );

        write_session_index(&entries);
        let read = read_session_index();

        assert_eq!(read["/tmp/session.jsonl"].size, 123);
        assert_eq!(read["/tmp/session.jsonl"].events[0].cached_input_tokens, 2);
    }

    #[test]
    fn ignores_invalid_cache_contents() {
        let _guard = XDG_CACHE_HOME_LOCK.lock().unwrap();
        let fixture = fs_fixture!({
            "ccusage/indexes/codex-session-index.json": r#"{"schemaVersion":999,"entries":["invalid"]}"#,
        });
        let _env = EnvRestore::set_path("XDG_CACHE_HOME", fixture.root());
        let _enabled = EnvRestore::set_path(TEST_ENABLE_SESSION_INDEX_ENV, fixture.root());

        assert!(read_session_index().is_empty());
    }
}
