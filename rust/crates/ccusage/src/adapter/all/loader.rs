use std::{collections::BTreeMap, sync::mpsc, thread};

use serde_json::{json, Value};

use crate::{
    adapter::{
        amp, claude, codebuff, codex, copilot, droid, gemini, goose, hermes, kilo, kimi, ncode,
        openclaw, opencode, pi, qwen, zcode,
    },
    cli::{AgentReportKind, CodexSpeed, SharedArgs, WeekDay},
    filter_loaded_entries_by_date, json_float, summarize_by_key, summarize_summaries_by_bucket,
    BucketKind, CodexGroup, LoadedEntry, ModelBreakdown, PricingMap, Result, SessionAccumulator,
    UsageSummary,
};

use super::{
    model::{canonical_model_name, model_group_name, model_matches_selectors},
    report::sort_rows,
    types::{
        merge_agent_breakdown, merge_model_breakdowns, AgentLoadSpec, AgentRows, AllAccumulator,
        AllLoadResult, AllRow, CachedAgentRows, LoadedAgentRows,
    },
};

pub(super) fn load_cached_agent_rows(
    agent: &'static str,
    sources: Vec<crate::report_cache::ReportSource>,
    load_kind: AgentReportKind,
    shared: &SharedArgs,
    load: impl FnOnce() -> Result<AgentRows>,
) -> Result<AgentRows> {
    let mut parameters = json!({
        "agent": agent,
        "kind": format!("{load_kind:?}"),
        "since": shared.since,
        "until": shared.until,
        "mode": format!("{:?}", shared.mode),
        "offline": shared.offline,
        "timezone": shared.timezone,
    });
    if agent == "codex" {
        // Codex costs depend on the speed tier resolved from config.toml,
        // which is not covered by the codex source fingerprint.
        parameters["codexSpeed"] = json!(format!(
            "{:?}",
            codex::resolve_codex_speed(CodexSpeed::Auto)
        ));
    }
    let cached: CachedAgentRows =
        crate::report_cache::with_report_cache("all-agent", parameters, sources, shared, || {
            load().map(AgentRows::into_cache)
        })?;
    Ok(cached.into_agent_rows())
}

pub(super) fn load_rows(kind: AgentReportKind, shared: &SharedArgs) -> Result<AllLoadResult> {
    let mut progress = crate::progress::UsageLoadProgress::new(
        crate::log_level() != Some(0)
            && crate::progress::should_show_usage_load_progress(
                shared.json,
                crate::progress::usage_load_output_is_tty(),
            ),
    );
    let pricing = PricingMap::load(
        shared.offline,
        shared.update_pricing,
        crate::log_level() != Some(0),
    );
    let load_kind = match kind {
        AgentReportKind::Session => AgentReportKind::Session,
        AgentReportKind::Daily | AgentReportKind::Weekly | AgentReportKind::Monthly => {
            AgentReportKind::Daily
        }
    };
    let loader_shared = SharedArgs {
        json: true,
        ..shared.clone()
    };
    let mut specs = vec![
        AgentLoadSpec {
            index: 0,
            agent: "claude",
            progress_agent: crate::progress::UsageLoadAgent::Claude,
            load: Box::new(|| load_claude_rows(load_kind, &loader_shared)),
        },
        AgentLoadSpec {
            index: 1,
            agent: "ncode",
            progress_agent: crate::progress::UsageLoadAgent::NCode,
            load: Box::new(|| load_ncode_rows(load_kind, &loader_shared)),
        },
        AgentLoadSpec {
            index: 2,
            agent: "codex",
            progress_agent: crate::progress::UsageLoadAgent::Codex,
            load: Box::new(|| load_codex_rows(load_kind, &loader_shared, &pricing)),
        },
        AgentLoadSpec {
            index: 3,
            agent: "opencode",
            progress_agent: crate::progress::UsageLoadAgent::OpenCode,
            load: Box::new(|| {
                load_summary_agent_rows(
                    "opencode",
                    load_kind,
                    &loader_shared,
                    || opencode::loader::load_entries(&loader_shared),
                    opencode::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 4,
            agent: "amp",
            progress_agent: crate::progress::UsageLoadAgent::Amp,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "amp",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    amp::load_entries,
                    amp::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 5,
            agent: "droid",
            progress_agent: crate::progress::UsageLoadAgent::Droid,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "droid",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    droid::load_entries,
                    droid::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 6,
            agent: "codebuff",
            progress_agent: crate::progress::UsageLoadAgent::Codebuff,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "codebuff",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    codebuff::load_entries,
                    codebuff::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 7,
            agent: "hermes",
            progress_agent: crate::progress::UsageLoadAgent::Hermes,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "hermes",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    hermes::load_entries,
                    hermes::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 8,
            agent: "pi",
            progress_agent: crate::progress::UsageLoadAgent::Pi,
            load: Box::new(|| {
                load_session_capable_summary_agent_rows(
                    "pi",
                    load_kind,
                    &loader_shared,
                    pi::load_entries,
                    pi::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 9,
            agent: "goose",
            progress_agent: crate::progress::UsageLoadAgent::Goose,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "goose",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    goose::load_entries,
                    goose::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 10,
            agent: "openclaw",
            progress_agent: crate::progress::UsageLoadAgent::OpenClaw,
            load: Box::new(|| {
                load_summary_agent_rows(
                    "openclaw",
                    load_kind,
                    &loader_shared,
                    || openclaw::load_entries(&loader_shared, None),
                    openclaw::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 11,
            agent: "kilo",
            progress_agent: crate::progress::UsageLoadAgent::Kilo,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "kilo",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    kilo::load_entries,
                    kilo::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 12,
            agent: "copilot",
            progress_agent: crate::progress::UsageLoadAgent::Copilot,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "copilot",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    copilot::load_entries,
                    copilot::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 13,
            agent: "gemini",
            progress_agent: crate::progress::UsageLoadAgent::Gemini,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "gemini",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    gemini::load_entries,
                    gemini::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 14,
            agent: "kimi",
            progress_agent: crate::progress::UsageLoadAgent::Kimi,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "kimi",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    kimi::load_entries,
                    kimi::summarize_entries,
                )
            }),
        },
        AgentLoadSpec {
            index: 15,
            agent: "qwen",
            progress_agent: crate::progress::UsageLoadAgent::Qwen,
            load: Box::new(|| load_qwen_rows(load_kind, &loader_shared)),
        },
        AgentLoadSpec {
            index: 16,
            agent: "zcode",
            progress_agent: crate::progress::UsageLoadAgent::ZCode,
            load: Box::new(|| {
                load_priced_summary_agent_rows(
                    "zcode",
                    load_kind,
                    &loader_shared,
                    &pricing,
                    zcode::load_entries,
                    zcode::summarize_entries,
                )
            }),
        },
    ];
    filter_agent_specs(&mut specs, shared);
    let loader_shared = &loader_shared;
    let specs = specs
        .into_iter()
        .map(|spec| AgentLoadSpec {
            index: spec.index,
            agent: spec.agent,
            progress_agent: spec.progress_agent,
            load: Box::new(move || {
                load_cached_agent_rows(
                    spec.agent,
                    crate::report_cache::agent_report_sources(spec.agent),
                    load_kind,
                    loader_shared,
                    spec.load,
                )
            }),
        })
        .collect();
    let loaded = load_agent_rows_parallel(specs, &mut progress)?;
    let mut detected_agents = Vec::new();
    let mut rows = Vec::new();
    for loaded in loaded {
        append_agent_rows(
            &mut rows,
            &mut detected_agents,
            loaded.agent,
            loaded.agent_rows,
        );
    }
    let mut rows = group_rows(rows, kind, shared);
    sort_rows(&mut rows, &shared.order);
    Ok(AllLoadResult {
        rows,
        detected_agents,
    })
}

pub(super) fn group_rows(
    mut rows: Vec<AllRow>,
    kind: AgentReportKind,
    shared: &SharedArgs,
) -> Vec<AllRow> {
    if let Some(selectors) = shared.model_filter.as_deref() {
        rows = filter_rows_by_model(rows, selectors);
    }
    if shared.by_provider {
        if shared.by_model && shared.model_filter.is_none() {
            for row in &mut rows {
                canonicalize_row_models(row);
            }
        }
        return split_rows_by_model(aggregate_rows_by_provider(rows), shared);
    }
    if shared.by_model {
        return aggregate_rows_by_model(rows, shared.model_filter.as_deref());
    }
    if kind == AgentReportKind::Session {
        for row in &mut rows {
            row.metadata_agents = None;
        }
        return rows;
    }
    aggregate_rows(rows, kind)
}

pub(super) fn aggregate_rows_by_provider(rows: Vec<AllRow>) -> Vec<AllRow> {
    let mut groups = BTreeMap::<&'static str, AllRow>::new();
    for mut row in rows {
        row.period = "all".to_string();
        row.metadata = None;
        row.metadata_agents = Some(vec![row.agent]);
        match groups.get_mut(row.agent) {
            Some(existing) => merge_agent_breakdown(existing, row),
            None => {
                groups.insert(row.agent, row);
            }
        }
    }

    groups
        .into_values()
        .map(|mut row| {
            row.period = "all".to_string();
            row.metadata = None;
            row.metadata_agents = Some(vec![row.agent]);
            row.agent_breakdowns = None;
            row
        })
        .collect()
}

pub(super) fn aggregate_rows_by_model(
    rows: Vec<AllRow>,
    selectors: Option<&[String]>,
) -> Vec<AllRow> {
    let mut groups = BTreeMap::<String, AllAccumulator>::new();
    for row in rows {
        for mut breakdown in row.model_breakdowns.iter().cloned() {
            let model_name = model_group_name(&breakdown.model_name, selectors);
            breakdown.model_name.clone_from(&model_name);
            let mut model_row = model_breakdown_row(&row, breakdown, false);
            model_row.period = "all".to_string();
            model_row.models_used = vec![model_name.clone()];
            groups.entry(model_name).or_default().add(model_row);
        }
    }

    groups
        .into_iter()
        .map(|(model_name, group)| {
            let mut row = group.into_row("all".to_string());
            row.models_used = vec![model_name];
            row
        })
        .collect()
}

pub(super) fn filter_rows_by_model(rows: Vec<AllRow>, selectors: &[String]) -> Vec<AllRow> {
    rows.into_iter()
        .filter_map(|mut row| {
            canonicalize_row_models(&mut row);
            row.model_breakdowns
                .retain(|breakdown| model_matches_selectors(&breakdown.model_name, selectors));
            if row.model_breakdowns.is_empty() {
                return None;
            }
            apply_model_breakdowns_to_row(&mut row);
            Some(row)
        })
        .collect()
}

fn canonicalize_row_models(row: &mut AllRow) {
    let breakdowns = std::mem::take(&mut row.model_breakdowns)
        .into_iter()
        .map(|mut breakdown| {
            breakdown.model_name = canonical_model_name(&breakdown.model_name);
            breakdown
        });
    row.model_breakdowns = merge_model_breakdowns([], breakdowns);
    row.models_used = row
        .model_breakdowns
        .iter()
        .map(|breakdown| breakdown.model_name.clone())
        .collect();
}

fn apply_model_breakdowns_to_row(row: &mut AllRow) {
    row.models_used = row
        .model_breakdowns
        .iter()
        .map(|breakdown| breakdown.model_name.clone())
        .collect();
    row.input_tokens = row
        .model_breakdowns
        .iter()
        .map(|breakdown| breakdown.input_tokens)
        .sum();
    row.output_tokens = row
        .model_breakdowns
        .iter()
        .map(|breakdown| breakdown.output_tokens)
        .sum();
    row.cache_creation_tokens = row
        .model_breakdowns
        .iter()
        .map(|breakdown| breakdown.cache_creation_tokens)
        .sum();
    row.cache_read_tokens = row
        .model_breakdowns
        .iter()
        .map(|breakdown| breakdown.cache_read_tokens)
        .sum();
    row.total_tokens = row
        .model_breakdowns
        .iter()
        .map(model_breakdown_total_tokens)
        .sum();
    row.total_cost = row
        .model_breakdowns
        .iter()
        .map(|breakdown| breakdown.cost)
        .sum();
}

fn filter_agent_specs(specs: &mut Vec<AgentLoadSpec<'_>>, shared: &SharedArgs) {
    let Some(filter) = shared.tool_filter.as_ref() else {
        return;
    };
    specs.retain(|spec| filter.iter().any(|tool| tool == spec.agent));
}

pub(super) fn split_rows_by_model(rows: Vec<AllRow>, shared: &SharedArgs) -> Vec<AllRow> {
    if !shared.by_model {
        return rows;
    }
    rows.into_iter().flat_map(split_row_by_model).collect()
}

fn split_row_by_model(row: AllRow) -> Vec<AllRow> {
    if row.model_breakdowns.is_empty() {
        return vec![row];
    }

    let mut row = row;
    let mut breakdowns = std::mem::take(&mut row.model_breakdowns);
    breakdowns.sort_by(|a, b| a.model_name.cmp(&b.model_name));
    breakdowns
        .into_iter()
        .enumerate()
        .map(|(index, breakdown)| model_breakdown_row(&row, breakdown, index == 0))
        .collect()
}

fn model_breakdown_row(row: &AllRow, breakdown: ModelBreakdown, keep_metadata: bool) -> AllRow {
    let total_tokens = model_breakdown_total_tokens(&breakdown);
    let model_name = breakdown.model_name.clone();
    AllRow {
        period: row.period.clone(),
        agent: row.agent,
        models_used: vec![model_name],
        input_tokens: breakdown.input_tokens,
        output_tokens: breakdown.output_tokens,
        cache_creation_tokens: breakdown.cache_creation_tokens,
        cache_read_tokens: breakdown.cache_read_tokens,
        total_tokens,
        total_cost: breakdown.cost,
        metadata: keep_metadata.then(|| row.metadata.clone()).flatten(),
        metadata_agents: row.metadata_agents.clone(),
        agent_breakdowns: None,
        model_breakdowns: vec![breakdown],
    }
}

fn model_breakdown_total_tokens(breakdown: &ModelBreakdown) -> u64 {
    breakdown
        .input_tokens
        .saturating_add(breakdown.output_tokens)
        .saturating_add(breakdown.cache_creation_tokens)
        .saturating_add(breakdown.cache_read_tokens)
        .saturating_add(breakdown.extra_total_tokens)
}

pub(super) fn load_agent_rows_parallel(
    specs: Vec<AgentLoadSpec<'_>>,
    progress: &mut crate::progress::UsageLoadProgress,
) -> Result<Vec<LoadedAgentRows>> {
    for spec in &specs {
        progress.start(spec.progress_agent);
    }

    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let mut handles = Vec::with_capacity(specs.len());
        for spec in specs {
            let sender = sender.clone();
            handles.push((
                spec.index,
                spec.progress_agent,
                scope.spawn(move || {
                    let result = (spec.load)();
                    let _ = sender.send((spec.index, spec.agent, spec.progress_agent, result));
                }),
            ));
        }
        drop(sender);

        let mut loaded = Vec::with_capacity(handles.len());
        let mut errors = Vec::new();
        for (index, agent, progress_agent, result) in receiver {
            match result {
                Ok(agent_rows) => {
                    progress.succeed(progress_agent);
                    loaded.push(LoadedAgentRows {
                        index,
                        agent,
                        agent_rows,
                    });
                }
                Err(error) => {
                    progress.fail(progress_agent);
                    errors.push((index, error));
                }
            }
        }

        for (index, progress_agent, handle) in handles {
            if handle.join().is_err() {
                progress.fail(progress_agent);
                errors.push((index, crate::cli_error("agent loader panicked")));
            }
        }

        errors.sort_by_key(|(index, _)| *index);
        if let Some((_, error)) = errors.into_iter().next() {
            return Err(error);
        }

        loaded.sort_by_key(|loaded| loaded.index);
        Ok(loaded)
    })
}

fn append_agent_rows(
    rows: &mut Vec<AllRow>,
    detected_agents: &mut Vec<&'static str>,
    agent: &'static str,
    agent_rows: AgentRows,
) {
    if agent_rows.detected {
        detected_agents.push(agent);
    }
    rows.extend(agent_rows.rows);
}

fn load_summary_agent_rows(
    agent: &'static str,
    kind: AgentReportKind,
    shared: &SharedArgs,
    load_entries: impl FnOnce() -> Result<Vec<LoadedEntry>>,
    summarize_entries: impl FnOnce(&[LoadedEntry], AgentReportKind) -> Result<Vec<UsageSummary>>,
) -> Result<AgentRows> {
    let mut entries = load_entries()?;
    let detected = !entries.is_empty();
    filter_loaded_entries_by_date(&mut entries, shared);
    let summaries = summarize_entries(&entries, kind)?;
    Ok(AgentRows {
        rows: summary_rows(agent, summaries),
        detected,
    })
}

fn load_session_capable_summary_agent_rows(
    agent: &'static str,
    kind: AgentReportKind,
    shared: &SharedArgs,
    load_entries: impl FnOnce(&SharedArgs, Option<&str>) -> Result<Vec<LoadedEntry>>,
    summarize_entries: impl FnOnce(&[LoadedEntry], AgentReportKind) -> Result<Vec<UsageSummary>>,
) -> Result<AgentRows> {
    let mut entries = load_entries(shared, None)?;
    let detected = !entries.is_empty();
    let summaries = if kind == AgentReportKind::Session {
        let mut summaries = summarize_entry_sessions(&entries, shared.timezone.as_deref())?;
        filter_session_summaries(&mut summaries, shared);
        summaries
    } else {
        filter_loaded_entries_by_date(&mut entries, shared);
        summarize_entries(&entries, kind)?
    };
    Ok(AgentRows {
        rows: summary_rows(agent, summaries),
        detected,
    })
}

fn load_claude_rows(kind: AgentReportKind, shared: &SharedArgs) -> Result<AgentRows> {
    if kind == AgentReportKind::Session {
        return load_session_capable_summary_agent_rows(
            "claude",
            kind,
            shared,
            claude::load_entries,
            summarize_entries,
        );
    }

    let mut summaries = claude::load_daily_summaries(shared, None, false)?;
    let detected = !summaries.is_empty();
    filter_daily_summaries_by_date(&mut summaries, shared);
    Ok(AgentRows {
        rows: summary_rows("claude", summaries),
        detected,
    })
}

fn load_ncode_rows(kind: AgentReportKind, shared: &SharedArgs) -> Result<AgentRows> {
    if kind == AgentReportKind::Session {
        return load_session_capable_summary_agent_rows(
            "ncode",
            kind,
            shared,
            ncode::load_entries,
            summarize_entries,
        );
    }

    let mut summaries = ncode::load_daily_summaries(shared, None, false)?;
    let detected = !summaries.is_empty();
    filter_daily_summaries_by_date(&mut summaries, shared);
    Ok(AgentRows {
        rows: summary_rows("ncode", summaries),
        detected,
    })
}

fn filter_daily_summaries_by_date(rows: &mut Vec<UsageSummary>, shared: &SharedArgs) {
    if shared.since.is_none() && shared.until.is_none() {
        return;
    }
    rows.retain(|row| {
        let date = row.date.as_deref().unwrap_or_default().replace('-', "");
        shared.since.as_ref().is_none_or(|since| &date >= since)
            && shared.until.as_ref().is_none_or(|until| &date <= until)
    });
}

fn load_codex_rows(
    kind: AgentReportKind,
    shared: &SharedArgs,
    pricing: &PricingMap,
) -> Result<AgentRows> {
    if shared.since.is_none() && shared.until.is_none() {
        let groups = codex::load_groups(shared, kind)?;
        let detected = !groups.is_empty();
        let speed = codex::resolve_codex_speed(CodexSpeed::Auto);
        return Ok(AgentRows {
            rows: groups
                .iter()
                .map(|(period, group)| codex_group_row(period, group, pricing, speed))
                .collect(),
            detected,
        });
    }

    let mut events = codex::load_codex_events(shared)?;
    let detected = !events.is_empty();
    codex::filter_events_by_date(&mut events, shared)?;
    let groups = codex::aggregate_events(&events, kind, shared.timezone.as_deref())?;
    let speed = codex::resolve_codex_speed(CodexSpeed::Auto);
    Ok(AgentRows {
        rows: groups
            .iter()
            .map(|(period, group)| codex_group_row(period, group, pricing, speed))
            .collect(),
        detected,
    })
}

fn load_priced_summary_agent_rows(
    agent: &'static str,
    kind: AgentReportKind,
    shared: &SharedArgs,
    pricing: &PricingMap,
    load_entries: impl FnOnce(&SharedArgs, &PricingMap) -> Result<Vec<LoadedEntry>>,
    summarize_entries: impl FnOnce(&[LoadedEntry], AgentReportKind) -> Result<Vec<UsageSummary>>,
) -> Result<AgentRows> {
    load_summary_agent_rows(
        agent,
        kind,
        shared,
        || load_entries(shared, pricing),
        summarize_entries,
    )
}

fn load_qwen_rows(kind: AgentReportKind, shared: &SharedArgs) -> Result<AgentRows> {
    let mut entries = qwen::load_entries(shared)?;
    let detected = !entries.is_empty() || qwen::has_data();
    if kind == AgentReportKind::Session {
        let mut summaries = qwen::summarize_entries(&entries, kind)?;
        filter_session_summaries(&mut summaries, shared);
        return Ok(AgentRows {
            rows: summary_rows("qwen", summaries),
            detected,
        });
    }
    filter_loaded_entries_by_date(&mut entries, shared);
    let summaries = qwen::summarize_entries(&entries, kind)?;
    Ok(AgentRows {
        rows: summary_rows("qwen", summaries),
        detected,
    })
}

fn summarize_entries(entries: &[LoadedEntry], kind: AgentReportKind) -> Result<Vec<UsageSummary>> {
    match kind {
        AgentReportKind::Daily => summarize_by_key(
            entries,
            |entry| entry.date.clone(),
            |date| (date.to_string(), None),
        ),
        AgentReportKind::Monthly => {
            let daily = summarize_entries(entries, AgentReportKind::Daily)?;
            Ok(summarize_summaries_by_bucket(
                &daily,
                BucketKind::Monthly,
                WeekDay::Sunday,
            ))
        }
        AgentReportKind::Weekly => {
            let daily = summarize_entries(entries, AgentReportKind::Daily)?;
            Ok(summarize_summaries_by_bucket(
                &daily,
                BucketKind::Weekly,
                WeekDay::Monday,
            ))
        }
        AgentReportKind::Session => summarize_by_key(
            entries,
            |entry| entry.session_id.to_string(),
            |session_id| (session_id.to_string(), None),
        )
        .map(|mut rows| {
            for row in &mut rows {
                row.session_id = row.date.take();
            }
            rows
        }),
    }
}

fn summarize_entry_sessions(
    entries: &[LoadedEntry],
    timezone: Option<&str>,
) -> Result<Vec<UsageSummary>> {
    let mut groups = BTreeMap::<(String, String), SessionAccumulator>::new();
    for entry in entries {
        groups
            .entry((entry.project_path.to_string(), entry.session_id.to_string()))
            .or_default()
            .add_entry(entry);
    }
    groups
        .into_values()
        .map(|group| group.into_summary(timezone))
        .collect()
}

fn filter_session_summaries(rows: &mut Vec<UsageSummary>, shared: &SharedArgs) {
    if shared.since.is_some() || shared.until.is_some() {
        rows.retain(|row| {
            let date = row
                .last_activity
                .as_deref()
                .unwrap_or_default()
                .replace('-', "");
            shared.since.as_ref().is_none_or(|since| &date >= since)
                && shared.until.as_ref().is_none_or(|until| &date <= until)
        });
    }
}

fn summary_rows(agent: &'static str, summaries: Vec<UsageSummary>) -> Vec<AllRow> {
    summaries
        .into_iter()
        .filter_map(|summary| {
            let period = summary
                .date
                .as_ref()
                .or(summary.week.as_ref())
                .or(summary.month.as_ref())
                .or(summary.session_id.as_ref())?
                .clone();
            let total_tokens = summary.total_tokens();
            if total_tokens == 0 {
                return None;
            }
            let metadata = summary_metadata(agent, &summary);
            Some(AllRow {
                period,
                agent,
                models_used: summary.models_used,
                input_tokens: summary.input_tokens,
                output_tokens: summary.output_tokens,
                cache_creation_tokens: summary.cache_creation_tokens,
                cache_read_tokens: summary.cache_read_tokens,
                total_tokens,
                total_cost: summary.total_cost,
                metadata,
                metadata_agents: Some(vec![agent]),
                agent_breakdowns: None,
                model_breakdowns: summary.model_breakdowns,
            })
        })
        .collect()
}

fn summary_metadata(agent: &'static str, summary: &UsageSummary) -> Option<Value> {
    let mut metadata = serde_json::Map::new();
    if let Some(credits) = summary.credits {
        metadata.insert("credits".to_string(), json_float(credits));
    }
    if summary.session_id.is_some() {
        if let Some(last_activity) = summary.last_activity.as_ref() {
            metadata.insert("lastActivity".to_string(), json!(last_activity));
        }
        if agent == "pi" {
            if let Some(project_path) = summary.project_path.as_ref() {
                metadata.insert("projectPath".to_string(), json!(project_path));
            }
        }
    }
    if metadata.is_empty() {
        None
    } else {
        Some(Value::Object(metadata))
    }
}

pub(super) fn codex_group_row(
    period: &str,
    group: &CodexGroup,
    pricing: &PricingMap,
    speed: CodexSpeed,
) -> AllRow {
    let mut model_breakdowns: Vec<ModelBreakdown> = group
        .models
        .iter()
        .map(|(model, usage)| {
            let input = codex::non_cached_input_tokens(
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_write_tokens,
            );
            ModelBreakdown {
                model_name: model.clone(),
                input_tokens: input,
                output_tokens: usage.output_tokens,
                cache_creation_tokens: usage.cache_write_tokens,
                cache_read_tokens: usage.cached_input_tokens,
                extra_total_tokens: 0,
                cost: codex::calculate_codex_model_cost(model, usage, pricing, speed),
                missing_pricing: codex::codex_model_missing_pricing(model, usage, pricing),
            }
        })
        .collect();
    model_breakdowns.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    AllRow {
        period: period.to_string(),
        agent: "codex",
        models_used: group.models.keys().cloned().collect(),
        input_tokens: codex::non_cached_input_tokens(
            group.input_tokens,
            group.cached_input_tokens,
            group.cache_write_tokens,
        ),
        output_tokens: group.output_tokens,
        cache_creation_tokens: group.cache_write_tokens,
        cache_read_tokens: group.cached_input_tokens,
        total_tokens: group.total_tokens,
        total_cost: codex::calculate_group_cost(group, pricing, speed),
        metadata: Some(json!({
            "lastActivity": group.last_activity,
            "reasoningOutputTokens": group.reasoning_output_tokens,
        })),
        metadata_agents: Some(vec!["codex"]),
        agent_breakdowns: None,
        model_breakdowns,
    }
}

pub(super) fn aggregate_rows(rows: Vec<AllRow>, kind: AgentReportKind) -> Vec<AllRow> {
    let mut groups = BTreeMap::<String, AllAccumulator>::new();
    for mut row in rows {
        let period = match kind {
            AgentReportKind::Daily => row.period.clone(),
            AgentReportKind::Monthly => row
                .period
                .get(..7)
                .map_or_else(|| row.period.clone(), str::to_string),
            AgentReportKind::Weekly => crate::week_start(&row.period, WeekDay::Monday)
                .unwrap_or_else(|| row.period.clone()),
            AgentReportKind::Session => row.period.clone(),
        };
        row.period = period.clone();
        groups.entry(period).or_default().add(row);
    }
    groups
        .into_iter()
        .map(|(period, group)| group.into_row(period))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_summary(date: &str, input_tokens: u64) -> UsageSummary {
        UsageSummary {
            date: Some(date.to_string()),
            month: None,
            week: None,
            session_id: None,
            project_path: None,
            last_activity: None,
            input_tokens,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            extra_total_tokens: 0,
            total_cost: 0.0,
            credits: None,
            message_count: None,
            models_used: Vec::new(),
            model_breakdowns: Vec::new(),
            project: None,
            versions: None,
        }
    }

    #[test]
    fn filters_daily_summaries_with_compact_date_bounds() {
        let mut rows = vec![
            usage_summary("2026-01-01", 10),
            usage_summary("2026-01-02", 20),
            usage_summary("2026-01-03", 30),
        ];
        let shared = SharedArgs {
            since: Some("20260102".to_string()),
            until: Some("20260102".to_string()),
            ..SharedArgs::default()
        };

        filter_daily_summaries_by_date(&mut rows, &shared);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].date.as_deref(), Some("2026-01-02"));
        assert_eq!(rows[0].input_tokens, 20);
    }
}
