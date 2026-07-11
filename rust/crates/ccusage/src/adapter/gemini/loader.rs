use std::{path::PathBuf, thread};

use crate::{
    chunk_file_indexes_by_size, cli::SharedArgs, parse_tz, LoadedEntry, PricingMap, Result,
};

use super::{
    parser::{event_to_loaded, parse_json_file, parse_jsonl_file, GeminiUsageEvent},
    paths::discover_log_files,
};

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Gemini, shared.json, || {
        load_entries_inner(shared, pricing)
    })
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let files = discover_log_files()?;
    let mut events = Vec::new();
    for result in parse_log_files(&files, shared.single_thread) {
        events.extend(result?);
    }
    events.sort_by_key(|event| event.timestamp);
    Ok(events
        .into_iter()
        .map(|event| event_to_loaded(event, tz.as_ref(), shared.mode, pricing))
        .collect())
}

fn parse_log_file(file: &PathBuf) -> Result<Vec<GeminiUsageEvent>> {
    if file.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
        parse_jsonl_file(file)
    } else {
        parse_json_file(file)
    }
}

fn parse_log_files(files: &[PathBuf], single_thread: bool) -> Vec<Result<Vec<GeminiUsageEvent>>> {
    let worker_count = if single_thread {
        1
    } else {
        thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(files.len())
    };
    if worker_count <= 1 {
        return files.iter().map(parse_log_file).collect();
    }

    let chunks = chunk_file_indexes_by_size(files, worker_count);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for chunk in chunks {
            handles.push(scope.spawn(move || {
                chunk
                    .into_iter()
                    .map(|index| (index, parse_log_file(&files[index])))
                    .collect::<Vec<_>>()
            }));
        }
        let mut results = Vec::with_capacity(files.len());
        results.resize_with(files.len(), || None);
        for (index, result) in handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("gemini log worker panicked"))
        {
            results[index] = Some(result);
        }
        results
            .into_iter()
            .map(|result| result.expect("gemini log worker returned every file"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::fs_fixture;

    #[test]
    fn parallel_load_matches_single_thread() {
        let _guard = super::super::GEMINI_DATA_DIR_LOCK.lock().unwrap();
        let fixture = fs_fixture!({});
        for index in 0..8_u64 {
            let _ = fixture.write_file(
                format!("project-{index}/chats/session-{index}.jsonl"),
                [
                    format!(
                        r#"{{"sessionId":"session-{index}","projectHash":"project-{index}","startTime":"2026-05-17T11:07:00.000Z"}}"#
                    ),
                    format!(
                        r#"{{"id":"msg-{index}","timestamp":"2026-05-17T11:07:{:02}.000Z","type":"gemini","model":"gemini-3-flash-preview","tokens":{{"input":{},"output":{},"cached":100,"total":{}}}}}"#,
                        index + 1,
                        1_000 + index,
                        200 + index,
                        1_300 + 2 * index,
                    ),
                ]
                .join("\n"),
            );
        }
        let _env_guard = super::super::GeminiDataDirEnvGuard::set(fixture.root());
        let single_thread = SharedArgs {
            timezone: Some("UTC".to_string()),
            single_thread: true,
            ..SharedArgs::default()
        };
        let parallel = SharedArgs {
            single_thread: false,
            ..single_thread.clone()
        };
        let projected = |entries: &[LoadedEntry]| {
            entries
                .iter()
                .map(|entry| {
                    (
                        entry.data.message.id.clone(),
                        entry.session_id.to_string(),
                        entry.timestamp,
                        entry.data.message.usage.input_tokens,
                        entry.data.message.usage.output_tokens,
                        entry.cost.to_bits(),
                    )
                })
                .collect::<Vec<_>>()
        };

        let pricing = PricingMap::load_embedded();
        let single_thread_entries = load_entries(&single_thread, &pricing).unwrap();
        let parallel_entries = load_entries(&parallel, &pricing).unwrap();

        assert_eq!(parallel_entries.len(), 8);
        assert_eq!(
            projected(&parallel_entries),
            projected(&single_thread_entries)
        );
    }

    #[test]
    fn loads_jsonl_token_events_and_separates_cached_input() {
        let _guard = super::super::GEMINI_DATA_DIR_LOCK.lock().unwrap();
        let fixture = fs_fixture!({
            "project/chats/session-a.jsonl": [
                r#"{"sessionId":"session-a","projectHash":"project-a","startTime":"2026-05-17T11:07:00.000Z"}"#,
                r#"{"id":"msg-a","timestamp":"2026-05-17T11:07:32.000Z","type":"gemini","model":"gemini-3-flash-preview","tokens":{"input":15327,"output":23,"cached":11526,"thoughts":919,"tool":7,"total":16276}}"#,
            ]
            .join("\n"),
        });
        let _env_guard = super::super::GeminiDataDirEnvGuard::set(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-05-17");
        assert_eq!(entries[0].session_id.as_ref(), "session-a");
        assert_eq!(entries[0].model.as_deref(), Some("gemini-3-flash-preview"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 3_808);
        assert_eq!(entries[0].data.message.usage.output_tokens, 23);
        assert_eq!(
            entries[0].data.message.usage.cache_read_input_tokens,
            11_526
        );
        assert_eq!(entries[0].extra_total_tokens, 919);
    }
}
