use std::{env, path::PathBuf};

use crate::{cli_error, fast::FxHashSet, home, Result};

#[cfg(test)]
pub(crate) static CODEX_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sibling of `sessions/` where Codex files older sessions. The TS fork scans
/// this in addition to `sessions/` (DEFAULT_ARCHIVED_SESSION_SUBDIR); Rust must
/// too, or it silently drops the bulk of a user's Codex history.
const ARCHIVED_SESSIONS_SUBDIR: &str = "archived_sessions";

pub(super) fn codex_usage_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut seen = FxHashSet::default();
    for path in codex_home_paths()? {
        let sessions = path.join("sessions");
        let archived = path.join(ARCHIVED_SESSIONS_SUBDIR);
        let has_sessions = sessions.is_dir();
        let has_archived = archived.is_dir();
        if has_sessions && seen.insert(sessions.clone()) {
            paths.push(sessions);
        }
        if has_archived && seen.insert(archived.clone()) {
            paths.push(archived);
        }
        // Legacy/custom layouts where session files live directly under CODEX_HOME.
        if !has_sessions && !has_archived && seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub(super) fn codex_home_paths() -> Result<Vec<PathBuf>> {
    if let Ok(env_paths) = env::var("CODEX_HOME") {
        return Ok(env_paths
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect());
    }

    let home = home::home_dir().ok_or_else(|| cli_error("home directory is not set"))?;
    Ok(vec![home.join(".codex")])
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::fs_fixture;

    use super::*;

    #[test]
    fn discovers_both_sessions_and_archived_sessions_like_typescript() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let fixture = fs_fixture!({
            "sessions/2026/01/01/active.jsonl": "",
            "archived_sessions/2026/02/01/old.jsonl": "",
        });

        let previous = env::var("CODEX_HOME").ok();
        env::set_var("CODEX_HOME", fixture.root());
        let paths = codex_usage_paths().unwrap();
        if let Some(previous) = previous {
            env::set_var("CODEX_HOME", previous);
        } else {
            env::remove_var("CODEX_HOME");
        }

        let names = paths
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>();
        assert!(names.contains(&"sessions"));
        assert!(names.contains(&"archived_sessions"));
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn falls_back_to_codex_home_when_no_session_subdirs_exist() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let fixture = fs_fixture!({
            "rollout-direct.jsonl": "",
        });

        let previous = env::var("CODEX_HOME").ok();
        env::set_var("CODEX_HOME", fixture.root());
        let paths = codex_usage_paths().unwrap();
        if let Some(previous) = previous {
            env::set_var("CODEX_HOME", previous);
        } else {
            env::remove_var("CODEX_HOME");
        }

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], fixture.root());
    }
}
