use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::json;

use super::*;
use crate::{
    cli::{AgentReportKind, CodexSpeed},
    Align, CodexGroup, CodexModelUsage, ModelBreakdown, PricingMap,
};

fn test_agent_rows(agent: &'static str) -> AgentRows {
    AgentRows {
        rows: vec![AllRow {
            period: "2026-01-02".to_string(),
            agent,
            models_used: Vec::new(),
            input_tokens: 1,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 1,
            total_cost: 0.0,
            metadata: None,
            metadata_agents: Some(vec![agent]),
            agent_breakdowns: None,
            model_breakdowns: Vec::new(),
        }],
        detected: true,
    }
}

#[test]
fn caches_agent_rows_per_agent_and_invalidates_on_source_change() {
    struct EnvRestore {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }
    impl EnvRestore {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    let _cache_guard = crate::pricing_cache::XDG_CACHE_HOME_LOCK.lock().unwrap();
    let fixture = ccusage_test_support::fs_fixture!({
        "gemini/chats/session-a.json": "{}",
    });
    let _cache_env = EnvRestore::set_path("XDG_CACHE_HOME", &fixture.path("cache"));
    let sources = || {
        vec![crate::report_cache::recursive_extensions(
            "gemini:test".to_string(),
            fixture.path("gemini"),
            &["json", "jsonl"],
        )]
    };
    let shared = crate::cli::SharedArgs {
        offline: true,
        json: true,
        ..crate::cli::SharedArgs::default()
    };
    // A cost whose shortest JSON representation stresses float parsing: the
    // cached row must come back bit-identical, not merely approximately equal.
    let awkward_cost = 0.057_322_649_999_999_996_f64;
    let cached_rows = || {
        let mut rows = test_agent_rows("gemini");
        rows.rows[0].total_cost = awkward_cost;
        rows
    };
    let mut loads = 0;

    let first = loader::load_cached_agent_rows(
        "gemini",
        sources(),
        AgentReportKind::Daily,
        &shared,
        || {
            loads += 1;
            Ok(cached_rows())
        },
    )
    .unwrap();
    let second = loader::load_cached_agent_rows(
        "gemini",
        sources(),
        AgentReportKind::Daily,
        &shared,
        || {
            loads += 1;
            Ok(cached_rows())
        },
    )
    .unwrap();

    assert_eq!(loads, 1, "unchanged gemini sources must reuse the cache");
    assert!(second.detected);
    assert_eq!(second.rows.len(), 1);
    assert_eq!(second.rows[0].period, first.rows[0].period);
    assert_eq!(second.rows[0].agent, "gemini");
    assert_eq!(second.rows[0].input_tokens, first.rows[0].input_tokens);
    assert_eq!(second.rows[0].metadata_agents, Some(vec!["gemini"]));
    assert_eq!(
        second.rows[0].total_cost.to_bits(),
        awkward_cost.to_bits(),
        "cached costs must round-trip bit-identically"
    );

    let _ = fixture.write_file("gemini/chats/session-b.json", "{}");
    loader::load_cached_agent_rows("gemini", sources(), AgentReportKind::Daily, &shared, || {
        loads += 1;
        Ok(test_agent_rows("gemini"))
    })
    .unwrap();

    assert_eq!(loads, 2, "changed gemini sources must invalidate the cache");
}

#[test]
fn loads_agent_rows_concurrently() {
    let active_loaders = Arc::new(AtomicUsize::new(0));
    let specs = [
        ("claude", crate::progress::UsageLoadAgent::Claude),
        ("codex", crate::progress::UsageLoadAgent::Codex),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (agent, progress_agent))| {
        let active_loaders = Arc::clone(&active_loaders);
        AgentLoadSpec {
            index,
            agent,
            progress_agent,
            load: Box::new(move || {
                active_loaders.fetch_add(1, Ordering::AcqRel);
                let started = Instant::now();
                while active_loaders.load(Ordering::Acquire) < 2 {
                    if started.elapsed() > Duration::from_secs(1) {
                        return Err(crate::cli_error("agent loaders did not overlap"));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(test_agent_rows(agent))
            }),
        }
    })
    .collect();
    let mut progress = crate::progress::UsageLoadProgress::new(false);

    let loaded = load_agent_rows_parallel(specs, &mut progress).unwrap();

    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].agent, "claude");
    assert_eq!(loaded[1].agent, "codex");
}

#[test]
fn aggregates_daily_agent_rows_by_period() {
    let rows = aggregate_rows(
        vec![
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "codex",
                models_used: vec!["gpt-5".to_string()],
                input_tokens: 100,
                output_tokens: 20,
                cache_creation_tokens: 0,
                cache_read_tokens: 10,
                total_tokens: 120,
                total_cost: 0.01,
                metadata: None,
                metadata_agents: Some(vec!["codex"]),
                agent_breakdowns: None,
                model_breakdowns: Vec::new(),
            },
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "claude",
                models_used: vec!["claude-sonnet-4-20250514".to_string()],
                input_tokens: 50,
                output_tokens: 25,
                cache_creation_tokens: 5,
                cache_read_tokens: 3,
                total_tokens: 83,
                total_cost: 0.02,
                metadata: None,
                metadata_agents: Some(vec!["claude"]),
                agent_breakdowns: None,
                model_breakdowns: Vec::new(),
            },
        ],
        AgentReportKind::Daily,
    );

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].period, "2026-01-02");
    assert_eq!(rows[0].agent, "all");
    assert_eq!(rows[0].input_tokens, 150);
    assert_eq!(rows[0].output_tokens, 45);
    assert_eq!(rows[0].cache_read_tokens, 13);
    assert_eq!(rows[0].total_tokens, 203);
    assert_eq!(
        rows[0].models_used,
        vec!["claude-sonnet-4-20250514".to_string(), "gpt-5".to_string()]
    );
    assert_eq!(rows[0].metadata_agents, Some(vec!["claude", "codex"]));
    let breakdowns = rows[0].agent_breakdowns.as_ref().unwrap();
    assert_eq!(breakdowns.len(), 2);
    assert_eq!(breakdowns[0].agent, "claude");
    assert_eq!(breakdowns[0].period, "2026-01-02");
    assert_eq!(breakdowns[1].agent, "codex");
}

#[test]
fn merges_same_agent_daily_rows_into_one_monthly_breakdown() {
    let rows = aggregate_rows(
        vec![
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "claude",
                models_used: vec!["claude-sonnet-4-20250514".to_string()],
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: 1,
                cache_read_tokens: 2,
                total_tokens: 18,
                total_cost: 0.01,
                metadata: None,
                metadata_agents: Some(vec!["claude"]),
                agent_breakdowns: None,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "claude-sonnet-4-20250514".to_string(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_tokens: 1,
                    cache_read_tokens: 2,
                    cost: 0.01,
                    ..ModelBreakdown::default()
                }],
            },
            AllRow {
                period: "2026-01-15".to_string(),
                agent: "claude",
                models_used: vec!["claude-opus-4-20250514".to_string()],
                input_tokens: 20,
                output_tokens: 10,
                cache_creation_tokens: 2,
                cache_read_tokens: 4,
                total_tokens: 36,
                total_cost: 0.05,
                metadata: None,
                metadata_agents: Some(vec!["claude"]),
                agent_breakdowns: None,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "claude-opus-4-20250514".to_string(),
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_creation_tokens: 2,
                    cache_read_tokens: 4,
                    cost: 0.05,
                    ..ModelBreakdown::default()
                }],
            },
            AllRow {
                period: "2026-01-20".to_string(),
                agent: "codex",
                models_used: vec!["gpt-5".to_string()],
                input_tokens: 30,
                output_tokens: 15,
                cache_creation_tokens: 0,
                cache_read_tokens: 6,
                total_tokens: 51,
                total_cost: 0.02,
                metadata: None,
                metadata_agents: Some(vec!["codex"]),
                agent_breakdowns: None,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "gpt-5".to_string(),
                    input_tokens: 30,
                    output_tokens: 15,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 6,
                    cost: 0.02,
                    ..ModelBreakdown::default()
                }],
            },
        ],
        AgentReportKind::Monthly,
    );

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].period, "2026-01");
    assert_eq!(rows[0].input_tokens, 60);
    assert_eq!(rows[0].output_tokens, 30);
    let breakdowns = rows[0].agent_breakdowns.as_ref().unwrap();
    assert_eq!(
        breakdowns.len(),
        2,
        "expected one breakdown row per agent per month, got {breakdowns:#?}"
    );
    let claude = breakdowns
        .iter()
        .find(|row| row.agent == "claude")
        .expect("claude breakdown present");
    assert_eq!(claude.period, "2026-01");
    assert_eq!(claude.input_tokens, 30);
    assert_eq!(claude.output_tokens, 15);
    assert_eq!(claude.cache_creation_tokens, 3);
    assert_eq!(claude.cache_read_tokens, 6);
    assert_eq!(
        claude.models_used,
        vec![
            "claude-opus-4-20250514".to_string(),
            "claude-sonnet-4-20250514".to_string(),
        ]
    );
    assert_eq!(claude.model_breakdowns.len(), 2);
    assert_eq!(
        claude
            .model_breakdowns
            .iter()
            .map(|breakdown| breakdown.model_name.as_str())
            .collect::<Vec<_>>(),
        vec!["claude-opus-4-20250514", "claude-sonnet-4-20250514",]
    );
    let codex = breakdowns
        .iter()
        .find(|row| row.agent == "codex")
        .expect("codex breakdown present");
    assert_eq!(codex.input_tokens, 30);
}

#[test]
fn aggregates_rows_by_provider_without_time_buckets() {
    let rows = aggregate_rows_by_provider(vec![
        AllRow {
            period: "2026-01-02".to_string(),
            agent: "claude",
            models_used: vec!["claude-sonnet-4-20250514".to_string()],
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: 1,
            cache_read_tokens: 2,
            total_tokens: 18,
            total_cost: 0.01,
            metadata: None,
            metadata_agents: Some(vec!["claude"]),
            agent_breakdowns: None,
            model_breakdowns: vec![ModelBreakdown {
                model_name: "claude-sonnet-4-20250514".to_string(),
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: 1,
                cache_read_tokens: 2,
                cost: 0.01,
                ..ModelBreakdown::default()
            }],
        },
        AllRow {
            period: "2026-01-03".to_string(),
            agent: "claude",
            models_used: vec!["claude-opus-4-20250514".to_string()],
            input_tokens: 20,
            output_tokens: 10,
            cache_creation_tokens: 2,
            cache_read_tokens: 4,
            total_tokens: 36,
            total_cost: 0.05,
            metadata: None,
            metadata_agents: Some(vec!["claude"]),
            agent_breakdowns: None,
            model_breakdowns: vec![ModelBreakdown {
                model_name: "claude-opus-4-20250514".to_string(),
                input_tokens: 20,
                output_tokens: 10,
                cache_creation_tokens: 2,
                cache_read_tokens: 4,
                cost: 0.05,
                ..ModelBreakdown::default()
            }],
        },
        AllRow {
            period: "2026-01-02".to_string(),
            agent: "codex",
            models_used: vec!["gpt-5".to_string()],
            input_tokens: 30,
            output_tokens: 15,
            cache_creation_tokens: 0,
            cache_read_tokens: 6,
            total_tokens: 51,
            total_cost: 0.02,
            metadata: None,
            metadata_agents: Some(vec!["codex"]),
            agent_breakdowns: None,
            model_breakdowns: vec![ModelBreakdown {
                model_name: "gpt-5".to_string(),
                input_tokens: 30,
                output_tokens: 15,
                cache_creation_tokens: 0,
                cache_read_tokens: 6,
                cost: 0.02,
                ..ModelBreakdown::default()
            }],
        },
    ]);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].agent, "claude");
    assert_eq!(rows[0].period, "all");
    assert_eq!(rows[0].input_tokens, 30);
    assert_eq!(rows[0].output_tokens, 15);
    assert_eq!(rows[0].cache_creation_tokens, 3);
    assert_eq!(rows[0].cache_read_tokens, 6);
    assert_eq!(rows[0].total_tokens, 54);
    assert_eq!(rows[0].metadata_agents, Some(vec!["claude"]));
    assert_eq!(
        rows[0].models_used,
        vec![
            "claude-opus-4-20250514".to_string(),
            "claude-sonnet-4-20250514".to_string(),
        ]
    );
    assert_eq!(rows[0].model_breakdowns.len(), 2);
    assert_eq!(rows[1].agent, "codex");
    assert_eq!(rows[1].period, "all");
    assert_eq!(rows[1].total_tokens, 51);
}

#[test]
fn renders_all_report_json_with_period_and_agent_metadata() {
    let rows = vec![AllRow {
        period: "2026-01-02".to_string(),
        agent: "all",
        models_used: vec!["gpt-5".to_string()],
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_tokens: 0,
        cache_read_tokens: 10,
        total_tokens: 130,
        total_cost: 0.01,
        metadata: None,
        metadata_agents: Some(vec!["codex"]),
        agent_breakdowns: None,
        model_breakdowns: Vec::new(),
    }];

    let report = report_json(&rows, AgentReportKind::Daily);

    assert_eq!(report["daily"][0]["period"], "2026-01-02");
    assert_eq!(report["daily"][0]["date"], "2026-01-02");
    assert_eq!(report["daily"][0]["agent"], "all");
    assert_eq!(report["daily"][0]["source"], "all");
    assert_eq!(report["daily"][0]["metadata"]["agents"], json!(["codex"]));
    assert_eq!(report["totals"]["totalTokens"], 130);
    assert_eq!(report["totalsBySource"]["all"]["totalTokens"], 130);
}

#[test]
fn renders_all_report_json_totals_by_source_from_agent_breakdowns() {
    let rows = vec![AllRow {
        period: "2026-01-02".to_string(),
        agent: "all",
        models_used: vec!["claude-sonnet-4-20250514".to_string(), "gpt-5".to_string()],
        input_tokens: 30,
        output_tokens: 15,
        cache_creation_tokens: 1,
        cache_read_tokens: 2,
        total_tokens: 48,
        total_cost: 0.03,
        metadata: None,
        metadata_agents: Some(vec!["claude", "codex"]),
        agent_breakdowns: Some(vec![
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "claude",
                models_used: vec!["claude-sonnet-4-20250514".to_string()],
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: 1,
                cache_read_tokens: 2,
                total_tokens: 18,
                total_cost: 0.01,
                metadata: None,
                metadata_agents: Some(vec!["claude"]),
                agent_breakdowns: None,
                model_breakdowns: Vec::new(),
            },
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "codex",
                models_used: vec!["gpt-5".to_string()],
                input_tokens: 20,
                output_tokens: 10,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_tokens: 30,
                total_cost: 0.02,
                metadata: None,
                metadata_agents: Some(vec!["codex"]),
                agent_breakdowns: None,
                model_breakdowns: Vec::new(),
            },
        ]),
        model_breakdowns: Vec::new(),
    }];

    let report = report_json(&rows, AgentReportKind::Daily);

    assert_eq!(report["totalsBySource"]["claude"]["totalTokens"], 18);
    assert_eq!(report["totalsBySource"]["codex"]["totalTokens"], 30);
}

#[test]
fn maps_codex_cache_reads_and_writes_into_all_rows() {
    let mut group = CodexGroup {
        input_tokens: 100,
        cached_input_tokens: 20,
        cache_write_tokens: 60,
        output_tokens: 5,
        total_tokens: 105,
        ..CodexGroup::default()
    };
    group.models.insert(
        "gpt-5".to_string(),
        CodexModelUsage {
            input_tokens: 100,
            cached_input_tokens: 20,
            cache_write_tokens: 60,
            output_tokens: 5,
            total_tokens: 105,
            ..CodexModelUsage::default()
        },
    );
    let row = codex_group_row(
        "2026-01-02",
        &group,
        &PricingMap::default(),
        CodexSpeed::Standard,
    );

    assert_eq!(row.input_tokens, 20);
    assert_eq!(row.cache_creation_tokens, 60);
    assert_eq!(row.cache_read_tokens, 20);
    assert_eq!(row.total_tokens, 105);
}

#[test]
fn includes_codex_model_breakdowns_in_all_rows() {
    let mut pricing = PricingMap::default();
    pricing.load_json(
        r#"{
            "gpt-5": {
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000010,
                "cache_read_input_token_cost": 0.0000001
            },
            "gpt-5-mini": {
                "input_cost_per_token": 0.0000001,
                "output_cost_per_token": 0.000001,
                "cache_read_input_token_cost": 0.00000001
            }
        }"#,
    );
    let mut group = CodexGroup {
        input_tokens: 300,
        cached_input_tokens: 100,
        output_tokens: 50,
        total_tokens: 350,
        ..CodexGroup::default()
    };
    group.models.insert(
        "gpt-5-mini".to_string(),
        CodexModelUsage {
            input_tokens: 100,
            cached_input_tokens: 20,
            output_tokens: 10,
            total_tokens: 110,
            ..CodexModelUsage::default()
        },
    );
    group.models.insert(
        "gpt-5".to_string(),
        CodexModelUsage {
            input_tokens: 200,
            cached_input_tokens: 80,
            output_tokens: 40,
            total_tokens: 240,
            ..CodexModelUsage::default()
        },
    );

    let row = codex_group_row("2026-01-02", &group, &pricing, CodexSpeed::Standard);

    assert_eq!(row.model_breakdowns.len(), 2);
    assert_eq!(row.model_breakdowns[0].model_name, "gpt-5");
    assert_eq!(row.model_breakdowns[0].input_tokens, 120);
    assert_eq!(row.model_breakdowns[0].cache_read_tokens, 80);
    assert_eq!(row.model_breakdowns[0].output_tokens, 40);
    assert_eq!(row.model_breakdowns[1].model_name, "gpt-5-mini");
}

#[test]
fn aggregates_model_breakdowns_across_agents() {
    let rows = aggregate_rows(
        vec![
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "codex",
                models_used: vec!["gpt-5".to_string()],
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: 0,
                cache_read_tokens: 2,
                total_tokens: 17,
                total_cost: 0.03,
                metadata: None,
                metadata_agents: Some(vec!["codex"]),
                agent_breakdowns: None,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "gpt-5".to_string(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 2,
                    cost: 0.03,
                    ..ModelBreakdown::default()
                }],
            },
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "claude",
                models_used: vec!["gpt-5".to_string(), "claude-sonnet-4-20250514".to_string()],
                input_tokens: 30,
                output_tokens: 20,
                cache_creation_tokens: 3,
                cache_read_tokens: 4,
                total_tokens: 57,
                total_cost: 0.07,
                metadata: None,
                metadata_agents: Some(vec!["claude"]),
                agent_breakdowns: None,
                model_breakdowns: vec![
                    ModelBreakdown {
                        model_name: "gpt-5".to_string(),
                        input_tokens: 8,
                        output_tokens: 3,
                        cache_creation_tokens: 1,
                        cache_read_tokens: 2,
                        cost: 0.01,
                        missing_pricing: true,
                        ..ModelBreakdown::default()
                    },
                    ModelBreakdown {
                        model_name: "claude-sonnet-4-20250514".to_string(),
                        input_tokens: 22,
                        output_tokens: 17,
                        cache_creation_tokens: 2,
                        cache_read_tokens: 2,
                        cost: 0.06,
                        ..ModelBreakdown::default()
                    },
                ],
            },
        ],
        AgentReportKind::Daily,
    );

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model_breakdowns.len(), 2);
    assert_eq!(
        rows[0].model_breakdowns[0].model_name,
        "claude-sonnet-4-20250514"
    );
    assert_eq!(rows[0].model_breakdowns[0].cost, 0.06);
    assert_eq!(rows[0].model_breakdowns[1].model_name, "gpt-5");
    assert_eq!(rows[0].model_breakdowns[1].input_tokens, 18);
    assert_eq!(rows[0].model_breakdowns[1].output_tokens, 8);
    assert_eq!(rows[0].model_breakdowns[1].cache_creation_tokens, 1);
    assert_eq!(rows[0].model_breakdowns[1].cache_read_tokens, 4);
    assert_eq!(rows[0].model_breakdowns[1].cost, 0.04);
    assert!(rows[0].model_breakdowns[1].missing_pricing);
}

#[test]
fn splits_aggregated_rows_by_agent_and_model_when_requested() {
    let rows = aggregate_rows(
        vec![
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "pi",
                models_used: vec!["[pi] gpt-5.4".to_string()],
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: 1,
                cache_read_tokens: 100,
                total_tokens: 116,
                total_cost: 0.1,
                metadata: None,
                metadata_agents: Some(vec!["pi"]),
                agent_breakdowns: None,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "[pi] gpt-5.4".to_string(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_tokens: 1,
                    cache_read_tokens: 100,
                    cost: 0.1,
                    ..ModelBreakdown::default()
                }],
            },
            AllRow {
                period: "2026-01-02".to_string(),
                agent: "pi",
                models_used: vec!["[pi] anthropic/claude-opus-4-7".to_string()],
                input_tokens: 20,
                output_tokens: 10,
                cache_creation_tokens: 2,
                cache_read_tokens: 200,
                total_tokens: 232,
                total_cost: 0.25,
                metadata: None,
                metadata_agents: Some(vec!["pi"]),
                agent_breakdowns: None,
                model_breakdowns: vec![ModelBreakdown {
                    model_name: "[pi] anthropic/claude-opus-4-7".to_string(),
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_creation_tokens: 2,
                    cache_read_tokens: 200,
                    cost: 0.25,
                    ..ModelBreakdown::default()
                }],
            },
        ],
        AgentReportKind::Monthly,
    );

    let rows = super::loader::split_rows_by_model(
        rows,
        &crate::cli::SharedArgs {
            by_model: true,
            ..crate::cli::SharedArgs::default()
        },
    );

    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .map(|row| row.models_used.clone())
            .collect::<Vec<_>>(),
        vec![
            vec!["[pi] anthropic/claude-opus-4-7".to_string()],
            vec!["[pi] gpt-5.4".to_string()],
        ]
    );
    assert_eq!(
        rows.iter().map(|row| row.total_tokens).collect::<Vec<_>>(),
        vec![232, 116]
    );
}

#[test]
fn displays_total_tokens_with_cache_tokens_like_typescript_table() {
    let row = AllRow {
        period: "2026-01-02".to_string(),
        agent: "codex",
        models_used: vec!["gpt-5".to_string()],
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_tokens: 0,
        cache_read_tokens: 10,
        total_tokens: 120,
        total_cost: 0.01,
        metadata: None,
        metadata_agents: Some(vec!["codex"]),
        agent_breakdowns: None,
        model_breakdowns: Vec::new(),
    };

    let cells = all_table_row(&row, false, false);

    assert_eq!(cells[7], "130");
}

#[test]
fn report_title_uses_detected_agents_even_when_filtered_rows_are_sparse() {
    let rows = vec![AllRow {
        period: "2026-01-02".to_string(),
        agent: "all",
        models_used: vec!["gpt-5".to_string()],
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_tokens: 0,
        cache_read_tokens: 10,
        total_tokens: 120,
        total_cost: 0.01,
        metadata: None,
        metadata_agents: Some(vec!["codex"]),
        agent_breakdowns: None,
        model_breakdowns: Vec::new(),
    }];

    let title = all_report_title(
        AgentReportKind::Daily,
        &rows,
        &["amp", "claude", "codex", "opencode", "pi"],
    );

    assert_eq!(
        title,
        "Coding (Agent) CLI Usage Report - Daily\nDetected: Amp, Claude, Codex, OpenCode, pi-agent"
    );
}

#[test]
fn all_table_rows_match_main_agent_breakdown_display() {
    let row = AllRow {
        period: "2026-01-02".to_string(),
        agent: "all",
        models_used: vec!["gpt-5".to_string()],
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_tokens: 0,
        cache_read_tokens: 10,
        total_tokens: 130,
        total_cost: 0.01,
        metadata: None,
        metadata_agents: Some(vec!["codex"]),
        agent_breakdowns: Some(vec![AllRow {
            period: "2026-01-02".to_string(),
            agent: "codex",
            models_used: vec!["gpt-5".to_string()],
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_tokens: 0,
            cache_read_tokens: 10,
            total_tokens: 130,
            total_cost: 0.01,
            metadata: None,
            metadata_agents: Some(vec!["codex"]),
            agent_breakdowns: None,
            model_breakdowns: Vec::new(),
        }]),
        model_breakdowns: Vec::new(),
    };

    assert_eq!(
        all_table_row(&row, true, false),
        vec!["2026-01-02", "All", "", "100", "20", "$0.01"]
    );
    assert_eq!(
        all_table_row(
            row.agent_breakdowns.as_ref().unwrap().first().unwrap(),
            true,
            true
        ),
        vec!["", "- Codex", "- gpt-5", "100", "20", "$0.01"]
    );
}

#[test]
fn all_report_title_lists_detected_agents() {
    let row = AllRow {
        period: "2026-01-02".to_string(),
        agent: "all",
        models_used: Vec::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        total_tokens: 0,
        total_cost: 0.0,
        metadata: None,
        metadata_agents: Some(vec!["claude", "codex"]),
        agent_breakdowns: None,
        model_breakdowns: Vec::new(),
    };

    assert_eq!(
        all_report_title(AgentReportKind::Daily, &[row], &[]),
        "Coding (Agent) CLI Usage Report - Daily\nDetected: Claude, Codex"
    );
}

#[test]
fn compact_table_columns_omit_cache_and_total_token_metrics() {
    let (headers, aligns) = all_table_columns(AgentReportKind::Daily, true);

    assert_eq!(
        headers,
        vec!["Date", "Agent", "Models", "Input", "Output", "Cost (USD)"]
    );
    assert_eq!(
        aligns,
        vec![
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
        ]
    );
}

#[test]
fn full_table_columns_include_cache_and_total_token_metrics() {
    let (headers, aligns) = all_table_columns(AgentReportKind::Daily, false);

    assert_eq!(
        headers,
        vec![
            "Date",
            "Agent",
            "Models",
            "Input",
            "Output",
            "Cache Create",
            "Cache Read",
            "Total Tokens",
            "Cost (USD)",
        ]
    );
    assert_eq!(headers.len(), aligns.len());
}
