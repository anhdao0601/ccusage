use std::sync::Arc;

use jiff::tz::TimeZone as JiffTimeZone;

use crate::{
    calculate_cost_for_usage, cli::CostMode, format_date_tz, format_rfc3339_millis,
    missing_pricing_model_for_candidates, LoadedEntry, PricingMap, TimestampMs, TokenUsageRaw,
    UsageEntry, UsageMessage,
};

pub(super) struct ZCodeEntry {
    id: String,
    session_id: String,
    provider_id: String,
    model_id: String,
    timestamp: TimestampMs,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    project_path: String,
    version: String,
}

pub(super) fn read_model_usage_row(statement: &sqlite::Statement<'_>) -> Option<ZCodeEntry> {
    let id = statement.read::<String, _>(0).ok()?;
    let session_id = statement.read::<String, _>(1).ok()?;
    let provider_id = statement.read::<String, _>(2).ok()?;
    let model_id = statement.read::<String, _>(3).ok()?;
    let started_at = statement.read::<i64, _>(4).ok()?;
    let timestamp = (started_at > 0).then(|| TimestampMs::from_millis(started_at))?;
    Some(ZCodeEntry {
        id,
        session_id,
        provider_id,
        model_id,
        timestamp,
        input_tokens: read_u64(statement, 5),
        output_tokens: read_u64(statement, 6),
        cache_creation_input_tokens: read_u64(statement, 7),
        cache_read_input_tokens: read_u64(statement, 8),
        project_path: statement.read::<String, _>(9).ok()?,
        version: statement.read::<String, _>(10).ok()?,
    })
}

fn read_u64(statement: &sqlite::Statement<'_>, index: usize) -> u64 {
    statement
        .read::<i64, _>(index)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

pub(super) fn to_loaded_entry(
    entry: ZCodeEntry,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: &PricingMap,
) -> LoadedEntry {
    let cached_input_tokens = entry
        .cache_creation_input_tokens
        .saturating_add(entry.cache_read_input_tokens);
    let usage = TokenUsageRaw {
        input_tokens: entry.input_tokens.saturating_sub(cached_input_tokens),
        output_tokens: entry.output_tokens,
        cache_creation_input_tokens: entry.cache_creation_input_tokens,
        cache_read_input_tokens: entry.cache_read_input_tokens,
        speed: None,
    };
    let candidates = model_candidates(&entry.provider_id, &entry.model_id);
    let cost = candidates
        .iter()
        .find_map(|candidate| {
            pricing.find(candidate).map(|_| {
                calculate_cost_for_usage(Some(candidate), usage, None, mode, Some(pricing))
            })
        })
        .unwrap_or(0.0);
    let missing_pricing_model = (mode != CostMode::Display)
        .then(|| {
            missing_pricing_model_for_candidates(
                &entry.model_id,
                candidates,
                crate::total_usage_tokens(usage),
                Some(pricing),
            )
        })
        .flatten();
    let timestamp_text = format_rfc3339_millis(entry.timestamp);
    let data = UsageEntry {
        session_id: Some(entry.session_id.clone()),
        timestamp: timestamp_text,
        version: Some(entry.version),
        message: UsageMessage {
            usage,
            model: Some(entry.model_id.clone()),
            id: Some(entry.id),
        },
        cost_usd: None,
        request_id: None,
        is_api_error_message: None,
        is_sidechain: None,
    };
    LoadedEntry {
        date: format_date_tz(entry.timestamp, tz),
        timestamp: entry.timestamp,
        project: Arc::from("zcode"),
        session_id: Arc::from(entry.session_id),
        project_path: Arc::from(entry.project_path),
        cost,
        credits: None,
        extra_total_tokens: 0,
        message_count: Some(1),
        model: Some(entry.model_id),
        usage_limit_reset_time: None,
        missing_pricing_model,
        data,
    }
}

fn model_candidates(provider_id: &str, model_id: &str) -> Vec<String> {
    vec![format!("{provider_id}/{model_id}"), model_id.to_string()]
}
