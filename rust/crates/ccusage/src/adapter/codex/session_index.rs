#[cfg(not(test))]
use std::time::UNIX_EPOCH;
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::CodexTokenUsageEvent;

const CACHE_DIRECTORY_NAME: &str = "ccusage";
const SESSION_INDEX_SUBDIR: &str = "indexes";
const SESSION_INDEX_FILE_NAME: &str = "codex-session-index.json";
const SESSION_INDEX_SCHEMA_VERSION: u64 = 4;
const MAX_SESSION_INDEX_ENTRIES: usize = 4_096;
const MAX_SESSION_INDEX_BYTES: usize = 64 * 1024 * 1024;
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
    write_session_index_with_limits(entries, MAX_SESSION_INDEX_ENTRIES, MAX_SESSION_INDEX_BYTES);
}

fn write_session_index_with_limits(
    entries: &BTreeMap<String, SessionIndexEntry>,
    max_entries: usize,
    max_bytes: usize,
) {
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
    let updated_at = crate::format_rfc3339_millis(crate::utc_now());
    let Some(bytes) = serialize_bounded_session_index(entries, max_entries, max_bytes, &updated_at)
    else {
        let _ = fs::remove_file(path);
        return;
    };
    if fs::write(&temp_path, bytes).is_ok() {
        let _ = fs::rename(&temp_path, path);
    }
    let _ = fs::remove_file(temp_path);
}

fn serialize_bounded_session_index(
    entries: &BTreeMap<String, SessionIndexEntry>,
    max_entries: usize,
    max_bytes: usize,
    updated_at: &str,
) -> Option<Vec<u8>> {
    let mut envelope = SessionIndexEnvelope {
        schema_version: SESSION_INDEX_SCHEMA_VERSION,
        updated_at: updated_at.to_string(),
        entries: Vec::new(),
    };
    let empty_size = serde_json::to_vec(&envelope).ok()?.len();
    if empty_size > max_bytes {
        return None;
    }

    let mut candidates = entries.values().cloned().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .mtime_ms
            .cmp(&left.mtime_ms)
            .then_with(|| left.file.cmp(&right.file))
    });
    let mut entries_size = 0_usize;
    for entry in candidates {
        if envelope.entries.len() >= max_entries {
            break;
        }
        let entry_size = serde_json::to_vec(&entry).ok()?.len();
        let separator_size = usize::from(!envelope.entries.is_empty());
        let Some(projected_size) = empty_size
            .checked_add(entries_size)?
            .checked_add(separator_size)?
            .checked_add(entry_size)
        else {
            continue;
        };
        if projected_size > max_bytes {
            continue;
        }
        entries_size = entries_size
            .checked_add(separator_size)?
            .checked_add(entry_size)?;
        envelope.entries.push(entry);
    }

    let bytes = serde_json::to_vec(&envelope).ok()?;
    debug_assert!(bytes.len() <= max_bytes);
    Some(bytes)
}

pub(super) fn remove_missing_entries_under_root(
    entries: &mut BTreeMap<String, SessionIndexEntry>,
    sessions_dir: &Path,
    files: &[PathBuf],
) -> bool {
    let live_files = files
        .iter()
        .map(|file| cache_key(file))
        .collect::<HashSet<_>>();
    let original_len = entries.len();
    entries.retain(|key, _| !Path::new(key).starts_with(sessions_dir) || live_files.contains(key));
    entries.len() != original_len
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

    // Shared with every other test that mutates XDG_CACHE_HOME; a module-local
    // mutex would let these tests race the pricing/report cache tests.
    use crate::pricing_cache::XDG_CACHE_HOME_LOCK;

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

    fn entry(file: &str, mtime_ms: u64) -> SessionIndexEntry {
        SessionIndexEntry {
            file: file.to_string(),
            session_id: Path::new(file)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            size: 123,
            mtime_ms,
            events: Vec::new(),
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

    #[test]
    fn removes_missing_entries_only_under_the_scanned_root() {
        let root = Path::new("/sessions/primary");
        let live_file = PathBuf::from("/sessions/primary/live.jsonl");
        let mut entries = BTreeMap::from([
            (
                live_file.to_string_lossy().into_owned(),
                entry("/sessions/primary/live.jsonl", 3),
            ),
            (
                "/sessions/primary/deleted.jsonl".to_string(),
                entry("/sessions/primary/deleted.jsonl", 2),
            ),
            (
                "/sessions/secondary/keep.jsonl".to_string(),
                entry("/sessions/secondary/keep.jsonl", 1),
            ),
        ]);

        assert!(remove_missing_entries_under_root(
            &mut entries,
            root,
            std::slice::from_ref(&live_file),
        ));

        assert!(entries.contains_key("/sessions/primary/live.jsonl"));
        assert!(!entries.contains_key("/sessions/primary/deleted.jsonl"));
        assert!(entries.contains_key("/sessions/secondary/keep.jsonl"));
        assert!(!remove_missing_entries_under_root(
            &mut entries,
            root,
            std::slice::from_ref(&live_file),
        ));
    }

    #[test]
    fn serializes_only_newest_entries_within_count_and_byte_limits() {
        let updated_at = "2026-07-18T12:00:00.000Z";
        let entries = BTreeMap::from([
            (
                "/sessions/old.jsonl".to_string(),
                entry("/sessions/old.jsonl", 1),
            ),
            (
                "/sessions/middle.jsonl".to_string(),
                entry("/sessions/middle.jsonl", 2),
            ),
            (
                "/sessions/new.jsonl".to_string(),
                entry("/sessions/new.jsonl", 3),
            ),
        ]);

        let count_bounded =
            serialize_bounded_session_index(&entries, 2, usize::MAX, updated_at).unwrap();
        let count_envelope: SessionIndexEnvelope = serde_json::from_slice(&count_bounded).unwrap();
        assert_eq!(
            count_envelope
                .entries
                .iter()
                .map(|entry| entry.file.as_str())
                .collect::<Vec<_>>(),
            vec!["/sessions/new.jsonl", "/sessions/middle.jsonl"]
        );

        let newest_only = BTreeMap::from([(
            "/sessions/new.jsonl".to_string(),
            entry("/sessions/new.jsonl", 3),
        )]);
        let byte_limit =
            serialize_bounded_session_index(&newest_only, usize::MAX, usize::MAX, updated_at)
                .unwrap()
                .len();
        let byte_bounded =
            serialize_bounded_session_index(&entries, usize::MAX, byte_limit, updated_at).unwrap();
        let byte_envelope: SessionIndexEnvelope = serde_json::from_slice(&byte_bounded).unwrap();

        assert!(byte_bounded.len() <= byte_limit);
        assert_eq!(byte_envelope.entries.len(), 1);
        assert_eq!(byte_envelope.entries[0].file, "/sessions/new.jsonl");
    }
}
