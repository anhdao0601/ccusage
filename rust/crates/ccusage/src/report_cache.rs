use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::UNIX_EPOCH,
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    cli::{CostMode, SharedArgs},
    Result,
};

const CACHE_DIRECTORY_NAME: &str = "ccusage";
const REPORT_CACHE_SUBDIR: &str = "reports";
const REPORT_CACHE_SCHEMA_VERSION: u64 = 3;
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub(crate) struct ReportSource {
    id: String,
    path: PathBuf,
    matcher: SourceMatcher,
}

enum SourceMatcher {
    ExactFile,
    RecursiveExtensions(&'static [&'static str]),
    RecursiveFileNames(&'static [&'static str]),
    RecursiveFileNameSuffixes(&'static [&'static str]),
    OpenCodeDatabases,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportCacheEnvelope<T> {
    schema_version: u64,
    created_at: String,
    // Cached payloads must not outlive the build that computed them: report
    // shaping can change between builds while the cache key stays identical.
    #[serde(default)]
    binary: String,
    payload: T,
}

fn binary_fingerprint() -> String {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT
        .get_or_init(|| {
            env::current_exe()
                .and_then(fs::metadata)
                .map(|metadata| {
                    let mtime_ms = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |duration| duration.as_millis());
                    format!("{mtime_ms}:{}", metadata.len())
                })
                .unwrap_or_else(|_| "unknown".to_string())
        })
        .clone()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceSnapshot {
    id: String,
    path: String,
    exists: bool,
    file_count: u64,
    newest_mtime_ms: u64,
    total_size: u64,
    file_hash: u64,
}

pub(crate) fn with_report_cache<T>(
    command: &str,
    parameters: Value,
    sources: Vec<ReportSource>,
    shared: &SharedArgs,
    load: impl FnOnce() -> Result<T>,
) -> Result<T>
where
    T: DeserializeOwned + Serialize,
{
    if report_cache_path("probe").is_none() {
        return load();
    }
    let source_fingerprint = compute_source_fingerprint(&sources);
    let read_pricing = read_pricing_fingerprint(shared);
    if let Some(pricing) = read_pricing.as_ref() {
        let key = cache_key(command, &parameters, &source_fingerprint, pricing);
        if let Some(cached) = read_report_cache(&key) {
            return Ok(cached);
        }
    }

    let payload = load()?;
    let pricing = write_pricing_fingerprint(shared);
    let key = cache_key(command, &parameters, &source_fingerprint, &pricing);
    write_report_cache(&key, &payload);
    Ok(payload)
}

pub(crate) fn agent_report_sources(agent: &str) -> Vec<ReportSource> {
    match agent {
        "claude" => claude_sources(),
        "ncode" => ncode_sources(),
        "codex" => codex_sources(),
        "opencode" => opencode_sources(),
        "amp" => amp_sources(),
        "droid" => droid_sources(),
        "codebuff" => codebuff_sources(),
        "hermes" => hermes_sources(),
        "pi" => pi_sources(None),
        "goose" => goose_sources(),
        "openclaw" => openclaw_sources(None),
        "kilo" => kilo_sources(),
        "copilot" => copilot_sources(),
        "gemini" => gemini_sources(),
        "kimi" => kimi_sources(),
        "qwen" => qwen_sources(),
        "zcode" => zcode_sources(),
        _ => Vec::new(),
    }
}

pub(crate) fn opencode_report_sources() -> Vec<ReportSource> {
    opencode_sources()
}

pub(crate) fn report_parameters(
    command: &str,
    kind: impl std::fmt::Debug,
    shared: &SharedArgs,
) -> Value {
    json!({
        "command": command,
        "kind": format!("{kind:?}"),
        "since": shared.since,
        "until": shared.until,
        "mode": format!("{:?}", shared.mode),
        "order": format!("{:?}", shared.order),
        "offline": shared.offline,
        "timezone": shared.timezone,
        "toolFilter": shared.tool_filter,
        "modelFilter": shared.model_filter,
        "byModel": shared.by_model,
        "byProvider": shared.by_provider,
        "singleThread": shared.single_thread,
    })
}

fn cache_key(command: &str, parameters: &Value, source_fingerprint: &str, pricing: &str) -> String {
    let value = json!({
        "schemaVersion": REPORT_CACHE_SCHEMA_VERSION,
        "command": command,
        "parameters": parameters,
        "sourceFingerprint": source_fingerprint,
        "pricing": pricing,
        "modelAliases": crate::model_aliases::cache_fingerprint(),
    });
    format!("{:016x}", hash_bytes(value.to_string().as_bytes()))
}

fn read_report_cache<T: DeserializeOwned>(key: &str) -> Option<T> {
    let bytes = fs::read(report_cache_path(key)?).ok()?;
    let envelope = serde_json::from_slice::<ReportCacheEnvelope<T>>(&bytes).ok()?;
    (envelope.schema_version == REPORT_CACHE_SCHEMA_VERSION
        && envelope.binary == binary_fingerprint())
    .then_some(envelope.payload)
}

fn write_report_cache<T: Serialize>(key: &str, payload: &T) {
    let Some(path) = report_cache_path(key) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let envelope = ReportCacheEnvelope {
        schema_version: REPORT_CACHE_SCHEMA_VERSION,
        created_at: crate::format_rfc3339_millis(crate::utc_now()),
        binary: binary_fingerprint(),
        payload,
    };
    let Ok(bytes) = serde_json::to_vec(&envelope) else {
        return;
    };
    // Unique per write: concurrent same-key writers (per-agent loader threads,
    // parallel tests) must never share a temp file, or the atomic rename can
    // publish a half-written entry.
    static WRITE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = path.with_extension(format!("json.{}.{sequence}.tmp", std::process::id()));
    if fs::write(&temp_path, bytes).is_ok() {
        let _ = fs::rename(&temp_path, path);
    }
    let _ = fs::remove_file(temp_path);
}

fn report_cache_path(key: &str) -> Option<PathBuf> {
    let cache_home = match env::var_os("XDG_CACHE_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ if cfg!(test) => return None,
        _ => crate::home::home_dir()?.join(".cache"),
    };
    Some(
        cache_home
            .join(CACHE_DIRECTORY_NAME)
            .join(REPORT_CACHE_SUBDIR)
            .join(format!("{key}.json")),
    )
}

fn read_pricing_fingerprint(shared: &SharedArgs) -> Option<String> {
    // --update-pricing forces a pricing refresh (and thus a new report-cache
    // key on write), so never serve a stale cached report on the read path.
    if shared.update_pricing {
        return None;
    }
    if shared.mode == CostMode::Display {
        return Some("pricing:none".to_string());
    }
    if shared.offline {
        return Some(crate::pricing::embedded_pricing_fingerprint());
    }
    crate::pricing_cache::fresh_pricing_cache_fetched_at()
        .map(|fetched_at| format!("pricing:online:{fetched_at}"))
}

fn write_pricing_fingerprint(shared: &SharedArgs) -> String {
    read_pricing_fingerprint(shared).unwrap_or_else(|| "pricing:online:none".to_string())
}

fn compute_source_fingerprint(sources: &[ReportSource]) -> String {
    let mut snapshots = sources.iter().map(snapshot_source).collect::<Vec<_>>();
    snapshots.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.path.cmp(&right.path))
    });
    format!(
        "{:016x}",
        hash_bytes(
            serde_json::to_string(&snapshots)
                .unwrap_or_default()
                .as_bytes()
        )
    )
}

fn snapshot_source(source: &ReportSource) -> SourceSnapshot {
    let mut snapshot = SourceSnapshot {
        id: source.id.clone(),
        path: source.path.to_string_lossy().into_owned(),
        exists: source.path.exists(),
        file_count: 0,
        newest_mtime_ms: 0,
        total_size: 0,
        file_hash: FNV_OFFSET,
    };
    match source.matcher {
        SourceMatcher::ExactFile => {
            snapshot_file(&source.path, &source.path, &mut snapshot);
        }
        SourceMatcher::RecursiveExtensions(extensions) => {
            snapshot_recursive(&source.path, &mut snapshot, &|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| extensions.contains(&ext))
            });
        }
        SourceMatcher::RecursiveFileNames(names) => {
            snapshot_recursive(&source.path, &mut snapshot, &|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| names.contains(&name))
            });
        }
        SourceMatcher::RecursiveFileNameSuffixes(suffixes) => {
            snapshot_recursive(&source.path, &mut snapshot, &|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| suffixes.iter().any(|suffix| name.ends_with(suffix)))
            });
        }
        SourceMatcher::OpenCodeDatabases => {
            snapshot_recursive(&source.path, &mut snapshot, &|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_opencode_database)
            });
        }
    }
    snapshot
}

fn snapshot_recursive(
    root: &Path,
    snapshot: &mut SourceSnapshot,
    matches: &impl Fn(&Path) -> bool,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            snapshot_recursive(&path, snapshot, matches);
        } else if file_type.is_file() && matches(&path) {
            snapshot_file(root, &path, snapshot);
        }
    }
}

fn snapshot_file(root: &Path, path: &Path, snapshot: &mut SourceSnapshot) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    snapshot.file_count += 1;
    snapshot.newest_mtime_ms = snapshot.newest_mtime_ms.max(mtime_ms);
    snapshot.total_size = snapshot.total_size.wrapping_add(metadata.len());
    snapshot.file_hash = hash_combine(snapshot.file_hash, relative.as_bytes());
    snapshot.file_hash = hash_combine(snapshot.file_hash, &metadata.len().to_le_bytes());
    snapshot.file_hash = hash_combine(snapshot.file_hash, &mtime_ms.to_le_bytes());
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    hash_combine(FNV_OFFSET, bytes)
}

fn hash_combine(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn is_opencode_database(name: &str) -> bool {
    name == "opencode.db"
        || (name.starts_with("opencode-")
            && name.ends_with(".db")
            && name["opencode-".len()..name.len() - ".db".len()]
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
}

fn claude_sources() -> Vec<ReportSource> {
    let dirs = if let Ok(raw) = env::var("CLAUDE_CONFIG_DIR") {
        env_paths(&raw)
            .into_iter()
            .map(|path| {
                if path.file_name().is_some_and(|name| name == "projects") {
                    path
                } else {
                    path.join("projects")
                }
            })
            .collect()
    } else {
        let Some(home) = crate::home::home_dir() else {
            return Vec::new();
        };
        let xdg = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        vec![xdg.join("claude/projects"), home.join(".claude/projects")]
    };
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| recursive_extensions(format!("claude:{index}"), path, &["jsonl"]))
        .collect()
}

fn ncode_sources() -> Vec<ReportSource> {
    let dirs = if let Ok(raw) = env::var("NCODE_CONFIG_DIR") {
        env_paths(&raw)
            .into_iter()
            .map(|path| {
                if path.file_name().is_some_and(|name| name == "projects") {
                    path
                } else {
                    path.join("projects")
                }
            })
            .collect()
    } else {
        let Some(home) = crate::home::home_dir() else {
            return Vec::new();
        };
        vec![home.join(".ncode/projects")]
    };
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| recursive_extensions(format!("ncode:{index}"), path, &["jsonl"]))
        .collect()
}

fn codex_sources() -> Vec<ReportSource> {
    let homes = env::var("CODEX_HOME")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".codex")]))
        .unwrap_or_default();
    homes
        .into_iter()
        .enumerate()
        .flat_map(|(index, path)| {
            [
                recursive_extensions(
                    format!("codex:{index}:sessions"),
                    path.join("sessions"),
                    &["jsonl"],
                ),
                recursive_extensions(format!("codex:{index}:root"), path, &["jsonl"]),
            ]
        })
        .collect()
}

fn opencode_sources() -> Vec<ReportSource> {
    let dirs = env::var("OPENCODE_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".local/share/opencode")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .flat_map(|(index, path)| {
            [
                recursive_extensions(
                    format!("opencode:{index}:messages"),
                    path.join("storage/message"),
                    &["json"],
                ),
                ReportSource {
                    id: format!("opencode:{index}:db"),
                    path,
                    matcher: SourceMatcher::OpenCodeDatabases,
                },
            ]
        })
        .collect()
}

fn amp_sources() -> Vec<ReportSource> {
    let dirs = env::var("AMP_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".local/share/amp")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| {
            recursive_extensions(format!("amp:{index}"), path.join("threads"), &["json"])
        })
        .collect()
}

fn droid_sources() -> Vec<ReportSource> {
    let dirs = env::var("DROID_SESSIONS_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".factory/sessions")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| {
            recursive_suffixes(format!("droid:{index}"), path, &[".settings.json"])
        })
        .collect()
}

fn codebuff_sources() -> Vec<ReportSource> {
    let dirs = env::var("CODEBUFF_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| {
            crate::home::home_dir().map(|home| {
                ["manicode", "manicode-dev", "manicode-staging"]
                    .into_iter()
                    .map(|channel| home.join(".config").join(channel))
                    .collect()
            })
        })
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| {
            let projects = if path.file_name().is_some_and(|name| name == "projects") {
                path
            } else {
                path.join("projects")
            };
            recursive_names(
                format!("codebuff:{index}"),
                projects,
                &["chat-messages.json"],
            )
        })
        .collect()
}

fn hermes_sources() -> Vec<ReportSource> {
    let dirs = env::var("HERMES_HOME")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".hermes")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .flat_map(|(index, path)| sqlite_sources(format!("hermes:{index}"), path.join("state.db")))
        .collect()
}

fn pi_sources(custom_path: Option<&str>) -> Vec<ReportSource> {
    let dirs = custom_path
        .filter(|path| !path.trim().is_empty())
        .map(env_paths)
        .or_else(|| env::var("PI_AGENT_DIR").ok().map(|raw| env_paths(&raw)))
        .or_else(|| {
            crate::home::home_dir().map(|home| {
                let mut dirs = Vec::new();
                if let Ok(agent_dir) = env::var("PI_CODING_AGENT_DIR") {
                    if !agent_dir.trim().is_empty() {
                        dirs.push(PathBuf::from(agent_dir).join("sessions"));
                    }
                } else {
                    let config_dir = env::var("PI_CONFIG_DIR")
                        .ok()
                        .filter(|path| !path.trim().is_empty())
                        .unwrap_or_else(|| ".omp".to_string());
                    dirs.push(home.join(config_dir).join("agent/sessions"));
                    if let Ok(xdg_data_home) = env::var("XDG_DATA_HOME") {
                        if !xdg_data_home.trim().is_empty() {
                            dirs.push(PathBuf::from(xdg_data_home).join("omp/sessions"));
                        }
                    }
                }
                dirs.push(home.join(".pi/agent/sessions"));
                dirs
            })
        })
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| recursive_extensions(format!("pi:{index}"), path, &["jsonl"]))
        .collect()
}

fn goose_sources() -> Vec<ReportSource> {
    let paths = if let Ok(root) = env::var("GOOSE_PATH_ROOT") {
        if root.trim().is_empty() {
            default_goose_sources()
        } else {
            vec![PathBuf::from(root).join("data/sessions/sessions.db")]
        }
    } else {
        default_goose_sources()
    };
    paths
        .into_iter()
        .enumerate()
        .flat_map(|(index, path)| sqlite_sources(format!("goose:{index}"), path))
        .collect()
}

fn default_goose_sources() -> Vec<PathBuf> {
    let Some(home) = crate::home::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".local/share/goose/sessions/sessions.db"),
        home.join("Library/Application Support/goose/sessions/sessions.db"),
        home.join(".local/share/Block/goose/sessions/sessions.db"),
    ]
}

fn openclaw_sources(custom_path: Option<&str>) -> Vec<ReportSource> {
    let dirs = custom_path
        .filter(|path| !path.trim().is_empty())
        .map(env_paths)
        .or_else(|| env::var("OPENCLAW_DIR").ok().map(|raw| env_paths(&raw)))
        .or_else(|| {
            crate::home::home_dir().map(|home| {
                [".openclaw", ".clawdbot", ".moltbot", ".moldbot"]
                    .into_iter()
                    .map(|name| home.join(name))
                    .collect()
            })
        })
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| recursive_suffixes(format!("openclaw:{index}"), path, &[".jsonl"]))
        .collect()
}

fn kilo_sources() -> Vec<ReportSource> {
    let dirs = env::var("KILO_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".local/share/kilo")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .flat_map(|(index, path)| sqlite_sources(format!("kilo:{index}"), path.join("kilo.db")))
        .collect()
}

fn copilot_sources() -> Vec<ReportSource> {
    let mut sources = crate::home::home_dir()
        .map(|home| {
            vec![recursive_extensions(
                "copilot:default".to_string(),
                home.join(".copilot/otel"),
                &["jsonl"],
            )]
        })
        .unwrap_or_default();
    if let Ok(path) = env::var("COPILOT_OTEL_FILE_EXPORTER_PATH") {
        if !path.trim().is_empty() {
            sources.push(exact_file(
                "copilot:exporter".to_string(),
                PathBuf::from(path),
            ));
        }
    }
    sources
}

fn gemini_sources() -> Vec<ReportSource> {
    let dirs = env::var("GEMINI_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".gemini/tmp")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| {
            recursive_extensions(format!("gemini:{index}"), path, &["json", "jsonl"])
        })
        .collect()
}

fn kimi_sources() -> Vec<ReportSource> {
    let dirs = env::var("KIMI_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".kimi")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| {
            recursive_names(
                format!("kimi:{index}"),
                path.join("sessions"),
                &["wire.jsonl"],
            )
        })
        .collect()
}

fn qwen_sources() -> Vec<ReportSource> {
    let dirs = env::var("QWEN_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".qwen")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .map(|(index, path)| {
            recursive_extensions(format!("qwen:{index}"), path.join("projects"), &["jsonl"])
        })
        .collect()
}

fn zcode_sources() -> Vec<ReportSource> {
    let dirs = env::var("ZCODE_DATA_DIR")
        .ok()
        .map(|raw| env_paths(&raw))
        .or_else(|| {
            env::var("ZCODE_STORAGE_DIR")
                .ok()
                .filter(|path| !path.trim().is_empty())
                .map(|path| vec![PathBuf::from(path)])
        })
        .or_else(|| crate::home::home_dir().map(|home| vec![home.join(".zcode")]))
        .unwrap_or_default();
    dirs.into_iter()
        .enumerate()
        .flat_map(|(index, path)| {
            sqlite_sources(format!("zcode:{index}"), path.join("cli/db/db.sqlite"))
        })
        .collect()
}

fn env_paths(raw: &str) -> Vec<PathBuf> {
    raw.split(',')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn exact_file(id: String, path: PathBuf) -> ReportSource {
    ReportSource {
        id,
        path,
        matcher: SourceMatcher::ExactFile,
    }
}

fn sqlite_sources(id: String, database: PathBuf) -> [ReportSource; 2] {
    let mut wal_name = database.file_name().unwrap_or_default().to_os_string();
    wal_name.push("-wal");
    let wal = database.with_file_name(wal_name);
    [
        exact_file(format!("{id}:database"), database),
        exact_file(format!("{id}:wal"), wal),
    ]
}

pub(crate) fn recursive_extensions(
    id: String,
    path: PathBuf,
    extensions: &'static [&'static str],
) -> ReportSource {
    ReportSource {
        id,
        path,
        matcher: SourceMatcher::RecursiveExtensions(extensions),
    }
}

fn recursive_names(id: String, path: PathBuf, names: &'static [&'static str]) -> ReportSource {
    ReportSource {
        id,
        path,
        matcher: SourceMatcher::RecursiveFileNames(names),
    }
}

fn recursive_suffixes(
    id: String,
    path: PathBuf,
    suffixes: &'static [&'static str],
) -> ReportSource {
    ReportSource {
        id,
        path,
        matcher: SourceMatcher::RecursiveFileNameSuffixes(suffixes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::fs_fixture;

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
    fn ignores_cached_payload_written_by_different_binary() {
        let _aliases = crate::model_aliases::set_model_aliases_for_tests([]);
        let _guard = crate::pricing_cache::XDG_CACHE_HOME_LOCK.lock().unwrap();
        let fixture = fs_fixture!({
            "source/usage.jsonl": "{}\n",
        });
        let _env = EnvRestore::set_path("XDG_CACHE_HOME", &fixture.path("cache"));
        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let sources = || {
            vec![recursive_extensions(
                "test".to_string(),
                fixture.path("source"),
                &["jsonl"],
            )]
        };
        let mut loads = 0;

        let first: Value =
            with_report_cache("daily", json!({"tool":"test"}), sources(), &shared, || {
                loads += 1;
                Ok(json!({"loads": loads}))
            })
            .unwrap();
        assert_eq!(first, json!({"loads": 1}));

        // Simulate a cache entry left behind by a previous build of ccusage.
        let reports_dir = fixture.path("cache").join("ccusage").join("reports");
        for entry in fs::read_dir(&reports_dir).unwrap() {
            let path = entry.unwrap().path();
            let mut envelope: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            envelope["binary"] = json!("stale-build-fingerprint");
            fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        }

        let second: Value =
            with_report_cache("daily", json!({"tool":"test"}), sources(), &shared, || {
                loads += 1;
                Ok(json!({"loads": loads}))
            })
            .unwrap();

        assert_eq!(second, json!({"loads": 2}));
        assert_eq!(loads, 2);
    }

    #[test]
    fn reuses_cached_payload_when_sources_are_unchanged() {
        let _aliases = crate::model_aliases::set_model_aliases_for_tests([]);
        let _guard = crate::pricing_cache::XDG_CACHE_HOME_LOCK.lock().unwrap();
        let fixture = fs_fixture!({
            "source/usage.jsonl": "{}\n",
        });
        let _env = EnvRestore::set_path("XDG_CACHE_HOME", &fixture.path("cache"));
        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let sources = || {
            vec![recursive_extensions(
                "test".to_string(),
                fixture.path("source"),
                &["jsonl"],
            )]
        };
        let mut loads = 0;

        let first: Value =
            with_report_cache("daily", json!({"tool":"test"}), sources(), &shared, || {
                loads += 1;
                Ok(json!({"loads": loads}))
            })
            .unwrap();
        let second: Value =
            with_report_cache("daily", json!({"tool":"test"}), sources(), &shared, || {
                loads += 1;
                Ok(json!({"loads": loads}))
            })
            .unwrap();

        assert_eq!(first, json!({"loads": 1}));
        assert_eq!(second, json!({"loads": 1}));
        assert_eq!(loads, 1);
    }

    #[test]
    fn invalidates_cached_payload_when_sources_change() {
        let _aliases = crate::model_aliases::set_model_aliases_for_tests([]);
        let _guard = crate::pricing_cache::XDG_CACHE_HOME_LOCK.lock().unwrap();
        let fixture = fs_fixture!({
            "source/usage.jsonl": "{}\n",
        });
        let _env = EnvRestore::set_path("XDG_CACHE_HOME", &fixture.path("cache"));
        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let sources = || {
            vec![recursive_extensions(
                "test".to_string(),
                fixture.path("source"),
                &["jsonl"],
            )]
        };
        let mut loads = 0;

        let _: Value = with_report_cache("daily", json!({}), sources(), &shared, || {
            loads += 1;
            Ok(json!({"loads": loads}))
        })
        .unwrap();
        fs::write(fixture.path("source/another.jsonl"), "{}\n").unwrap();
        let second: Value = with_report_cache("daily", json!({}), sources(), &shared, || {
            loads += 1;
            Ok(json!({"loads": loads}))
        })
        .unwrap();

        assert_eq!(second, json!({"loads": 2}));
        assert_eq!(loads, 2);
    }

    #[test]
    fn sqlite_source_fingerprint_changes_when_wal_changes() {
        let fixture = fs_fixture!({
            "database/state.db": "database",
            "database/state.db-wal": "wal",
        });
        let sources = sqlite_sources("test".to_string(), fixture.path("database/state.db"));
        let before = compute_source_fingerprint(&sources);

        fs::write(fixture.path("database/state.db-wal"), "wal grew").unwrap();
        let after = compute_source_fingerprint(&sources);

        assert_ne!(before, after);
    }

    #[test]
    fn offline_cache_key_uses_embedded_pricing_fingerprint() {
        let shared = SharedArgs {
            offline: true,
            ..SharedArgs::default()
        };

        let pricing = read_pricing_fingerprint(&shared);

        assert_eq!(
            pricing,
            Some(crate::pricing::embedded_pricing_fingerprint())
        );
        assert_ne!(pricing, Some("pricing:offline".to_string()));
    }

    #[test]
    fn report_parameters_include_model_filter() {
        let shared = SharedArgs {
            model_filter: Some(vec!["opus".to_string()]),
            ..SharedArgs::default()
        };

        let parameters = report_parameters("all", "monthly", &shared);

        assert_eq!(parameters["modelFilter"], json!(["opus"]));
    }

    #[test]
    fn cache_key_changes_with_model_aliases() {
        let without_aliases = {
            let _aliases = crate::model_aliases::set_model_aliases_for_tests([]);
            cache_key("daily", &json!({}), "sources", "pricing")
        };
        let with_aliases = {
            let _aliases = crate::model_aliases::set_model_aliases_for_tests([(
                "private-model",
                "canonical-model",
            )]);
            cache_key("daily", &json!({}), "sources", "pricing")
        };

        assert_ne!(without_aliases, with_aliases);
    }

    #[test]
    fn canonical_model_reports_use_current_cache_schema() {
        assert_eq!(REPORT_CACHE_SCHEMA_VERSION, 3);
    }
}
