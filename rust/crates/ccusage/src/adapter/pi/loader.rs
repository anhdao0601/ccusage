use std::collections::HashSet;

use crate::{
    cli::SharedArgs, collect_files_with_extension, parse_tz, read_files_parallel, LoadedEntry,
    Result,
};

use super::{parser, paths};

pub(crate) fn load_entries(
    shared: &SharedArgs,
    custom_path: Option<&str>,
) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Pi, shared.json, || {
        load_entries_inner(shared, custom_path)
    })
}

fn load_entries_inner(shared: &SharedArgs, custom_path: Option<&str>) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for path in paths::paths(custom_path)? {
        let mut files = Vec::new();
        collect_files_with_extension(&path, "jsonl", &mut files);
        let loaded = read_files_parallel(&files, shared.single_thread, |file| {
            parser::read_session_file(file, tz.as_ref())
        });
        for file_entries in loaded {
            for entry in file_entries? {
                let id = parser::entry_id(&entry);
                if seen.insert(id) {
                    entries.push(entry);
                }
            }
        }
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::fs_fixture;

    #[test]
    fn loads_oh_my_pi_assistant_usage_from_session_files() {
        let fixture = fs_fixture!({
            ".omp/agent/sessions/-personal/2026-05-29T14-11-09-566Z_019e7413-1a7e-7000-b1ca-89447e702d4f.jsonl": [
                r#"{"type":"session","version":1,"id":"019e7413-1a7e-7000-b1ca-89447e702d4f","timestamp":"2026-05-29T14:11:09.566Z","cwd":"/personal"}"#,
                r#"{"type":"message","id":"assistant-1","parentId":"user-1","timestamp":"2026-05-29T14:12:00.000Z","message":{"role":"assistant","api":"openai-responses","provider":"zai","model":"glm-5.1","content":[{"type":"text","text":"Done"}],"timestamp":1780000000000,"stopReason":"stop","usage":{"input":123,"output":45,"cacheRead":678,"cacheWrite":9,"totalTokens":855,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.01}}}}"#,
            ].join("\n"),
        });
        let shared = SharedArgs {
            json: true,
            ..SharedArgs::default()
        };
        let sessions_path = fixture
            .path(".omp/agent/sessions")
            .to_string_lossy()
            .into_owned();

        let entries = load_entries(&shared, Some(sessions_path.as_str())).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.timestamp, "2026-05-29T14:12:00.000Z");
        assert_eq!(entries[0].model.as_deref(), Some("[pi] glm-5.1"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 123);
        assert_eq!(entries[0].data.message.usage.output_tokens, 45);
        assert_eq!(entries[0].data.message.usage.cache_creation_input_tokens, 9);
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 678);
        assert_eq!(entries[0].cost, 0.01);
        assert_eq!(entries[0].project.as_ref(), "-personal");
        assert_eq!(
            entries[0].session_id.as_ref(),
            "019e7413-1a7e-7000-b1ca-89447e702d4f"
        );
    }
}
