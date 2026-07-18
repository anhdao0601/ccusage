use std::path::{Path, PathBuf};

#[cfg(not(test))]
use crate::chunk_file_indexes_by_size;
use crate::{cli::SharedArgs, collect_usage_files, progress, CodexTokenUsageEvent, Result};
#[cfg(not(test))]
use std::thread;

#[cfg(not(test))]
use super::session_index::{self, SessionIndexEntry};
use super::{parser::visit_codex_session_file, paths::codex_usage_paths};

pub(crate) fn load_codex_events_from_directory(
    sessions_dir: &Path,
    single_thread: bool,
) -> Result<Vec<CodexTokenUsageEvent>> {
    let mut files = Vec::new();
    collect_usage_files(sessions_dir, &mut files);
    files.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
    Ok(read_codex_session_files_with_index(
        sessions_dir,
        &files,
        single_thread,
    ))
}

pub(crate) fn load_codex_events(shared: &SharedArgs) -> Result<Vec<CodexTokenUsageEvent>> {
    progress::track_usage_load(progress::UsageLoadAgent::Codex, shared.json, || {
        load_codex_events_inner(shared)
    })
}

fn load_codex_events_inner(shared: &SharedArgs) -> Result<Vec<CodexTokenUsageEvent>> {
    let mut events = Vec::new();
    for path in codex_usage_paths()? {
        events.extend(load_codex_events_from_directory(
            &path,
            shared.single_thread,
        )?);
    }
    Ok(events)
}

fn read_codex_session_files_with_index(
    sessions_dir: &Path,
    files: &[PathBuf],
    single_thread: bool,
) -> Vec<CodexTokenUsageEvent> {
    #[cfg(test)]
    {
        let _ = single_thread;
        files
            .iter()
            .flat_map(|file| read_codex_session_file(sessions_dir, file))
            .collect()
    }

    #[cfg(not(test))]
    {
        let mut index = session_index::read_session_index();
        let mut index_changed =
            session_index::remove_missing_entries_under_root(&mut index, sessions_dir, files);
        let mut loaded_files = Vec::new();
        loaded_files.resize_with(files.len(), || None);
        let mut changed_files = Vec::new();

        for (file_index, file) in files.iter().enumerate() {
            let key = session_index::cache_key(file);
            let Some((size, mtime_ms)) = session_index::file_state(file) else {
                continue;
            };
            if let Some(entry) = index.get(&key) {
                if entry.size == size && entry.mtime_ms == mtime_ms {
                    loaded_files[file_index] = Some(entry.events.clone());
                    continue;
                }
            }
            changed_files.push((file_index, file.clone(), key, size, mtime_ms));
        }

        let parsed_files = if single_thread {
            changed_files
                .iter()
                .map(|(file_index, file, key, size, mtime_ms)| {
                    (
                        *file_index,
                        key.clone(),
                        *size,
                        *mtime_ms,
                        read_codex_session_file(sessions_dir, file),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            read_changed_codex_session_files_parallel(sessions_dir, &changed_files)
        };

        for (file_index, key, size, mtime_ms, events) in parsed_files {
            loaded_files[file_index] = Some(events.clone());
            index.insert(
                key.clone(),
                SessionIndexEntry {
                    file: key,
                    session_id: session_index::session_id_from_path(&files[file_index]),
                    size,
                    mtime_ms,
                    events,
                },
            );
            index_changed = true;
        }
        if index_changed {
            session_index::write_session_index(&index);
        }

        loaded_files
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>()
    }
}

#[cfg(not(test))]
fn read_changed_codex_session_files_parallel(
    sessions_dir: &Path,
    files: &[(usize, PathBuf, String, u64, u64)],
) -> Vec<(usize, String, u64, u64, Vec<CodexTokenUsageEvent>)> {
    let worker_count = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(files.len());
    if worker_count <= 1 {
        return files
            .iter()
            .map(|(file_index, file, key, size, mtime_ms)| {
                (
                    *file_index,
                    key.clone(),
                    *size,
                    *mtime_ms,
                    read_codex_session_file(sessions_dir, file),
                )
            })
            .collect();
    }

    let paths = files
        .iter()
        .map(|(_, path, _, _, _)| path.clone())
        .collect::<Vec<_>>();
    let chunks = chunk_file_indexes_by_size(&paths, worker_count);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            handles.push(scope.spawn(move || {
                chunk
                    .into_iter()
                    .map(|index| {
                        let (file_index, file, key, size, mtime_ms) = &files[index];
                        (
                            *file_index,
                            key.clone(),
                            *size,
                            *mtime_ms,
                            read_codex_session_file(sessions_dir, file),
                        )
                    })
                    .collect::<Vec<_>>()
            }));
        }

        let mut loaded = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("codex worker panicked"))
            .collect::<Vec<_>>();
        loaded.sort_by_key(|(file_index, _, _, _, _)| *file_index);
        loaded
    })
}

fn read_codex_session_file(sessions_dir: &Path, path: &Path) -> Vec<CodexTokenUsageEvent> {
    let mut events = Vec::new();
    let _ = visit_codex_session_file(sessions_dir, path, |event| {
        events.push(event);
        Ok(())
    });
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    use ccusage_test_support::fs_fixture;
    use serde_json::json;

    #[test]
    fn loads_copied_branch_history_for_report_aware_dedupe() {
        let parent_history = [
            json!({
                "timestamp": "2026-05-12T08:00:00.000Z",
                "type": "turn_context",
                "payload": {
                    "model": "gpt-5.2",
                },
            })
            .to_string(),
            json!({
                "timestamp": "2026-05-12T08:01:00.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1_000,
                            "cached_input_tokens": 100,
                            "output_tokens": 200,
                            "reasoning_output_tokens": 20,
                            "total_tokens": 1_200,
                        },
                    },
                },
            })
            .to_string(),
        ]
        .join("\n");
        let branch_history = [
            parent_history.as_str(),
            &json!({
                "timestamp": "2026-05-12T08:02:00.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1_600,
                            "cached_input_tokens": 300,
                            "output_tokens": 450,
                            "reasoning_output_tokens": 40,
                            "total_tokens": 2_050,
                        },
                    },
                },
            })
            .to_string(),
        ]
        .join("\n");
        let fixture = fs_fixture!({
            "2026-05-12T08-00-00-parent.jsonl": &parent_history,
            "2026-05-12T08-02-00-branch.jsonl": branch_history,
        });

        for single_thread in [true, false] {
            let events = load_codex_events_from_directory(fixture.root(), single_thread).unwrap();

            assert_eq!(events.len(), 3);
            assert_eq!(events[0].session_id, "2026-05-12T08-00-00-parent");
            assert_eq!(events[0].input_tokens, 1_000);
            assert_eq!(events[0].cached_input_tokens, 100);
            assert_eq!(events[0].output_tokens, 200);
            assert_eq!(events[0].reasoning_output_tokens, 20);
            assert_eq!(events[0].total_tokens, 1_200);
            assert_eq!(events[1].session_id, "2026-05-12T08-02-00-branch");
            assert_eq!(events[1].input_tokens, 1_000);
            assert_eq!(events[2].session_id, "2026-05-12T08-02-00-branch");
            assert_eq!(events[2].input_tokens, 600);
            assert_eq!(events[2].cached_input_tokens, 200);
            assert_eq!(events[2].output_tokens, 250);
            assert_eq!(events[2].reasoning_output_tokens, 20);
            assert_eq!(events[2].total_tokens, 850);

            let groups = crate::adapter::codex::aggregate_events(
                &events,
                crate::cli::AgentReportKind::Daily,
                Some("UTC"),
            )
            .unwrap();
            let group = groups.get("2026-05-12").unwrap();
            assert_eq!(group.input_tokens, 1_600);
            assert_eq!(group.cached_input_tokens, 300);
            assert_eq!(group.output_tokens, 450);
            assert_eq!(group.reasoning_output_tokens, 40);
            assert_eq!(group.total_tokens, 2_050);
        }
    }

    #[test]
    fn loads_saved_codex_exec_json_usage() {
        let fixture = fs_fixture!({
            "run.jsonl": [
                json!({
                    "type": "turn.completed",
                    "timestamp": "2026-01-02T03:04:05.000Z",
                    "model": "gpt-5.2-codex",
                    "usage": {
                        "input_tokens": 120,
                        "cached_input_tokens": 20,
                        "output_tokens": 30,
                        "total_tokens": 150,
                    },
                })
                .to_string(),
                json!({
                    "type": "result",
                    "data": {
                        "timestamp": "2026-01-02T03:05:05.000Z",
                        "model_name": "gpt-5.2-codex",
                        "usage": {
                            "prompt_tokens": 50,
                            "cached_tokens": 5,
                            "completion_tokens": 12,
                        },
                    },
                })
                .to_string(),
                json!({
                    "type": "turn.completed",
                    "timestamp": "2026-01-02T03:06:05.000Z",
                    "model": "gpt-5.2-codex",
                    "usage": {
                        "input_tokens": 9,
                        "output_tokens": 4,
                        "reasoning_output_tokens": 1,
                        "total_tokens": 0,
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].session_id, "run");
        assert_eq!(events[0].timestamp, "2026-01-02T03:04:05.000Z");
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(events[0].input_tokens, 120);
        assert_eq!(events[0].cached_input_tokens, 20);
        assert_eq!(events[0].output_tokens, 30);
        assert_eq!(events[0].total_tokens, 150);
        assert_eq!(events[1].timestamp, "2026-01-02T03:05:05.000Z");
        assert_eq!(events[1].model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(events[1].input_tokens, 50);
        assert_eq!(events[1].cached_input_tokens, 5);
        assert_eq!(events[1].output_tokens, 12);
        assert_eq!(events[1].total_tokens, 62);
        assert_eq!(events[2].timestamp, "2026-01-02T03:06:05.000Z");
        assert_eq!(events[2].input_tokens, 9);
        assert_eq!(events[2].output_tokens, 4);
        assert_eq!(events[2].reasoning_output_tokens, 1);
        assert_eq!(events[2].total_tokens, 13);
    }

    #[test]
    fn loads_codex_cache_write_tokens() {
        let fixture = fs_fixture!({
            "session.jsonl": [
                json!({
                    "timestamp": "2026-07-09T00:00:00.000Z",
                    "type": "turn_context",
                    "payload": {
                        "model": "gpt-5.6-sol",
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-07-09T00:00:01.000Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 120,
                                "cached_input_tokens": 20,
                                "cache_write_tokens": 60,
                                "output_tokens": 30,
                                "total_tokens": 150,
                            },
                        },
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(events[0].cache_write_tokens, 60);
    }

    #[test]
    fn loads_session_usage_with_numeric_timestamp() {
        let fixture = fs_fixture!({
            "session.jsonl": [
                json!({
                    "timestamp": "2026-01-02T00:00:00.000Z",
                    "type": "turn_context",
                    "payload": {
                        "model": "gpt-5",
                    },
                })
                .to_string(),
                json!({
                    "timestamp": 1767312001000_u64,
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 100,
                                "cached_input_tokens": 10,
                                "output_tokens": 50,
                                "reasoning_output_tokens": 0,
                                "total_tokens": 150,
                            },
                            "model": "gpt-5",
                        },
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "session");
        assert_eq!(events[0].timestamp, "2026-01-02T00:00:01.000Z");
        assert_eq!(events[0].model.as_deref(), Some("gpt-5"));
        assert_eq!(events[0].input_tokens, 100);
        assert_eq!(events[0].cached_input_tokens, 10);
        assert_eq!(events[0].output_tokens, 50);
        assert_eq!(events[0].total_tokens, 150);
    }

    #[test]
    fn loads_session_usage_with_spaced_type_fields() {
        let fixture = fs_fixture!({
            "session.jsonl": [
                r#"{ "timestamp": "2026-01-02T00:00:00.000Z", "type" : "turn_context", "payload": { "model": "gpt-5" } }"#,
                r#"{ "timestamp": "2026-01-02T00:00:01.000Z", "type" : "event_msg", "payload": { "type" : "token_count", "info": { "total_token_usage": { "input_tokens": 100, "cached_input_tokens": 10, "output_tokens": 50, "total_tokens": 150 }, "model": "gpt-5" } } }"#,
                r#"{ "timestamp": "2026-01-02T00:00:02.000Z", "type" : "event_msg", "payload": { "type":"token_count", "info": { "total_token_usage": { "input_tokens": 200, "cached_input_tokens": 20, "output_tokens": 75, "total_tokens": 275 }, "model": "gpt-5" } } }"#,
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].timestamp, "2026-01-02T00:00:01.000Z");
        assert_eq!(events[0].model.as_deref(), Some("gpt-5"));
        assert_eq!(events[0].input_tokens, 100);
        assert_eq!(events[0].cached_input_tokens, 10);
        assert_eq!(events[0].output_tokens, 50);
        assert_eq!(events[0].total_tokens, 150);
        assert_eq!(events[1].timestamp, "2026-01-02T00:00:02.000Z");
        assert_eq!(events[1].input_tokens, 100);
        assert_eq!(events[1].cached_input_tokens, 10);
        assert_eq!(events[1].output_tokens, 25);
        assert_eq!(events[1].total_tokens, 125);
    }

    #[test]
    fn loads_headless_usage_with_unexpected_noncritical_field_types() {
        let fixture = fs_fixture!({
            "run.jsonl":
            json!({
                "type": "turn.completed",
                "timestamp": false,
                "model": {
                    "name": "unexpected"
                },
                "usage": {
                    "input_tokens": 120,
                    "cached_input_tokens": 20,
                    "output_tokens": 30,
                    "total_tokens": 150,
                },
            })
            .to_string(),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "run");
        assert_eq!(events[0].model.as_deref(), Some("gpt-5"));
        assert!(events[0].is_fallback_model);
        assert_eq!(events[0].input_tokens, 120);
        assert_eq!(events[0].cached_input_tokens, 20);
        assert_eq!(events[0].output_tokens, 30);
        assert_eq!(events[0].total_tokens, 150);
    }

    #[test]
    fn resolves_legacy_fallback_model_when_context_appears_later() {
        let fixture = fs_fixture!({
            "late-context.jsonl": [
                json!({
                    "timestamp": "2026-05-10T22:00:00.000Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1_000,
                                "cached_input_tokens": 600,
                                "output_tokens": 100,
                                "reasoning_output_tokens": 20,
                                "total_tokens": 1_100,
                            },
                        },
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-05-10T22:01:00.000Z",
                    "type": "turn_context",
                    "payload": {
                        "model": "gpt-5.5",
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-05-10T22:02:00.000Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 500,
                                "cached_input_tokens": 100,
                                "output_tokens": 50,
                                "reasoning_output_tokens": 10,
                                "total_tokens": 550,
                            },
                        },
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .iter()
                .map(|event| event.model.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("gpt-5.5"), Some("gpt-5.5")]
        );
        assert!(events.iter().all(|event| !event.is_fallback_model));
    }

    #[test]
    fn resolves_codex_auto_review_to_latest_model_for_event_date() {
        let fixture = fs_fixture!({
            "run.jsonl": [
                json!({
                    "type": "turn.completed",
                    "timestamp": "2026-02-05T00:00:00.000Z",
                    "model": "codex-auto-review",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "total_tokens": 15,
                    },
                })
                .to_string(),
                json!({
                    "type": "turn.completed",
                    "timestamp": "2026-03-05T00:00:00.000Z",
                    "model": "codex-auto-review",
                    "usage": {
                        "input_tokens": 20,
                        "output_tokens": 10,
                        "total_tokens": 30,
                    },
                })
                .to_string(),
                json!({
                    "type": "turn.completed",
                    "timestamp": "2026-04-23T00:00:00.000Z",
                    "model": "codex-auto-review",
                    "usage": {
                        "input_tokens": 30,
                        "output_tokens": 15,
                        "total_tokens": 45,
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(events[1].model.as_deref(), Some("gpt-5.4"));
        assert_eq!(events[2].model.as_deref(), Some("gpt-5.5"));
        assert!(events.iter().all(|event| event.is_fallback_model));
    }

    #[test]
    fn resolves_codex_auto_review_uses_created_at_when_top_level_timestamp_is_malformed() {
        let fixture = fs_fixture!({
            "run.jsonl": json!({
                "type": "turn.completed",
                "timestamp": "not-a-date",
                "created_at": "2025-12-11T00:00:00.000Z",
                "model": "codex-auto-review",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "total_tokens": 15,
                },
            })
            .to_string(),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.2-codex"));
        assert!(events[0].is_fallback_model);
    }

    #[test]
    fn resolves_codex_auto_review_turn_context_for_each_event_date() {
        let fixture = fs_fixture!({
            "session.jsonl": [
                json!({
                    "timestamp": "2025-12-11T00:00:00.000Z",
                    "type": "turn_context",
                    "payload": {
                        "model": "codex-auto-review",
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2025-12-11T00:01:00.000Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 10,
                                "output_tokens": 5,
                                "total_tokens": 15,
                            },
                        },
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-04-23T00:01:00.000Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 20,
                                "output_tokens": 10,
                                "total_tokens": 30,
                            },
                        },
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(events[1].model.as_deref(), Some("gpt-5.5"));
        assert!(events.iter().all(|event| event.is_fallback_model));
    }

    #[test]
    fn keeps_explicit_codex_auto_review_model_when_later_context_differs() {
        let fixture = fs_fixture!({
            "session.jsonl": [
                json!({
                    "timestamp": "2026-04-23T00:01:00.000Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "model": "codex-auto-review",
                            "last_token_usage": {
                                "input_tokens": 20,
                                "output_tokens": 10,
                                "total_tokens": 30,
                            },
                        },
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-04-23T00:02:00.000Z",
                    "type": "turn_context",
                    "payload": {
                        "model": "gpt-5.6-sol",
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.5"));
        assert!(events[0].is_fallback_model);
    }

    #[test]
    fn skips_initial_codex_desktop_fork_replay_without_bootstrap_marker() {
        let fixture = fs_fixture!({
            "rollout-2026-06-15T08-13-09-019ec9b2-a82c-7b02-a6b1-44ec3c6e3723.jsonl": [
                json!({
                    "timestamp": "2026-06-15T05:13:09.000Z",
                    "type": "session_meta",
                    "payload": {
                        "id": "019ec9b2-a82c-7b02-a6b1-44ec3c6e3723",
                        "forked_from_id": "019ec646-0bd0-7fc2-bfe3-e2ab626ede0b",
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-06-15T05:13:09.200Z",
                    "type": "turn_context",
                    "payload": {
                        "model": "gpt-5.5",
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-06-15T05:13:09.300Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 200_000,
                                "cached_input_tokens": 190_000,
                                "output_tokens": 1_000,
                                "reasoning_output_tokens": 100,
                                "total_tokens": 201_000,
                            },
                        },
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-06-15T05:13:12.000Z",
                    "type": "turn_context",
                    "payload": {
                        "model": "gpt-5.5",
                    },
                })
                .to_string(),
                json!({
                    "timestamp": "2026-06-15T05:13:12.500Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1_500,
                                "cached_input_tokens": 1_000,
                                "output_tokens": 200,
                                "reasoning_output_tokens": 50,
                                "total_tokens": 1_700,
                            },
                        },
                    },
                })
                .to_string(),
            ]
            .join("\n"),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.5"));
        assert_eq!(events[0].input_tokens, 1_500);
        assert_eq!(events[0].cached_input_tokens, 1_000);
        assert_eq!(events[0].output_tokens, 200);
        assert_eq!(events[0].total_tokens, 1_700);
    }

    #[test]
    fn loads_headless_usage_with_token_count_text_content() {
        let fixture = fs_fixture!({
            "run.jsonl":
            json!({
                "type": "turn.completed",
                "timestamp": "2026-01-02T03:04:05.000Z",
                "model": "gpt-5.2-codex",
                "content": "debug token_count payload text",
                "usage": {
                    "input_tokens": 120,
                    "cached_input_tokens": 20,
                    "output_tokens": 30,
                    "total_tokens": 150,
                },
            })
            .to_string(),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "run");
        assert_eq!(events[0].timestamp, "2026-01-02T03:04:05.000Z");
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(events[0].input_tokens, 120);
        assert_eq!(events[0].cached_input_tokens, 20);
        assert_eq!(events[0].output_tokens, 30);
        assert_eq!(events[0].total_tokens, 150);
    }

    #[test]
    fn uses_nested_model_name_for_standalone_exec_usage() {
        let fixture = fs_fixture!({
            "solo.jsonl":
            json!({
                "data": {
                    "timestamp": "2026-03-01T00:00:00.000Z",
                    "model_name": "gpt-5.2-codex",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "total_tokens": 15,
                    },
                },
            })
            .to_string(),
        });

        let events = load_codex_events_from_directory(fixture.root(), true).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "solo");
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(events[0].input_tokens, 10);
        assert_eq!(events[0].output_tokens, 5);
        assert_eq!(events[0].total_tokens, 15);
    }
}
