use std::{collections::HashSet, env, path::PathBuf};

use crate::Result;

pub(super) const ZCODE_DATA_DIR_ENV: &str = "ZCODE_DATA_DIR";
const ZCODE_STORAGE_DIR_ENV: &str = "ZCODE_STORAGE_DIR";
const ZCODE_DB_RELATIVE_PATH: &str = "cli/db/db.sqlite";

#[cfg(test)]
pub(super) static ZCODE_DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn database_paths() -> Result<Vec<PathBuf>> {
    let roots = if let Ok(paths) = env::var(ZCODE_DATA_DIR_ENV) {
        paths
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    } else if let Some(path) = env::var(ZCODE_STORAGE_DIR_ENV)
        .ok()
        .filter(|path| !path.trim().is_empty())
    {
        vec![PathBuf::from(path)]
    } else {
        let home =
            crate::home::home_dir().ok_or_else(|| crate::cli_error("home directory is not set"))?;
        vec![home.join(".zcode")]
    };
    let mut seen = HashSet::new();
    Ok(roots
        .into_iter()
        .map(|root| root.join(ZCODE_DB_RELATIVE_PATH))
        .filter(|path| path.is_file())
        .filter(|path| seen.insert(path.clone()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::fs_fixture;

    #[test]
    fn finds_zcode_cli_database_in_configured_root() {
        let _guard = ZCODE_DATA_DIR_LOCK.lock().unwrap();
        let fixture = fs_fixture!({ "cli/db/db.sqlite": "" });
        env::set_var(ZCODE_DATA_DIR_ENV, fixture.root());

        let paths = database_paths().unwrap();

        env::remove_var(ZCODE_DATA_DIR_ENV);
        assert_eq!(paths, vec![fixture.path(ZCODE_DB_RELATIVE_PATH)]);
    }
}
