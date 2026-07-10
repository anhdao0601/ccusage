use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::{
    cli::{AgentReportKind, CodexSpeed, SharedArgs},
    color, format_currency, format_models_multiline, format_number, json_float,
    missing_pricing_model_for_token_total, print_box_title,
    print_missing_pricing_warnings_for_models, Align, CodexGroup, CodexModelUsage, Color,
    PricingMap, Result, SimpleTable,
};

pub(super) fn report_from_groups(
    groups: &BTreeMap<String, CodexGroup>,
    kind: AgentReportKind,
    pricing: &PricingMap,
    speed: CodexSpeed,
) -> Value {
    let rows = groups
        .iter()
        .map(|(period, group)| group_json(period, group, kind, pricing, speed))
        .collect::<Vec<_>>();
    let totals = totals_json(groups.values(), pricing, speed);
    json!({
        rows_key(kind): rows,
        "totals": totals,
    })
}

fn rows_key(kind: AgentReportKind) -> &'static str {
    match kind {
        AgentReportKind::Daily => "daily",
        AgentReportKind::Weekly => "weekly",
        AgentReportKind::Monthly => "monthly",
        AgentReportKind::Session => "sessions",
    }
}

fn period_key(kind: AgentReportKind) -> &'static str {
    match kind {
        AgentReportKind::Daily => "date",
        AgentReportKind::Weekly => "week",
        AgentReportKind::Monthly => "month",
        AgentReportKind::Session => "sessionId",
    }
}

fn group_json(
    period: &str,
    group: &CodexGroup,
    kind: AgentReportKind,
    pricing: &PricingMap,
    speed: CodexSpeed,
) -> Value {
    let cost = calculate_group_cost(group, pricing, speed);
    let input_tokens = non_cached_input_tokens(
        group.input_tokens,
        group.cached_input_tokens,
        group.cache_write_tokens,
    );
    let models = group
        .models
        .iter()
        .map(|(model, usage)| (model.clone(), model_usage_json(usage)))
        .collect::<BTreeMap<_, _>>();
    let mut row = json!({
        period_key(kind): period,
        "inputTokens": input_tokens,
        "cachedInputTokens": group.cached_input_tokens,
        "cacheWriteInputTokens": group.cache_write_tokens,
        "outputTokens": group.output_tokens,
        "reasoningOutputTokens": group.reasoning_output_tokens,
        "totalTokens": group.total_tokens,
        "costUSD": json_float(cost),
        "models": models,
    });
    if kind == AgentReportKind::Session {
        row["lastActivity"] = json!(group.last_activity);
        let separator = period.rfind('/');
        row["sessionFile"] = json!(separator.map_or(period, |index| &period[index + 1..]));
        row["directory"] = json!(separator.map_or("", |index| &period[..index]));
    }
    row
}

pub(crate) fn non_cached_input_tokens(
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_tokens: u64,
) -> u64 {
    input_tokens
        .saturating_sub(cached_input_tokens)
        .saturating_sub(cache_write_tokens)
}

fn model_usage_json(usage: &CodexModelUsage) -> Value {
    json!({
        "inputTokens": non_cached_input_tokens(
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_write_tokens,
        ),
        "cachedInputTokens": usage.cached_input_tokens,
        "cacheWriteInputTokens": usage.cache_write_tokens,
        "outputTokens": usage.output_tokens,
        "reasoningOutputTokens": usage.reasoning_output_tokens,
        "totalTokens": usage.total_tokens,
        "isFallback": usage.is_fallback,
    })
}

fn totals_json<'a>(
    groups: impl Iterator<Item = &'a CodexGroup>,
    pricing: &PricingMap,
    speed: CodexSpeed,
) -> Value {
    let mut input = 0;
    let mut cached = 0;
    let mut cache_write = 0;
    let mut output = 0;
    let mut reasoning = 0;
    let mut total = 0;
    let mut cost = 0.0;
    for group in groups {
        input += non_cached_input_tokens(
            group.input_tokens,
            group.cached_input_tokens,
            group.cache_write_tokens,
        );
        cached += group.cached_input_tokens;
        cache_write += group.cache_write_tokens;
        output += group.output_tokens;
        reasoning += group.reasoning_output_tokens;
        total += group.total_tokens;
        cost += calculate_group_cost(group, pricing, speed);
    }
    json!({
        "inputTokens": input,
        "cachedInputTokens": cached,
        "cacheWriteInputTokens": cache_write,
        "outputTokens": output,
        "reasoningOutputTokens": reasoning,
        "totalTokens": total,
        "costUSD": json_float(cost),
    })
}

pub(crate) fn calculate_codex_model_cost(
    model: &str,
    usage: &CodexModelUsage,
    pricing: &PricingMap,
    speed: CodexSpeed,
) -> f64 {
    let Some(pricing) = pricing.find(model) else {
        return 0.0;
    };
    let non_cached_input = non_cached_input_tokens(
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_tokens,
    );
    let multiplier = if matches!(speed, CodexSpeed::Fast) {
        if pricing.fast_multiplier == 1.0 {
            2.0
        } else {
            pricing.fast_multiplier
        }
    } else {
        1.0
    };
    let cache_read = if pricing.cache_read_explicit {
        pricing.cache_read
    } else {
        pricing.input
    };
    (non_cached_input as f64 * pricing.input
        + usage.cache_write_tokens as f64 * pricing.cache_create
        + usage.cached_input_tokens as f64 * cache_read
        + usage.output_tokens as f64 * pricing.output)
        * multiplier
}

pub(crate) fn calculate_group_cost(
    group: &CodexGroup,
    pricing: &PricingMap,
    speed: CodexSpeed,
) -> f64 {
    group
        .models
        .iter()
        .map(|(model, usage)| calculate_codex_model_cost(model, usage, pricing, speed))
        .sum()
}

pub(crate) fn codex_model_missing_pricing(
    model: &str,
    usage: &CodexModelUsage,
    pricing: &PricingMap,
) -> bool {
    missing_pricing_model_for_token_total(
        Some(model),
        usage
            .total_tokens
            .max(usage.input_tokens.saturating_add(usage.output_tokens)),
        Some(pricing),
    )
    .is_some()
}

pub(crate) fn codex_missing_pricing_models(
    groups: &BTreeMap<String, CodexGroup>,
    pricing: &PricingMap,
) -> Vec<String> {
    let mut models = BTreeSet::new();
    for group in groups.values() {
        for (model, usage) in &group.models {
            if codex_model_missing_pricing(model, usage, pricing) {
                models.insert(model.clone());
            }
        }
    }
    models.into_iter().collect()
}

pub(super) fn print_table_from_groups(
    groups: &BTreeMap<String, CodexGroup>,
    kind: AgentReportKind,
    pricing: &PricingMap,
    speed: CodexSpeed,
    shared: &SharedArgs,
) -> Result<()> {
    if groups.is_empty() {
        eprintln!("No Codex usage data found.");
        return Ok(());
    }
    let first_column = match kind {
        AgentReportKind::Daily => "Date",
        AgentReportKind::Weekly => "Week",
        AgentReportKind::Monthly => "Month",
        AgentReportKind::Session => "Session",
    };
    print_box_title(
        &format!(
            "Codex Token Usage Report - {}",
            match kind {
                AgentReportKind::Daily => "Daily",
                AgentReportKind::Weekly => "Weekly",
                AgentReportKind::Monthly => "Monthly",
                AgentReportKind::Session => "Session",
            }
        ),
        shared,
    );
    if kind == AgentReportKind::Session {
        return print_codex_session_table(groups, pricing, speed, shared);
    }
    let mut table = SimpleTable::new(
        vec![
            first_column,
            "Models",
            "Input",
            "Output",
            "Reasoning",
            "Cache Write",
            "Cache Read",
            "Total Tokens",
            "Cost (USD)",
        ],
        vec![
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        crate::terminal_style(shared),
    )
    .with_terminal_width(crate::terminal_width())
    .with_date_compaction(true);
    let mut total_input = 0;
    let mut total_cached = 0;
    let mut total_cache_write = 0;
    let mut total_output = 0;
    let mut total_reasoning = 0;
    let mut total_tokens = 0;
    let mut total_cost = 0.0;
    for (label, group) in groups {
        let input_tokens = non_cached_input_tokens(
            group.input_tokens,
            group.cached_input_tokens,
            group.cache_write_tokens,
        );
        let cost = calculate_group_cost(group, pricing, speed);
        total_input += input_tokens;
        total_cached += group.cached_input_tokens;
        total_cache_write += group.cache_write_tokens;
        total_output += group.output_tokens;
        total_reasoning += group.reasoning_output_tokens;
        total_tokens += group.total_tokens;
        total_cost += cost;
        let models = format_models_multiline(&group.models.keys().cloned().collect::<Vec<_>>());
        table.push(vec![
            label.clone(),
            models,
            format_number(input_tokens),
            format_number(group.output_tokens),
            format_number(group.reasoning_output_tokens),
            format_number(group.cache_write_tokens),
            format_number(group.cached_input_tokens),
            format_number(group.total_tokens),
            format_currency(cost),
        ]);
    }
    table.separator();
    table.push(vec![
        color(shared, "Total", Color::Yellow),
        String::new(),
        color(shared, format_number(total_input), Color::Yellow),
        color(shared, format_number(total_output), Color::Yellow),
        color(shared, format_number(total_reasoning), Color::Yellow),
        color(shared, format_number(total_cache_write), Color::Yellow),
        color(shared, format_number(total_cached), Color::Yellow),
        color(shared, format_number(total_tokens), Color::Yellow),
        color(shared, format_currency(total_cost), Color::Yellow),
    ]);
    table.print()?;
    let missing_models = codex_missing_pricing_models(groups, pricing);
    print_missing_pricing_warnings_for_models(
        missing_models.iter().map(String::as_str),
        shared.offline,
    );
    Ok(())
}

fn print_codex_session_table(
    groups: &BTreeMap<String, CodexGroup>,
    pricing: &PricingMap,
    speed: CodexSpeed,
    shared: &SharedArgs,
) -> Result<()> {
    let mut table = SimpleTable::new(
        vec![
            "Date",
            "Directory",
            "Session",
            "Models",
            "Input",
            "Output",
            "Reasoning",
            "Cache Write",
            "Cache Read",
            "Total Tokens",
            "Cost (USD)",
            "Last Activity",
        ],
        vec![
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
        ],
        crate::terminal_style(shared),
    )
    .with_terminal_width(crate::terminal_width());
    let mut total_input = 0;
    let mut total_output = 0;
    let mut total_reasoning = 0;
    let mut total_cache_write = 0;
    let mut total_cached = 0;
    let mut total_tokens = 0;
    let mut total_cost = 0.0;
    for (period, group) in groups {
        let cost = calculate_group_cost(group, pricing, speed);
        let input_tokens = non_cached_input_tokens(
            group.input_tokens,
            group.cached_input_tokens,
            group.cache_write_tokens,
        );
        let (directory, file_name) = period.rfind('/').map_or(("", period.as_str()), |index| {
            (&period[..index], &period[index + 1..])
        });
        let last_activity = group.last_activity.clone().unwrap_or_default();
        let date = last_activity.get(..10).unwrap_or("").to_string();
        table.push(vec![
            date,
            directory.to_string(),
            short_session_label(file_name),
            format_models_multiline(&group.models.keys().cloned().collect::<Vec<_>>()),
            format_number(input_tokens),
            format_number(group.output_tokens),
            format_number(group.reasoning_output_tokens),
            format_number(group.cache_write_tokens),
            format_number(group.cached_input_tokens),
            format_number(group.total_tokens),
            format_currency(cost),
            last_activity,
        ]);
        total_input += input_tokens;
        total_output += group.output_tokens;
        total_reasoning += group.reasoning_output_tokens;
        total_cache_write += group.cache_write_tokens;
        total_cached += group.cached_input_tokens;
        total_tokens += group.total_tokens;
        total_cost += cost;
    }
    table.separator();
    table.push(vec![
        color(shared, "Total", Color::Yellow),
        String::new(),
        String::new(),
        String::new(),
        color(shared, format_number(total_input), Color::Yellow),
        color(shared, format_number(total_output), Color::Yellow),
        color(shared, format_number(total_reasoning), Color::Yellow),
        color(shared, format_number(total_cache_write), Color::Yellow),
        color(shared, format_number(total_cached), Color::Yellow),
        color(shared, format_number(total_tokens), Color::Yellow),
        color(shared, format_currency(total_cost), Color::Yellow),
        String::new(),
    ]);
    table.print()?;
    let missing_models = codex_missing_pricing_models(groups, pricing);
    print_missing_pricing_warnings_for_models(
        missing_models.iter().map(String::as_str),
        shared.offline,
    );
    Ok(())
}

/// Shortens a Codex session file stem to its last 8 characters with a `...`
/// prefix, mirroring the TS Codex session table.
fn short_session_label(file_name: &str) -> String {
    let stem = file_name.strip_suffix(".jsonl").unwrap_or(file_name);
    let chars = stem.chars().collect::<Vec<_>>();
    if chars.len() > 8 {
        let tail = chars[chars.len() - 8..].iter().collect::<String>();
        format!("...{tail}")
    } else {
        stem.to_string()
    }
}
