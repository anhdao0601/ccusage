use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use jiff::tz::TimeZone as JiffTimeZone;
use memchr::memmem;
use serde::Deserialize;

use crate::{
    calculate_cost_for_usage,
    cli::{CostMode, SharedArgs},
    fast::{byte_lines, suffix_string, FxHashMap, SmallIndexVec},
    format_date_tz, log_level, missing_pricing_model_for_usage, parse_ts_timestamp, parse_tz,
    ModelBreakdown, PricingMap, Result, Speed, TimestampMs, TokenCounts, TokenUsageRaw,
    UsageSummary,
};

use super::{
    chunk_file_indexes_by_size, has_unsupported_null_field, is_supported_version,
    paths::{extract_project, usage_files},
    usage_dedupe_hash, PathLoader,
};

pub(super) fn load_daily_summaries_inner(
    shared: &SharedArgs,
    project_filter: Option<&str>,
    group_by_project: bool,
    paths: PathLoader,
) -> Result<Vec<UsageSummary>> {
    let paths = paths()?;
    let files = usage_files(&paths, project_filter);
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let pricing = if shared.mode == CostMode::Display {
        None
    } else {
        Some(PricingMap::load(
            shared.offline,
            shared.update_pricing,
            log_level() != Some(0),
        ))
    };
    let tz = parse_tz(shared.timezone.as_deref());
    let mode = shared.mode;
    let loaded_files = if shared.single_thread {
        files
            .iter()
            .map(|file| read_daily_usage_file(file, tz.as_ref(), mode, pricing.as_ref()))
            .collect::<Vec<_>>()
    } else {
        read_daily_usage_files_parallel(&files, tz.as_ref(), mode, pricing.as_ref())
    };

    let mut deduped_indexes: FxHashMap<u64, SmallIndexVec> = FxHashMap::default();
    let mut deduped = Vec::with_capacity(loaded_files.iter().map(|file| file.entries.len()).sum());
    for loaded_file in loaded_files {
        for entry in loaded_file.entries {
            if let Some(filter) = project_filter {
                if entry.project.as_ref() != filter {
                    continue;
                }
            }
            push_deduped_daily_entry(entry, &mut deduped_indexes, &mut deduped);
        }
    }

    if group_by_project {
        let mut groups = BTreeMap::<(String, Arc<str>), DailyAccumulator>::new();
        for entry in &deduped {
            groups
                .entry((entry.date.clone(), Arc::clone(&entry.project)))
                .or_default()
                .add_entry(entry);
        }
        return Ok(groups
            .into_iter()
            .map(|((date, project), group)| {
                let mut summary = group.into_summary();
                summary.date = Some(date);
                summary.project = Some(project.to_string());
                summary
            })
            .collect());
    }

    let mut groups = BTreeMap::<String, DailyAccumulator>::new();
    for entry in &deduped {
        groups
            .entry(entry.date.clone())
            .or_default()
            .add_entry(entry);
    }
    Ok(groups
        .into_iter()
        .map(|(key, group)| {
            let mut summary = group.into_summary();
            summary.date = Some(key);
            summary
        })
        .collect())
}

#[derive(Debug)]
struct DailyLoadedFile {
    timestamp: Option<TimestampMs>,
    entries: Vec<DailyLoadedEntry>,
}

#[derive(Debug)]
struct DailyLoadedEntry {
    date: String,
    project: Arc<str>,
    usage: TokenUsageRaw,
    cost: f64,
    model: Option<String>,
    missing_pricing_model: Option<String>,
    message_id: Option<String>,
    request_id: Option<String>,
    is_sidechain: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DailyUsageEntry {
    timestamp: String,
    message: DailyUsageMessage,
    version: Option<String>,
    session_id: Option<String>,
    #[serde(rename = "costUSD")]
    cost_usd: Option<f64>,
    request_id: Option<String>,
    is_sidechain: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DailyUsageLine {
    Direct(DailyUsageEntry),
    AgentProgress(DailyAgentProgressEntry),
}

impl DailyUsageLine {
    fn into_entry(self) -> DailyUsageEntry {
        match self {
            DailyUsageLine::Direct(entry) => entry,
            DailyUsageLine::AgentProgress(entry) => DailyUsageEntry {
                timestamp: entry.data.message.timestamp,
                message: entry.data.message.message,
                version: None,
                session_id: None,
                cost_usd: entry.data.message.cost_usd,
                request_id: entry.data.message.request_id,
                is_sidechain: entry.data.message.is_sidechain,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct DailyAgentProgressEntry {
    data: DailyAgentProgressData,
}

#[derive(Debug, Deserialize)]
struct DailyAgentProgressData {
    message: DailyAgentProgressMessage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DailyAgentProgressMessage {
    timestamp: String,
    message: DailyUsageMessage,
    #[serde(rename = "costUSD")]
    cost_usd: Option<f64>,
    request_id: Option<String>,
    is_sidechain: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DailyUsageMessage {
    usage: TokenUsageRaw,
    model: Option<String>,
    id: Option<String>,
}

fn read_daily_usage_files_parallel(
    files: &[PathBuf],
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> Vec<DailyLoadedFile> {
    let worker_count = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(files.len());
    if worker_count <= 1 {
        return files
            .iter()
            .map(|file| read_daily_usage_file(file, tz, mode, pricing))
            .collect();
    }

    let chunks = chunk_file_indexes_by_size(files, worker_count);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for chunk in chunks {
            let tz = tz.cloned();
            handles.push(scope.spawn(move || {
                chunk
                    .into_iter()
                    .map(|index| {
                        (
                            index,
                            read_daily_usage_file(&files[index], tz.as_ref(), mode, pricing),
                        )
                    })
                    .collect::<Vec<_>>()
            }));
        }
        let mut loaded_files = Vec::with_capacity(files.len());
        loaded_files.resize_with(files.len(), || None);
        for (index, file) in handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("daily usage worker panicked"))
        {
            loaded_files[index] = Some(file);
        }
        loaded_files
            .into_iter()
            .map(|file| file.expect("daily usage worker returned every file"))
            .collect()
    })
}

fn read_daily_usage_file(
    path: &Path,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> DailyLoadedFile {
    let project: Arc<str> = Arc::from(extract_project(path));
    let mut loaded_file = DailyLoadedFile {
        timestamp: None,
        entries: Vec::new(),
    };
    let Ok(content) = fs::read(path) else {
        return loaded_file;
    };

    let usage_marker = memmem::Finder::new(br#""usage":{"#);
    for line in byte_lines(&content) {
        if usage_marker.find(line).is_none() {
            continue;
        }
        if has_unsupported_null_field(line) {
            continue;
        }
        let Ok(data) = serde_json::from_slice::<DailyUsageLine>(line) else {
            continue;
        };
        let data = data.into_entry();
        let Some(timestamp) = parse_ts_timestamp(&data.timestamp) else {
            continue;
        };
        loaded_file.timestamp = Some(
            loaded_file
                .timestamp
                .map_or(timestamp, |current| current.min(timestamp)),
        );
        if !is_valid_daily_usage_entry(&data) {
            continue;
        }
        let usage = data.message.usage;
        let cost = calculate_cost_for_usage(
            data.message.model.as_deref(),
            usage,
            data.cost_usd,
            mode,
            pricing,
        );
        let missing_pricing_model = missing_pricing_model_for_usage(
            data.message.model.as_deref(),
            usage,
            data.cost_usd,
            mode,
            pricing,
        );
        let model = data.message.model.as_ref().and_then(|model| {
            if model == "<synthetic>" {
                None
            } else if matches!(usage.speed, Some(Speed::Fast)) {
                Some(suffix_string(model, "-fast"))
            } else {
                Some(model.clone())
            }
        });
        loaded_file.entries.push(DailyLoadedEntry {
            date: format_date_tz(timestamp, tz),
            project: Arc::clone(&project),
            usage,
            cost,
            model,
            missing_pricing_model,
            message_id: data.message.id,
            request_id: data.request_id,
            is_sidechain: data.is_sidechain,
        });
    }
    loaded_file
}

fn is_valid_daily_usage_entry(data: &DailyUsageEntry) -> bool {
    if data
        .version
        .as_deref()
        .is_some_and(|version| !is_supported_version(version))
    {
        return false;
    }
    if data
        .session_id
        .as_deref()
        .is_some_and(|session_id| session_id.is_empty())
    {
        return false;
    }
    if data
        .request_id
        .as_deref()
        .is_some_and(|request_id| request_id.is_empty())
    {
        return false;
    }
    if data
        .message
        .id
        .as_deref()
        .is_some_and(|message_id| message_id.is_empty())
    {
        return false;
    }
    if data
        .message
        .model
        .as_deref()
        .is_some_and(|model| model.is_empty())
    {
        return false;
    }
    true
}
fn daily_usage_token_total(entry: &DailyLoadedEntry) -> u64 {
    entry.usage.input_tokens
        + entry.usage.output_tokens
        + entry.usage.cache_creation_token_count()
        + entry.usage.cache_read_input_tokens
}

fn push_deduped_daily_entry(
    entry: DailyLoadedEntry,
    deduped_indexes: &mut FxHashMap<u64, SmallIndexVec>,
    deduped: &mut Vec<DailyLoadedEntry>,
) {
    let dedupe_lookup = entry.message_id.as_deref().map(|message_id| {
        let request_id = entry.request_id.as_deref();
        let exact_hash = usage_dedupe_hash(message_id, request_id);
        let existing_index = deduped_indexes
            .get(&exact_hash)
            .and_then(|indexes| {
                indexes.iter().copied().find(|&index| {
                    deduped[index].message_id.as_deref() == Some(message_id)
                        && deduped[index].request_id.as_deref() == request_id
                })
            })
            .or_else(|| {
                // /btw sidechain logs can replay parent messages with new request IDs.
                let message_hash = usage_dedupe_hash(message_id, None);
                let candidate_is_sidechain = is_sidechain_daily_entry(&entry);
                deduped_indexes.get(&message_hash).and_then(|indexes| {
                    indexes.iter().copied().find(|&index| {
                        deduped[index].message_id.as_deref() == Some(message_id)
                            && (candidate_is_sidechain || is_sidechain_daily_entry(&deduped[index]))
                    })
                })
            });
        (exact_hash, existing_index)
    });

    if let Some((_, Some(index))) = dedupe_lookup {
        if should_replace_deduped_daily_entry(&entry, &deduped[index]) {
            deduped[index] = entry;
            index_deduped_daily_entry(deduped_indexes, &deduped[index], index);
        }
        return;
    }

    let index = deduped.len();
    deduped.push(entry);
    if let Some((hash, None)) = dedupe_lookup {
        push_deduped_daily_index(deduped_indexes, hash, index);
        index_deduped_daily_entry(deduped_indexes, &deduped[index], index);
    }
}

fn should_replace_deduped_daily_entry(
    candidate: &DailyLoadedEntry,
    existing: &DailyLoadedEntry,
) -> bool {
    let candidate_is_sidechain = is_sidechain_daily_entry(candidate);
    let existing_is_sidechain = is_sidechain_daily_entry(existing);
    if candidate_is_sidechain != existing_is_sidechain {
        return existing_is_sidechain;
    }

    let candidate_total = daily_usage_token_total(candidate);
    let existing_total = daily_usage_token_total(existing);
    if candidate_total != existing_total {
        return candidate_total > existing_total;
    }
    if candidate.cost != existing.cost {
        return candidate.cost > existing.cost;
    }
    candidate.usage.speed.is_some() && existing.usage.speed.is_none()
}

fn is_sidechain_daily_entry(entry: &DailyLoadedEntry) -> bool {
    entry.is_sidechain == Some(true)
}

fn push_deduped_daily_index(
    deduped_indexes: &mut FxHashMap<u64, SmallIndexVec>,
    hash: u64,
    index: usize,
) {
    let indexes = deduped_indexes.entry(hash).or_default();
    if !indexes.contains(&index) {
        indexes.push(index);
    }
}

fn index_deduped_daily_entry(
    deduped_indexes: &mut FxHashMap<u64, SmallIndexVec>,
    entry: &DailyLoadedEntry,
    index: usize,
) {
    if let Some(message_id) = entry.message_id.as_deref() {
        push_deduped_daily_index(
            deduped_indexes,
            usage_dedupe_hash(message_id, entry.request_id.as_deref()),
            index,
        );
        push_deduped_daily_index(deduped_indexes, usage_dedupe_hash(message_id, None), index);
    }
}

#[derive(Default)]
struct DailyAccumulator {
    counts: TokenCounts,
    cost: f64,
    models: Vec<String>,
    breakdowns: Vec<ModelBreakdown>,
    breakdown_indexes: FxHashMap<String, usize>,
}

impl DailyAccumulator {
    fn add_entry(&mut self, entry: &DailyLoadedEntry) {
        self.counts.add_usage(entry.usage);
        self.cost += entry.cost;
        if let Some(model) = &entry.model {
            let model = crate::model_aliases::resolve_model_name(model);
            let index = if let Some(index) = self.breakdown_indexes.get(model.as_ref()) {
                *index
            } else {
                let index = self.breakdowns.len();
                let owned = model.into_owned();
                self.breakdown_indexes.insert(owned.clone(), index);
                self.models.push(owned.clone());
                self.breakdowns.push(ModelBreakdown {
                    model_name: owned,
                    ..ModelBreakdown::default()
                });
                index
            };
            let breakdown = &mut self.breakdowns[index];
            breakdown.input_tokens += entry.usage.input_tokens;
            breakdown.output_tokens += entry.usage.output_tokens;
            breakdown.cache_creation_tokens += entry.usage.cache_creation_token_count();
            breakdown.cache_read_tokens += entry.usage.cache_read_input_tokens;
            breakdown.cost += entry.cost;
            if entry.missing_pricing_model.is_some() {
                breakdown.missing_pricing = true;
            }
        }
    }

    fn into_summary(mut self) -> UsageSummary {
        self.breakdowns.sort_by(|a, b| b.cost.total_cmp(&a.cost));
        UsageSummary {
            date: None,
            month: None,
            week: None,
            session_id: None,
            project_path: None,
            last_activity: None,
            input_tokens: self.counts.input_tokens,
            output_tokens: self.counts.output_tokens,
            cache_creation_tokens: self.counts.cache_creation_tokens,
            cache_read_tokens: self.counts.cache_read_tokens,
            extra_total_tokens: 0,
            total_cost: self.cost,
            credits: None,
            message_count: None,
            models_used: self.models,
            model_breakdowns: self.breakdowns,
            project: None,
            versions: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        is_valid_daily_usage_entry, push_deduped_daily_entry, DailyAccumulator, DailyLoadedEntry,
    };
    use crate::TokenUsageRaw;

    #[test]
    fn keeps_parent_daily_usage_when_sidechain_replays_message_with_new_request_id() {
        let mut deduped_indexes = Default::default();
        let mut deduped = Vec::new();

        push_deduped_daily_entry(
            daily_loaded_entry(DailyEntryFixture {
                message_id: "msg-parent",
                request_id: "req-parent",
                is_sidechain: false,
                cache_read_tokens: 20,
                output_tokens: 10,
            }),
            &mut deduped_indexes,
            &mut deduped,
        );
        push_deduped_daily_entry(
            daily_loaded_entry(DailyEntryFixture {
                message_id: "msg-parent",
                request_id: "req-sidechain-replay",
                is_sidechain: true,
                cache_read_tokens: 50_000,
                output_tokens: 10,
            }),
            &mut deduped_indexes,
            &mut deduped,
        );
        push_deduped_daily_entry(
            daily_loaded_entry(DailyEntryFixture {
                message_id: "msg-sidechain-answer",
                request_id: "req-sidechain-answer",
                is_sidechain: true,
                cache_read_tokens: 700,
                output_tokens: 30,
            }),
            &mut deduped_indexes,
            &mut deduped,
        );

        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].message_id.as_deref(), Some("msg-parent"));
        assert_eq!(deduped[0].request_id.as_deref(), Some("req-parent"));
        assert_eq!(deduped[0].usage.cache_read_input_tokens, 20);
        assert_eq!(
            deduped[1].message_id.as_deref(),
            Some("msg-sidechain-answer")
        );
        assert_eq!(deduped[1].usage.cache_read_input_tokens, 700);
    }

    #[test]
    fn propagates_sidechain_metadata_from_agent_progress_lines() {
        let data = serde_json::from_str::<super::DailyUsageLine>(
            r#"{"data":{"message":{"timestamp":"2026-03-29T07:00:00.000Z","requestId":"req-sidechain","isSidechain":true,"message":{"usage":{"input_tokens":0,"output_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":20},"model":"claude-sonnet-4-20250514","id":"msg-sidechain"}}}}"#,
        )
        .unwrap()
        .into_entry();

        assert_eq!(data.is_sidechain, Some(true));
    }

    #[test]
    fn accepts_ncode_unknown_version_daily_usage_entries() {
        let data = serde_json::from_str::<super::DailyUsageLine>(
            r#"{"parentUuid":"2552b92a-e274-4655-bebd-544b51693c44","isSidechain":false,"message":{"model":"claude-opus-4-6","id":"msg_bdrk_017eKctTmYXcRx3VFkPTNzo6","type":"message","role":"assistant","content":[],"usage":{"input_tokens":3,"cache_creation_input_tokens":18645,"cache_read_input_tokens":0,"output_tokens":11}},"type":"assistant","uuid":"fc9e98a3-c8b4-4d41-95b3-9f7347e76f38","timestamp":"2026-06-20T11:44:35.754Z","userType":"internal","entrypoint":"cli","cwd":"/Users/daoa/work","sessionId":"ad4d1aed-504c-46c9-aa25-4982f47d4ac2","version":"unknown","gitBranch":"HEAD"}"#,
        )
        .unwrap()
        .into_entry();

        assert!(is_valid_daily_usage_entry(&data));
    }

    #[test]
    fn refreshes_dedupe_indexes_when_parent_replaces_sidechain_replay() {
        let mut deduped_indexes = Default::default();
        let mut deduped = Vec::new();

        push_deduped_daily_entry(
            daily_loaded_entry(DailyEntryFixture {
                message_id: "msg-parent",
                request_id: "req-sidechain-replay",
                is_sidechain: true,
                cache_read_tokens: 50_000,
                output_tokens: 10,
            }),
            &mut deduped_indexes,
            &mut deduped,
        );
        push_deduped_daily_entry(
            daily_loaded_entry(DailyEntryFixture {
                message_id: "msg-parent",
                request_id: "req-parent",
                is_sidechain: false,
                cache_read_tokens: 20,
                output_tokens: 10,
            }),
            &mut deduped_indexes,
            &mut deduped,
        );
        push_deduped_daily_entry(
            daily_loaded_entry(DailyEntryFixture {
                message_id: "msg-parent",
                request_id: "req-parent",
                is_sidechain: false,
                cache_read_tokens: 20,
                output_tokens: 10,
            }),
            &mut deduped_indexes,
            &mut deduped,
        );

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].request_id.as_deref(), Some("req-parent"));
        assert!(!deduped[0].is_sidechain.unwrap());
    }

    #[test]
    fn daily_summary_merges_configured_model_aliases() {
        let _aliases = crate::model_aliases::set_model_aliases_for_tests([
            ("private-claude-a", "claude-opus-4-8"),
            ("private-claude-b", "claude-opus-4-8"),
        ]);
        let mut accumulator = DailyAccumulator::default();
        let mut first = daily_loaded_entry(DailyEntryFixture {
            message_id: "msg-a",
            request_id: "req-a",
            is_sidechain: false,
            cache_read_tokens: 20,
            output_tokens: 10,
        });
        first.model = Some("private-claude-a".to_string());
        let mut second = daily_loaded_entry(DailyEntryFixture {
            message_id: "msg-b",
            request_id: "req-b",
            is_sidechain: false,
            cache_read_tokens: 30,
            output_tokens: 15,
        });
        second.model = Some("private-claude-b".to_string());

        accumulator.add_entry(&first);
        accumulator.add_entry(&second);
        let summary = accumulator.into_summary();

        assert_eq!(summary.models_used, vec!["claude-opus-4-8"]);
        assert_eq!(summary.model_breakdowns.len(), 1);
        assert_eq!(summary.model_breakdowns[0].cache_read_tokens, 50);
        assert_eq!(summary.model_breakdowns[0].output_tokens, 25);
    }

    struct DailyEntryFixture {
        message_id: &'static str,
        request_id: &'static str,
        is_sidechain: bool,
        cache_read_tokens: u64,
        output_tokens: u64,
    }

    fn daily_loaded_entry(fixture: DailyEntryFixture) -> DailyLoadedEntry {
        DailyLoadedEntry {
            date: "2026-03-29".to_string(),
            project: Arc::from("project-a"),
            usage: TokenUsageRaw {
                input_tokens: 0,
                output_tokens: fixture.output_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: fixture.cache_read_tokens,
                speed: None,
                cache_creation: None,
            },
            cost: 0.0,
            model: Some("claude-sonnet-4-20250514".to_string()),
            missing_pricing_model: None,
            message_id: Some(fixture.message_id.to_string()),
            request_id: Some(fixture.request_id.to_string()),
            is_sidechain: Some(fixture.is_sidechain),
        }
    }
}
