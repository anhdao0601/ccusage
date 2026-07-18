use std::{
    borrow::Cow,
    sync::{Mutex, OnceLock, RwLock},
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::fast::FxHashMap;

const BUILD_TIME_PRICING_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/litellm-pricing.json"));
const BUILD_TIME_MODELS_DEV_JSON: &str = include_str!("models-dev-pricing.json");
const FAST_MULTIPLIER_OVERRIDES_JSON: &str = include_str!("fast-multiplier-overrides.json");
const LITELLM_PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const PRICING_FETCH_TIMEOUT_SECONDS: u64 = 10;
const PRICING_FETCH_MAX_BYTES: u64 = 64 * 1024 * 1024;
const MODELS_DEV_FAILURE_RETRY_AFTER: Duration = Duration::from_secs(60);
// Anthropic date-suffixed model aliases use YYYYMMDD, while other numeric
// suffixes are treated as distinct model versions.
const MODEL_DATE_SUFFIX_DIGITS: usize = 8;
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Pricing {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_create: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_read_explicit: bool,
    pub(crate) input_above_200k: Option<f64>,
    pub(crate) output_above_200k: Option<f64>,
    pub(crate) cache_create_above_200k: Option<f64>,
    pub(crate) cache_read_above_200k: Option<f64>,
    pub(crate) fast_multiplier: f64,
}

#[derive(Debug, Default)]
pub(crate) struct PricingMap {
    entries: FxHashMap<String, Pricing>,
    context_limits: FxHashMap<String, u64>,
    enable_models_dev_fallback: bool,
    enable_embedded_models_dev_fallback: bool,
    // Memoizes fuzzy `find_entry` misses of the exact-key map; those fall back
    // to a full scan of `entries`, which is too slow to repeat per usage event.
    resolved_models: RwLock<FxHashMap<String, Option<Pricing>>>,
}

#[derive(Debug, Deserialize)]
struct LiteLlmPricing {
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    input_cost_per_token_above_200k_tokens: Option<f64>,
    output_cost_per_token_above_200k_tokens: Option<f64>,
    cache_creation_input_token_cost_above_200k_tokens: Option<f64>,
    cache_read_input_token_cost_above_200k_tokens: Option<f64>,
    max_input_tokens: Option<u64>,
    provider_specific_entry: Option<ProviderSpecificEntry>,
}

#[derive(Debug, Deserialize)]
struct ProviderSpecificEntry {
    fast: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevProvider {
    models: FxHashMap<String, ModelsDevModel>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ModelsDevJson {
    Providers(FxHashMap<String, ModelsDevProvider>),
    Models(FxHashMap<String, ModelsDevModel>),
}

struct ModelsDevPricingCache {
    pricing: OnceLock<PricingMap>,
    last_failure: Mutex<Option<Instant>>,
    failure_retry_after: Duration,
}

impl ModelsDevPricingCache {
    const fn new(failure_retry_after: Duration) -> Self {
        Self {
            pricing: OnceLock::new(),
            last_failure: Mutex::new(None),
            failure_retry_after,
        }
    }

    fn get_or_try_load<F>(&self, fetch_json: F) -> Option<&PricingMap>
    where
        F: FnOnce() -> std::io::Result<String>,
    {
        if let Some(pricing) = self.pricing.get() {
            return Some(pricing);
        }
        if self.last_failure.lock().is_ok_and(|last_failure| {
            last_failure.is_some_and(|failed_at| failed_at.elapsed() < self.failure_retry_after)
        }) {
            return None;
        }

        let Some(map) = load_models_dev_pricing(fetch_json) else {
            if let Ok(mut last_failure) = self.last_failure.lock() {
                *last_failure = Some(Instant::now());
            }
            return None;
        };
        let _ = self.pricing.set(map);
        if let Ok(mut last_failure) = self.last_failure.lock() {
            *last_failure = None;
        }
        self.pricing.get()
    }
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    id: Option<String>,
    cost: Option<ModelsDevCost>,
    limit: Option<ModelsDevLimit>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevLimit {
    context: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct FastMultiplierOverrides {
    exact: FxHashMap<String, f64>,
    normalized_prefix: FxHashMap<String, f64>,
}

impl FastMultiplierOverrides {
    fn load() -> Self {
        serde_json::from_str(FAST_MULTIPLIER_OVERRIDES_JSON)
            .expect("parse embedded fast-multiplier-overrides.json")
    }

    fn multiplier_for(&self, model: &str) -> Option<f64> {
        if let Some(multiplier) = self.exact.get(model) {
            return Some(*multiplier);
        }
        let normalized = model.replace(['.', '@'], "-");
        normalized.split(['/', ':']).find_map(|part| {
            self.normalized_prefix
                .iter()
                .find_map(|(base, multiplier)| {
                    matches_model_suffix(part, base).then_some(*multiplier)
                })
        })
    }
}

impl PricingMap {
    pub(crate) fn load_embedded() -> Self {
        let mut map = Self::default();
        let fast_multiplier_overrides = FastMultiplierOverrides::load();
        map.load_json_with_overrides(BUILD_TIME_PRICING_JSON, &fast_multiplier_overrides);
        map.put_builtin_pricing(&fast_multiplier_overrides);
        map.enable_embedded_models_dev_fallback = true;
        map
    }

    pub(crate) fn load(offline: bool, force_refresh: bool, log: bool) -> Self {
        let mut map = Self::load_embedded();
        if offline {
            return map;
        }

        if !force_refresh {
            if let Some(json) = crate::pricing_cache::read_fresh_pricing_cache() {
                if map.load_json(&json) > 0 {
                    map.enable_models_dev_fallback = true;
                    return map;
                }
            }
        }

        let fetch_result = crate::progress::track_status(
            log && crate::progress::usage_load_output_is_tty(),
            "Refreshing model pricing from LiteLLM...",
            fetch_pricing_json,
        );

        match fetch_result {
            Ok(json) => {
                let loaded_count = map.load_json(&json);
                if loaded_count > 0 {
                    crate::pricing_cache::write_pricing_cache(&json);
                }
                if loaded_count == 0 && should_log_pricing_refresh_details() {
                    eprintln!("WARN  Failed to parse LiteLLM pricing; using embedded pricing.");
                }
            }
            Err(error) => {
                let loaded_stale_cache = crate::pricing_cache::read_pricing_cache()
                    .is_some_and(|json| map.load_json(&json) > 0);
                if should_log_pricing_refresh_details() && !loaded_stale_cache {
                    eprintln!(
                        "WARN  Failed to fetch LiteLLM pricing ({error}); using embedded pricing."
                    );
                } else if should_log_pricing_refresh_details() {
                    eprintln!(
                        "WARN  Failed to fetch LiteLLM pricing ({error}); using cached pricing."
                    );
                }
            }
        }

        map.enable_models_dev_fallback = true;
        map
    }

    pub(crate) fn load_json(&mut self, json: &str) -> usize {
        let fast_multiplier_overrides = FastMultiplierOverrides::load();
        self.load_json_with_overrides(json, &fast_multiplier_overrides)
    }

    fn load_json_with_overrides(
        &mut self,
        json: &str,
        fast_multiplier_overrides: &FastMultiplierOverrides,
    ) -> usize {
        self.invalidate_resolved_models();
        let Ok(raw) = serde_json::from_str::<FxHashMap<String, serde_json::Value>>(json) else {
            return 0;
        };
        let mut loaded_count = 0;
        for (model, value) in raw {
            let Ok(pricing) = serde_json::from_value::<LiteLlmPricing>(value) else {
                continue;
            };
            let Some(input) = pricing.input_cost_per_token else {
                continue;
            };
            let Some(output) = pricing.output_cost_per_token else {
                continue;
            };
            let context_limit = pricing.max_input_tokens;
            let cache_read_explicit = pricing.cache_read_input_token_cost.is_some();
            let fast_multiplier = pricing
                .provider_specific_entry
                .and_then(|entry| entry.fast)
                .or_else(|| fast_multiplier_overrides.multiplier_for(&model))
                .unwrap_or(1.0);
            self.entries.insert(
                model.clone(),
                Pricing {
                    input,
                    output,
                    cache_create: pricing
                        .cache_creation_input_token_cost
                        .unwrap_or(input * 1.25),
                    cache_read: pricing.cache_read_input_token_cost.unwrap_or(input * 0.1),
                    cache_read_explicit,
                    input_above_200k: pricing.input_cost_per_token_above_200k_tokens,
                    output_above_200k: pricing.output_cost_per_token_above_200k_tokens,
                    cache_create_above_200k: pricing
                        .cache_creation_input_token_cost_above_200k_tokens,
                    cache_read_above_200k: pricing.cache_read_input_token_cost_above_200k_tokens,
                    fast_multiplier,
                },
            );
            if let Some(context_limit) = context_limit {
                self.context_limits.insert(model, context_limit);
            }
            loaded_count += 1;
        }
        loaded_count
    }

    fn load_models_dev_json_missing(&mut self, json: &str) -> Option<usize> {
        self.invalidate_resolved_models();
        let raw = serde_json::from_str::<ModelsDevJson>(json).ok()?;
        Some(match raw {
            ModelsDevJson::Providers(providers) => providers
                .into_values()
                .map(|provider| self.load_models_dev_models(provider.models))
                .sum(),
            ModelsDevJson::Models(models) => self.load_models_dev_models(models),
        })
    }

    fn load_models_dev_models(&mut self, models: FxHashMap<String, ModelsDevModel>) -> usize {
        let mut loaded_count = 0;
        for (model_key, model) in models {
            let model_id = model.id.unwrap_or(model_key);
            if self.entries.contains_key(&model_id) {
                continue;
            }
            let Some(cost) = model.cost else {
                continue;
            };
            let Some(input) = cost.input else {
                continue;
            };
            let Some(output) = cost.output else {
                continue;
            };
            let input = input / 1_000_000.0;
            let output = output / 1_000_000.0;
            let cache_read_explicit = cost.cache_read.is_some();
            self.entries.insert(
                model_id.clone(),
                Pricing {
                    input,
                    output,
                    cache_create: cost
                        .cache_write
                        .map(|value| value / 1_000_000.0)
                        .unwrap_or(input * 1.25),
                    cache_read: cost
                        .cache_read
                        .map(|value| value / 1_000_000.0)
                        .unwrap_or(input * 0.1),
                    cache_read_explicit,
                    input_above_200k: None,
                    output_above_200k: None,
                    cache_create_above_200k: None,
                    cache_read_above_200k: None,
                    fast_multiplier: 1.0,
                },
            );
            if let Some(context_limit) = model.limit.and_then(|limit| limit.context) {
                self.context_limits.insert(model_id, context_limit);
            }
            loaded_count += 1;
        }
        loaded_count
    }

    pub(crate) fn find(&self, model: &str) -> Option<Pricing> {
        let alias = crate::model_aliases::resolve_model_name(model);
        let resolved_alias = alias.as_ref();
        self.find_entry_or_alias(model)
            .or_else(|| {
                (resolved_alias != model)
                    .then(|| self.find_entry_or_alias(resolved_alias))
                    .flatten()
            })
            .or_else(|| {
                self.enable_models_dev_fallback
                    .then(|| {
                        models_dev_pricing()
                            .and_then(|pricing| pricing.find_entry_or_alias(resolved_alias))
                    })
                    .flatten()
            })
            .or_else(|| {
                self.enable_embedded_models_dev_fallback
                    .then(|| embedded_models_dev_pricing().find_entry_or_alias(resolved_alias))
                    .flatten()
            })
    }

    fn find_entry_or_alias(&self, model: &str) -> Option<Pricing> {
        self.find_entry(model)
            .or_else(|| pricing_alias(model).and_then(|alias| self.find_entry(alias)))
    }

    fn find_entry(&self, model: &str) -> Option<Pricing> {
        if let Some(pricing) = self.entries.get(model) {
            return Some(*pricing);
        }
        if let Ok(resolved) = self.resolved_models.read() {
            if let Some(resolved) = resolved.get(model) {
                return *resolved;
            }
        }
        let normalized_model = normalized_pricing_key(model);
        let resolved = self
            .entries
            .iter()
            .filter(|(candidate, _)| {
                pricing_key_matches(candidate, model, normalized_model.as_ref())
            })
            .max_by(|(left, _), (right, _)| {
                left.len().cmp(&right.len()).then_with(|| right.cmp(left))
            })
            .map(|(_, pricing)| *pricing);
        if let Ok(mut cache) = self.resolved_models.write() {
            cache.insert(model.to_string(), resolved);
        }
        resolved
    }

    fn invalidate_resolved_models(&mut self) {
        if let Ok(cache) = self.resolved_models.get_mut() {
            cache.clear();
        }
    }

    pub(crate) fn context_limit(&self, model: &str) -> Option<u64> {
        let alias = crate::model_aliases::resolve_model_name(model);
        let resolved_alias = alias.as_ref();
        self.context_limit_entry_or_alias(model)
            .or_else(|| {
                (resolved_alias != model)
                    .then(|| self.context_limit_entry_or_alias(resolved_alias))
                    .flatten()
            })
            .or_else(|| {
                self.enable_models_dev_fallback
                    .then(|| {
                        models_dev_pricing().and_then(|pricing| {
                            pricing.context_limit_entry_or_alias(resolved_alias)
                        })
                    })
                    .flatten()
            })
            .or_else(|| {
                self.enable_embedded_models_dev_fallback
                    .then(|| {
                        embedded_models_dev_pricing().context_limit_entry_or_alias(resolved_alias)
                    })
                    .flatten()
            })
    }

    fn context_limit_entry_or_alias(&self, model: &str) -> Option<u64> {
        self.context_limit_entry(model)
            .or_else(|| pricing_alias(model).and_then(|alias| self.context_limit_entry(alias)))
    }

    fn context_limit_entry(&self, model: &str) -> Option<u64> {
        self.context_limits.get(model).copied().or_else(|| {
            let normalized_model = normalized_pricing_key(model);
            self.context_limits
                .iter()
                .filter(|(candidate, _)| {
                    pricing_key_matches(candidate, model, normalized_model.as_ref())
                })
                .max_by(|(left, _), (right, _)| {
                    left.len().cmp(&right.len()).then_with(|| right.cmp(left))
                })
                .map(|(_, context_limit)| *context_limit)
        })
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn models_dev_fallback_enabled(&self) -> bool {
        self.enable_models_dev_fallback
    }

    fn put_builtin_pricing(&mut self, fast_multiplier_overrides: &FastMultiplierOverrides) {
        self.invalidate_resolved_models();
        self.entries.insert(
            "claude-opus-4-5".to_string(),
            Pricing {
                input: 5e-6,
                output: 25e-6,
                cache_create: 6.25e-6,
                cache_read: 0.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "claude-opus-4-6".to_string(),
            Pricing {
                input: 5e-6,
                output: 25e-6,
                cache_create: 6.25e-6,
                cache_read: 0.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: fast_multiplier_overrides
                    .multiplier_for("claude-opus-4-6")
                    .unwrap_or(1.0),
            },
        );
        self.entries.insert(
            "claude-opus-4-7".to_string(),
            Pricing {
                input: 5e-6,
                output: 25e-6,
                cache_create: 6.25e-6,
                cache_read: 0.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: fast_multiplier_overrides
                    .multiplier_for("claude-opus-4-7")
                    .unwrap_or(1.0),
            },
        );
        self.entries.insert(
            "claude-opus-4-8".to_string(),
            Pricing {
                input: 5e-6,
                output: 25e-6,
                cache_create: 6.25e-6,
                cache_read: 0.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: fast_multiplier_overrides
                    .multiplier_for("claude-opus-4-8")
                    .unwrap_or(1.0),
            },
        );
        self.entries.insert(
            "claude-haiku-4-5".to_string(),
            Pricing {
                input: 1e-6,
                output: 5e-6,
                cache_create: 1.25e-6,
                cache_read: 0.1e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "claude-fable-5".to_string(),
            Pricing {
                input: 10e-6,
                output: 50e-6,
                cache_create: 12.5e-6,
                cache_read: 1e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "claude-opus-4".to_string(),
            Pricing {
                input: 15e-6,
                output: 75e-6,
                cache_create: 18.75e-6,
                cache_read: 1.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "claude-sonnet-4-6".to_string(),
            Pricing {
                input: 3e-6,
                output: 15e-6,
                cache_create: 3.75e-6,
                cache_read: 0.3e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "claude-sonnet-4".to_string(),
            Pricing {
                input: 3e-6,
                output: 15e-6,
                cache_create: 3.75e-6,
                cache_read: 0.3e-6,
                cache_read_explicit: true,
                input_above_200k: Some(6e-6),
                output_above_200k: Some(22.5e-6),
                cache_create_above_200k: Some(7.5e-6),
                cache_read_above_200k: Some(0.6e-6),
                fast_multiplier: 1.0,
            },
        );
        let claude_3_5_haiku = Pricing {
            input: 0.8e-6,
            output: 4e-6,
            cache_create: 1.0e-6,
            cache_read: 0.08e-6,
            cache_read_explicit: true,
            input_above_200k: None,
            output_above_200k: None,
            cache_create_above_200k: None,
            cache_read_above_200k: None,
            fast_multiplier: 1.0,
        };
        self.entries
            .insert("claude-3-5-haiku".to_string(), claude_3_5_haiku);
        self.entries
            .insert("claude-3-5-haiku-20241022".to_string(), claude_3_5_haiku);
        self.entries.insert(
            "claude-3-opus".to_string(),
            Pricing {
                input: 15e-6,
                output: 75e-6,
                cache_create: 18.75e-6,
                cache_read: 1.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "claude-3-sonnet".to_string(),
            Pricing {
                input: 3e-6,
                output: 15e-6,
                cache_create: 3.75e-6,
                cache_read: 0.3e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "claude-3-haiku".to_string(),
            Pricing {
                input: 0.25e-6,
                output: 1.25e-6,
                cache_create: 0.3e-6,
                cache_read: 0.03e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "gpt-5".to_string(),
            Pricing {
                input: 1.25e-6,
                output: 10e-6,
                cache_create: 1.25e-6,
                cache_read: 0.125e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "gpt-5.5".to_string(),
            Pricing {
                input: 5e-6,
                output: 30e-6,
                cache_create: 5e-6,
                cache_read: 0.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: fast_multiplier_overrides
                    .multiplier_for("gpt-5.5")
                    .unwrap_or(1.0),
            },
        );
        let gpt_5_6_sol = Pricing {
            input: 5e-6,
            output: 30e-6,
            cache_create: 6.25e-6,
            cache_read: 0.5e-6,
            cache_read_explicit: true,
            input_above_200k: None,
            output_above_200k: None,
            cache_create_above_200k: None,
            cache_read_above_200k: None,
            fast_multiplier: 1.0,
        };
        self.entries.insert("gpt-5.6".to_string(), gpt_5_6_sol);
        self.entries.insert("gpt-5.6-sol".to_string(), gpt_5_6_sol);
        self.entries.insert(
            "gpt-5.6-terra".to_string(),
            Pricing {
                input: 2.5e-6,
                output: 15e-6,
                cache_create: 3.125e-6,
                cache_read: 0.25e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "gpt-5.6-luna".to_string(),
            Pricing {
                input: 1e-6,
                output: 6e-6,
                cache_create: 1.25e-6,
                cache_read: 0.1e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "grok-4.3".to_string(),
            Pricing {
                input: 1.25e-6,
                output: 2.5e-6,
                cache_create: 1.25e-6,
                cache_read: 0.125e-6,
                cache_read_explicit: false,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        // Source: https://platform.kimi.ai/docs/pricing/chat-k25
        self.entries.insert(
            "moonshot/kimi-k2.5".to_string(),
            Pricing {
                input: 0.6e-6,
                output: 3e-6,
                cache_create: 0.75e-6,
                cache_read: 0.1e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        // Source: https://platform.kimi.ai/docs/pricing/chat-k26
        self.entries.insert(
            "moonshot/kimi-k2.6".to_string(),
            Pricing {
                input: 0.95e-6,
                output: 4e-6,
                cache_create: 1.1875e-6,
                cache_read: 0.16e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        // Source: https://www.kimi.com/resources/kimi-k2-7-code-pricing
        self.entries.insert(
            "moonshot/kimi-k2.7-code".to_string(),
            Pricing {
                input: 0.95e-6,
                output: 4e-6,
                cache_create: 1.1875e-6,
                cache_read: 0.19e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        for (model, input, output, cache_read) in [
            ("glm-4.5", 0.6e-6, 2.2e-6, 0.11e-6),
            ("glm-4.5-x", 2.2e-6, 8.9e-6, 0.45e-6),
            ("glm-4.5-air", 0.2e-6, 1.1e-6, 0.03e-6),
            ("glm-4.5-airx", 1.1e-6, 4.5e-6, 0.22e-6),
            ("glm-4.5v", 0.6e-6, 1.8e-6, 0.11e-6),
            ("glm-4.5-flash", 0.0, 0.0, 0.0),
            ("glm-4.6", 0.6e-6, 2.2e-6, 0.11e-6),
            ("glm-4.7", 0.6e-6, 2.2e-6, 0.11e-6),
            ("glm-4.7-flash", 0.07e-6, 0.4e-6, 0.0),
            ("glm-5", 1.0e-6, 3.2e-6, 0.2e-6),
            ("glm-5.2", 1.4e-6, 4.4e-6, 0.26e-6),
            ("glm-5.2[1m]", 1.4e-6, 4.4e-6, 0.26e-6),
        ] {
            let pricing = Pricing {
                input,
                output,
                cache_create: 0.0,
                cache_read,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            };
            self.entries.insert(model.to_string(), pricing);
            self.entries.insert(format!("zai/{model}"), pricing);
        }
        let gpt_5_1_pricing = Pricing {
            input: 1.25e-6,
            output: 10e-6,
            cache_create: 1.25e-6,
            cache_read: 0.125e-6,
            cache_read_explicit: true,
            input_above_200k: None,
            output_above_200k: None,
            cache_create_above_200k: None,
            cache_read_above_200k: None,
            fast_multiplier: 1.0,
        };
        self.entries.insert("gpt-5.1".to_string(), gpt_5_1_pricing);
        self.entries
            .insert("gpt-5.1-codex".to_string(), gpt_5_1_pricing);
        let gpt_5_codex_pricing = Pricing {
            input: 1.75e-6,
            output: 14e-6,
            cache_create: 1.75e-6,
            cache_read: 0.175e-6,
            cache_read_explicit: true,
            input_above_200k: None,
            output_above_200k: None,
            cache_create_above_200k: None,
            cache_read_above_200k: None,
            fast_multiplier: 1.0,
        };
        self.entries
            .insert("gpt-5.2-codex".to_string(), gpt_5_codex_pricing);
        self.entries.insert(
            "gpt-5.3-codex".to_string(),
            Pricing {
                fast_multiplier: fast_multiplier_overrides
                    .multiplier_for("gpt-5.3-codex")
                    .unwrap_or(1.0),
                ..gpt_5_codex_pricing
            },
        );
        self.entries
            .insert("gpt-5.2".to_string(), gpt_5_codex_pricing);
        self.entries.insert(
            "gpt-5.4".to_string(),
            Pricing {
                input: 2.5e-6,
                output: 15e-6,
                cache_create: 2.5e-6,
                cache_read: 0.25e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: fast_multiplier_overrides
                    .multiplier_for("gpt-5.4")
                    .unwrap_or(1.0),
            },
        );
        self.entries.insert(
            "gpt-5.4-mini".to_string(),
            Pricing {
                input: 0.75e-6,
                output: 4.5e-6,
                cache_create: 0.75e-6,
                cache_read: 0.075e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.entries.insert(
            "gpt-5.4-nano".to_string(),
            Pricing {
                input: 0.2e-6,
                output: 1.25e-6,
                cache_create: 0.2e-6,
                cache_read: 0.02e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        self.context_limits.insert("gpt-5.5".to_string(), 1_050_000);
        for model in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            self.context_limits.insert(model.to_string(), 1_050_000);
        }
        self.context_limits
            .insert("grok-4.3".to_string(), 1_000_000);
        self.context_limits.insert("gpt-5.4".to_string(), 1_050_000);
        for model in [
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-fable-5",
        ] {
            self.context_limits.insert(model.to_string(), 1_000_000);
        }
        self.context_limits
            .insert("moonshot/kimi-k2.5".to_string(), 262_144);
        self.context_limits
            .insert("moonshot/kimi-k2.6".to_string(), 262_144);
        self.context_limits
            .insert("moonshot/kimi-k2.7-code".to_string(), 262_144);
        for model in ["zai/glm-5.2", "zai/glm-5.2[1m]"] {
            self.context_limits.insert(model.to_string(), 1_000_000);
        }

        for model in [
            "claude-opus-4-5",
            "claude-haiku-4-5",
            "claude-opus-4",
            "claude-sonnet-4",
            "claude-3-5-haiku",
            "claude-3-5-haiku-20241022",
            "claude-3-opus",
            "claude-3-sonnet",
            "claude-3-haiku",
        ] {
            self.context_limits.insert(model.to_string(), 200_000);
        }
    }
}

pub(crate) fn embedded_pricing_fingerprint() -> String {
    let pricing = PricingMap::load_embedded();
    format!(
        "pricing:offline:{:016x}",
        pricing.embedded_fingerprint_hash()
    )
}

impl PricingMap {
    fn embedded_fingerprint_hash(&self) -> u64 {
        let mut hash = FNV_OFFSET;
        let mut entries = self.entries.iter().collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (model, pricing) in entries {
            hash = hash_combine(hash, model.as_bytes());
            hash = hash_combine(hash, &pricing.input.to_bits().to_le_bytes());
            hash = hash_combine(hash, &pricing.output.to_bits().to_le_bytes());
            hash = hash_combine(hash, &pricing.cache_create.to_bits().to_le_bytes());
            hash = hash_combine(hash, &pricing.cache_read.to_bits().to_le_bytes());
            hash = hash_combine(hash, &[u8::from(pricing.cache_read_explicit)]);
            hash = hash_combine_optional_f64(hash, pricing.input_above_200k);
            hash = hash_combine_optional_f64(hash, pricing.output_above_200k);
            hash = hash_combine_optional_f64(hash, pricing.cache_create_above_200k);
            hash = hash_combine_optional_f64(hash, pricing.cache_read_above_200k);
            hash = hash_combine(hash, &pricing.fast_multiplier.to_bits().to_le_bytes());
        }

        let mut context_limits = self.context_limits.iter().collect::<Vec<_>>();
        context_limits.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (model, context_limit) in context_limits {
            hash = hash_combine(hash, model.as_bytes());
            hash = hash_combine(hash, &context_limit.to_le_bytes());
        }
        hash
    }
}

fn hash_combine_optional_f64(hash: u64, value: Option<f64>) -> u64 {
    match value {
        Some(value) => hash_combine(hash_combine(hash, &[1]), &value.to_bits().to_le_bytes()),
        None => hash_combine(hash, &[0]),
    }
}

fn hash_combine(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Matches pricing keys across provider/model aliases while preserving version boundaries.
fn pricing_key_matches(candidate: &str, model: &str, normalized_model: &str) -> bool {
    if contains_pricing_key(model, candidate) || contains_pricing_key(candidate, model) {
        return true;
    }
    let normalized_candidate = normalized_pricing_key(candidate);
    if contains_pricing_key(normalized_model, normalized_candidate.as_ref())
        || contains_pricing_key(normalized_candidate.as_ref(), normalized_model)
    {
        return true;
    }
    normalized_candidate
        .rsplit('/')
        .next()
        .is_some_and(|candidate_model| contains_pricing_key(normalized_model, candidate_model))
}

/// Finds a key only when the surrounding bytes are non-alphanumeric boundaries.
fn contains_pricing_key(value: &str, key: &str) -> bool {
    value.match_indices(key).any(|(index, _)| {
        let before = index
            .checked_sub(1)
            .and_then(|before| value.as_bytes().get(before))
            .copied();
        let suffix = &value[index + key.len()..];
        before.is_none_or(is_pricing_key_boundary) && suffix_allows_pricing_key_match(key, suffix)
    })
}

/// Treats punctuation separators as boundaries, but not adjacent version digits.
fn is_pricing_key_boundary(byte: u8) -> bool {
    !byte.is_ascii_alphanumeric()
}

fn suffix_allows_pricing_key_match(key: &str, suffix: &str) -> bool {
    let Some(separator) = suffix.as_bytes().first().copied() else {
        return true;
    };
    if !is_pricing_key_boundary(separator) {
        return false;
    }
    !suffix_starts_with_numeric_model_version(key, suffix)
}

fn suffix_starts_with_numeric_model_version(key: &str, suffix: &str) -> bool {
    if !key.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    if !matches!(suffix.as_bytes().first(), Some(b'-' | b'.')) {
        return false;
    }

    let rest = &suffix[1..];
    let digit_len = rest
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_len == 0 {
        return false;
    }
    let after_digits = rest.as_bytes().get(digit_len).copied();
    !(digit_len == MODEL_DATE_SUFFIX_DIGITS && after_digits.is_none_or(is_pricing_key_boundary))
}

/// Normalizes known model separator variants without allocating for canonical keys.
fn normalized_pricing_key(value: &str) -> Cow<'_, str> {
    if value.contains(['.', '@']) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(value.to_ascii_lowercase().replace(['.', '@'], "-"))
    } else {
        Cow::Borrowed(value)
    }
}

/// Maps Codex log labels that upstream pricing sources do not publish to
/// canonical pricing keys.
fn pricing_alias(model: &str) -> Option<&'static str> {
    match model {
        "gpt-5.3-spark" => Some("gpt-5.3-codex-spark"),
        _ => None,
    }
}

fn matches_model_suffix(part: &str, base: &str) -> bool {
    let Some(index) = part.rfind(base) else {
        return false;
    };
    let suffix = &part[index..];
    suffix == base || suffix.as_bytes().get(base.len()) == Some(&b'-')
}

fn should_log_pricing_refresh_details() -> bool {
    crate::log_level().is_some_and(|level| level >= 4)
}

fn models_dev_pricing() -> Option<&'static PricingMap> {
    static MODELS_DEV_PRICING: ModelsDevPricingCache =
        ModelsDevPricingCache::new(MODELS_DEV_FAILURE_RETRY_AFTER);
    MODELS_DEV_PRICING.get_or_try_load(fetch_models_dev_json)
}

fn embedded_models_dev_pricing() -> &'static PricingMap {
    static EMBEDDED_MODELS_DEV_PRICING: OnceLock<PricingMap> = OnceLock::new();
    EMBEDDED_MODELS_DEV_PRICING.get_or_init(|| {
        let mut map = PricingMap::default();
        map.load_models_dev_json_missing(BUILD_TIME_MODELS_DEV_JSON)
            .expect("embedded models-dev-pricing.json must parse");
        map
    })
}

fn load_models_dev_pricing<F>(fetch_json: F) -> Option<PricingMap>
where
    F: FnOnce() -> std::io::Result<String>,
{
    let json = match fetch_json() {
        Ok(json) => json,
        Err(error) => {
            if should_log_pricing_refresh_details() {
                eprintln!(
                    "WARN  Failed to fetch models.dev pricing ({error}); using LiteLLM pricing."
                );
            }
            return None;
        }
    };
    let mut map = PricingMap::default();
    if map.load_models_dev_json_missing(&json).is_none() {
        if should_log_pricing_refresh_details() {
            eprintln!("WARN  Failed to parse models.dev pricing; using LiteLLM pricing.");
        }
        return None;
    }
    Some(map)
}

fn fetch_pricing_json() -> std::io::Result<String> {
    fetch_json_url(LITELLM_PRICING_URL)
}

fn fetch_models_dev_json() -> std::io::Result<String> {
    fetch_json_url(MODELS_DEV_API_URL)
}

fn fetch_json_url(url: &str) -> std::io::Result<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(PRICING_FETCH_TIMEOUT_SECONDS)))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if response.status().as_u16() != 200 {
        return Err(std::io::Error::other(format!(
            "HTTP {}",
            response.status().as_u16()
        )));
    }
    response
        .body_mut()
        .with_config()
        .limit(PRICING_FETCH_MAX_BYTES)
        .read_to_string()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{embedded_models_dev_pricing, Pricing, PricingMap, BUILD_TIME_PRICING_JSON};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn loads_embedded_claude_pricing() {
        let pricing = PricingMap::load_embedded();
        assert!(pricing.len() > 0);
        assert!(pricing.find("claude-sonnet-4-20250514").is_some());
    }

    #[test]
    fn loads_embedded_claude_sonnet_5_pricing() {
        let pricing = PricingMap::load_embedded();

        assert!(pricing.find("claude-sonnet-5").is_some());
    }

    #[test]
    fn offline_resolves_models_only_in_embedded_models_dev() {
        let offline = PricingMap::load_embedded();
        let model = embedded_models_dev_pricing()
            .entries
            .keys()
            .find(|model| offline.find_entry(model).is_none())
            .expect("snapshot should contain a model absent from primary pricing");

        assert!(offline.find(model).is_some());
        assert!(PricingMap::default().find(model).is_none());
    }

    #[test]
    fn reads_embedded_model_context_limits() {
        let pricing = PricingMap::load_embedded();

        assert_eq!(
            pricing.context_limit("anthropic.claude-3-5-sonnet-20240620-v1:0"),
            Some(1_000_000)
        );
    }

    #[test]
    fn embedded_pricing_includes_hermes_frontier_models() {
        let pricing = PricingMap::load_embedded();

        assert!(pricing.find("gpt-5.5").is_some());
        assert!(pricing.find("grok-4.3").is_some());
        assert_eq!(pricing.context_limit("grok-4.3"), Some(1_000_000));
    }

    #[test]
    fn embedded_pricing_includes_moonshot_kimi_for_offline_reports() {
        let pricing = PricingMap::load_embedded();
        let kimi_k25 = pricing.find("moonshot/kimi-k2.5").unwrap();
        let kimi_k26 = pricing.find("moonshot/kimi-k2.6").unwrap();

        assert_eq!(kimi_k25.input, 0.6e-6);
        assert_eq!(kimi_k25.output, 3e-6);
        assert_eq!(kimi_k25.cache_read, 0.1e-6);
        assert!(kimi_k25.cache_read_explicit);
        assert_eq!(kimi_k26.input, 0.95e-6);
        assert_eq!(kimi_k26.output, 4e-6);
        assert_eq!(kimi_k26.cache_read, 0.16e-6);
        assert!(kimi_k26.cache_read_explicit);
        assert_eq!(pricing.context_limit("moonshot/kimi-k2.5"), Some(262_144));
        assert_eq!(pricing.context_limit("moonshot/kimi-k2.6"), Some(262_144));
    }

    #[test]
    fn embedded_pricing_resolves_kimi_k27_hugging_face_model_path() {
        let pricing = PricingMap::load_embedded();
        let model = "/data/models/hf/moonshotai__Kimi-K2.7-Code";
        let kimi_k27 = pricing.find(model).unwrap();

        assert_eq!(kimi_k27.input, 0.95e-6);
        assert_eq!(kimi_k27.output, 4e-6);
        assert_eq!(kimi_k27.cache_read, 0.19e-6);
        assert!(kimi_k27.cache_read_explicit);
        assert_eq!(pricing.context_limit(model), Some(262_144));
    }

    #[test]
    fn embedded_pricing_includes_claude_fable_5_for_offline_reports() {
        let pricing = PricingMap::load_embedded();
        let fable = pricing.find("claude-fable-5").unwrap();

        assert_eq!(fable.input, 10e-6);
        assert_eq!(fable.output, 50e-6);
        assert_eq!(fable.cache_create, 12.5e-6);
        assert_eq!(fable.cache_read, 1e-6);
        assert!(fable.cache_read_explicit);
        assert_eq!(
            pricing.find("claude-fable-5[1m]").unwrap().input,
            fable.input
        );
        assert_eq!(pricing.context_limit("claude-fable-5"), Some(1_000_000));
    }

    #[test]
    fn embedded_pricing_includes_opencode_glm_for_offline_reports() {
        let pricing = PricingMap::load_embedded();

        let glm_46 = pricing.find("glm-4.6").unwrap();
        let glm_47 = pricing.find("glm-4.7").unwrap();
        let glm_47_flash = pricing.find("glm-4.7-flash").unwrap();
        let glm_5 = pricing.find("glm-5").unwrap();
        assert!(glm_46.input > 0.0);
        assert!(glm_47.output > 0.0);
        assert!(glm_47_flash.output > 0.0);
        assert!(glm_5.output > 0.0);
        assert_eq!(glm_46.cache_create, 0.0);
        assert_eq!(glm_47.cache_create, 0.0);
        assert_eq!(glm_47_flash.cache_create, 0.0);
        assert_eq!(glm_5.cache_create, 0.0);
        let glm_52 = pricing.find("glm-5.2").unwrap();
        assert_eq!(glm_52.input, 1.4e-6);
        assert_eq!(glm_52.output, 4.4e-6);
        assert_eq!(glm_52.cache_create, 0.0);
        assert_eq!(glm_52.cache_read, 0.26e-6);
        assert!(glm_52.cache_read_explicit);
        assert_eq!(pricing.find("glm-5.2[1m]").unwrap().input, glm_52.input);
        assert_eq!(pricing.context_limit("glm-5.2[1m]"), Some(1_000_000));
    }

    #[test]
    fn embedded_pricing_resolves_hugging_face_model_paths() {
        let pricing = PricingMap::load_embedded();
        let glm_52 = pricing.find("glm-5.2").unwrap();
        let hf_path = pricing
            .find("/data/models/hf/zai-org__GLM-5.2-FP8")
            .unwrap();

        assert_eq!(hf_path.input, glm_52.input);
        assert_eq!(hf_path.output, glm_52.output);
        assert_eq!(hf_path.cache_read, glm_52.cache_read);
        assert_eq!(
            pricing.context_limit("/data/models/hf/zai-org__GLM-5.2-FP8"),
            pricing.context_limit("glm-5.2")
        );
    }

    #[test]
    fn repeated_fuzzy_lookups_stay_consistent_and_see_later_loads() {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "gemini/gemini-9.9-test": {
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000010
                }
            }"#,
        );

        let first = pricing.find("google/gemini-9.9-test");
        let second = pricing.find("google/gemini-9.9-test");
        assert_eq!(first.map(|entry| entry.input), Some(0.000001));
        assert_eq!(
            first.map(|entry| entry.input.to_bits()),
            second.map(|entry| entry.input.to_bits())
        );
        assert!(pricing.find("google/no-such-model-anywhere").is_none());

        pricing.load_json(
            r#"{
                "google/gemini-9.9-test": {
                    "input_cost_per_token": 0.000002,
                    "output_cost_per_token": 0.000020
                },
                "google/no-such-model-anywhere": {
                    "input_cost_per_token": 0.000003,
                    "output_cost_per_token": 0.000030
                }
            }"#,
        );

        assert_eq!(
            pricing
                .find("google/gemini-9.9-test")
                .map(|entry| entry.input),
            Some(0.000002)
        );
        assert_eq!(
            pricing
                .find("google/no-such-model-anywhere")
                .map(|entry| entry.input),
            Some(0.000003)
        );
    }

    #[test]
    fn records_whether_cache_read_rate_came_from_litellm_pricing() {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "gpt-with-cache": {
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000010,
                    "cache_read_input_token_cost": 0.0000001
                },
                "gpt-without-cache": {
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000010
                }
            }"#,
        );

        assert!(pricing.find("gpt-with-cache").unwrap().cache_read_explicit);
        assert!(
            !pricing
                .find("gpt-without-cache")
                .unwrap()
                .cache_read_explicit
        );
    }

    #[test]
    fn skips_invalid_litellm_entries_without_discarding_valid_pricing() {
        let mut pricing = PricingMap::default();
        let loaded = pricing.load_json(
            r#"{
                "sample_spec": {
                    "max_input_tokens": "max input tokens, if the provider specifies it"
                },
                "gpt-valid": {
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000010,
                    "max_input_tokens": 123
                }
            }"#,
        );

        assert_eq!(loaded, 1);
        assert!(pricing.find("gpt-valid").is_some());
        assert_eq!(pricing.context_limit("gpt-valid"), Some(123));
    }

    #[test]
    fn keeps_models_dev_fallback_disabled_for_embedded_and_offline_pricing() {
        assert!(!PricingMap::load_embedded().models_dev_fallback_enabled());
        assert!(!PricingMap::load(true, false, false).models_dev_fallback_enabled());
    }

    #[test]
    fn retries_models_dev_pricing_after_fetch_failure() {
        let cache = super::ModelsDevPricingCache::new(std::time::Duration::ZERO);

        let failed = cache.get_or_try_load(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "temporary failure",
            ))
        });
        assert!(failed.is_none());

        let pricing = cache
            .get_or_try_load(|| {
                Ok(r#"{
                    "openai": {
                        "id": "openai",
                        "name": "OpenAI",
                        "models": {
                            "gpt-retry": {
                                "id": "gpt-retry",
                                "name": "GPT Retry",
                                "cost": {
                                    "input": 1.0,
                                    "output": 2.0
                                },
                                "limit": {
                                    "context": 42
                                }
                            }
                        }
                    }
                }"#
                .to_string())
            })
            .expect("models.dev retry should cache successful pricing");

        let gpt_retry = pricing
            .find_entry("gpt-retry")
            .expect("successful retry should load pricing");
        assert_eq!(gpt_retry.input, 0.000001);
        assert_eq!(gpt_retry.output, 0.000002);
        assert_eq!(pricing.context_limit_entry("gpt-retry"), Some(42));
    }

    #[test]
    fn backs_off_models_dev_pricing_after_fetch_failure() {
        let cache = super::ModelsDevPricingCache::new(std::time::Duration::from_secs(60));
        let attempts = AtomicUsize::new(0);

        let failed = cache.get_or_try_load(|| {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "temporary failure",
            ))
        });
        assert!(failed.is_none());

        let skipped = cache.get_or_try_load(|| {
            attempts.fetch_add(1, Ordering::Relaxed);
            Ok(r#"{
                "openai": {
                    "id": "openai",
                    "name": "OpenAI",
                    "models": {
                        "gpt-skipped": {
                            "id": "gpt-skipped",
                            "name": "GPT Skipped",
                            "cost": {
                                "input": 1.0,
                                "output": 2.0
                            }
                        }
                    }
                }
            }"#
            .to_string())
        });
        assert!(skipped.is_none());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn loads_missing_models_dev_pricing_without_overriding_litellm() {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "gpt-primary": {
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000010,
                    "cache_read_input_token_cost": 0.0000001,
                    "max_input_tokens": 123
                },
                "openrouter/gpt-alias": {
                    "input_cost_per_token": 0.000003,
                    "output_cost_per_token": 0.000030,
                    "max_input_tokens": 321
                }
            }"#,
        );

        let models_dev_json = r#"{
                "openai": {
                    "id": "openai",
                    "name": "OpenAI",
                    "models": {
                        "gpt-primary": {
                            "id": "gpt-primary",
                            "name": "GPT Primary",
                            "cost": {
                                "input": 9.0,
                                "output": 90.0,
                                "cache_read": 0.9,
                                "cache_write": 11.25
                            },
                            "limit": {
                                "context": 999
                            }
                        },
                        "gpt-fallback": {
                            "id": "gpt-fallback",
                            "name": "GPT Fallback",
                            "cost": {
                                "input": 2.0,
                                "output": 8.0,
                                "cache_read": 0.2,
                                "cache_write": 2.5
                            },
                            "limit": {
                                "context": 456
                            }
                        },
                        "gpt-alias": {
                            "id": "gpt-alias",
                            "name": "GPT Alias",
                            "cost": {
                                "input": 4.0,
                                "output": 16.0
                            },
                            "limit": {
                                "context": 654
                            }
                        }
                    }
                }
            }"#;

        assert_eq!(
            pricing.load_models_dev_json_missing(models_dev_json),
            Some(2)
        );

        let primary = pricing.find("gpt-primary").unwrap();
        let fallback = pricing.find("gpt-fallback").unwrap();
        let alias = pricing.entries.get("gpt-alias").unwrap();

        assert_eq!(primary.input, 1e-6);
        assert_eq!(primary.output, 10e-6);
        assert_eq!(primary.cache_read, 0.1e-6);
        assert_eq!(pricing.context_limit("gpt-primary"), Some(123));
        assert!((fallback.input - 2e-6).abs() < f64::EPSILON);
        assert!((fallback.output - 8e-6).abs() < f64::EPSILON);
        assert!((fallback.cache_create - 2.5e-6).abs() < f64::EPSILON);
        assert!((fallback.cache_read - 0.2e-6).abs() < f64::EPSILON);
        assert!(fallback.cache_read_explicit);
        assert_eq!(fallback.input_above_200k, None);
        assert_eq!(fallback.output_above_200k, None);
        assert_eq!(fallback.fast_multiplier, 1.0);
        assert_eq!(pricing.context_limit("gpt-fallback"), Some(456));
        assert!((alias.input - 4e-6).abs() < f64::EPSILON);
        assert_eq!(pricing.context_limits.get("gpt-alias"), Some(&654));
    }

    #[test]
    fn embedded_pricing_resolves_overlapping_model_keys_exactly() {
        let pricing = PricingMap::load_embedded();
        let sonnet_4 = pricing.find("claude-sonnet-4-20250514").unwrap();
        let sonnet_45 = pricing.find("claude-sonnet-4-5-20250929").unwrap();

        assert_eq!(
            pricing.find("claude-sonnet-4-20250514").unwrap().input,
            sonnet_4.input
        );
        assert_eq!(
            pricing.find("claude-sonnet-4-5-20250929").unwrap().input,
            sonnet_45.input,
        );
        assert_eq!(
            pricing
                .find("anthropic.claude-sonnet-4-20250514-v1:0")
                .unwrap()
                .input,
            sonnet_4.input,
        );
        assert_eq!(
            pricing.find("claude-3-5-haiku-20241022").unwrap().input,
            0.8e-6,
        );
    }

    #[test]
    fn embedded_pricing_includes_gpt_5_5_for_offline_codex_reports() {
        let pricing = PricingMap::load_embedded();
        let gpt_55 = pricing.find("gpt-5.5").unwrap();

        assert_eq!(gpt_55.input, 5e-6);
        assert_eq!(gpt_55.output, 30e-6);
        assert_eq!(gpt_55.cache_read, 0.5e-6);
        assert!(gpt_55.cache_read_explicit);
        assert_eq!(gpt_55.fast_multiplier, 2.5);
        assert_eq!(pricing.context_limit("gpt-5.5"), Some(1_050_000));
    }

    #[test]
    fn embedded_pricing_includes_gpt_5_6_family_for_offline_codex_reports() {
        let pricing = PricingMap::load_embedded();
        let sol = pricing.find("gpt-5.6-sol").unwrap();
        let terra = pricing.find("gpt-5.6-terra").unwrap();
        let luna = pricing.find("gpt-5.6-luna").unwrap();

        assert_eq!(pricing.find("gpt-5.6").unwrap().input, sol.input);
        assert_eq!(sol.input, 5e-6);
        assert_eq!(sol.output, 30e-6);
        assert_eq!(sol.cache_create, 6.25e-6);
        assert_eq!(sol.cache_read, 0.5e-6);
        assert_eq!(terra.input, 2.5e-6);
        assert_eq!(terra.output, 15e-6);
        assert_eq!(terra.cache_create, 3.125e-6);
        assert_eq!(terra.cache_read, 0.25e-6);
        assert_eq!(luna.input, 1e-6);
        assert_eq!(luna.output, 6e-6);
        assert_eq!(luna.cache_create, 1.25e-6);
        assert_eq!(luna.cache_read, 0.1e-6);
        assert_eq!(pricing.context_limit("gpt-5.6-sol"), Some(1_050_000));
        assert_eq!(pricing.context_limit("gpt-5.6-terra"), Some(1_050_000));
        assert_eq!(pricing.context_limit("gpt-5.6-luna"), Some(1_050_000));
    }

    #[test]
    fn embedded_pricing_includes_codex_priority_multiplier() {
        let pricing = PricingMap::load_embedded();

        assert_eq!(pricing.find("gpt-5.5").unwrap().fast_multiplier, 2.5);
        assert_eq!(pricing.find("gpt-5.4").unwrap().fast_multiplier, 2.0);
        assert_eq!(pricing.find("gpt-5.3-codex").unwrap().fast_multiplier, 2.0);
    }

    #[test]
    fn embedded_pricing_does_not_resolve_undated_codex_auto_review_model() {
        let pricing = PricingMap::load_embedded();

        assert!(pricing.find("codex-auto-review").is_none());
        assert!(pricing.context_limit("codex-auto-review").is_none());
    }

    #[test]
    fn embedded_pricing_resolves_codex_spark_short_model_alias() {
        let pricing = PricingMap::load_embedded();
        let short_spark = pricing
            .find("gpt-5.3-spark")
            .expect("gpt-5.3-spark should resolve via model alias");
        let codex_spark = pricing
            .find("gpt-5.3-codex-spark")
            .expect("canonical Codex Spark pricing should exist");

        assert_eq!(short_spark.input, codex_spark.input);
        assert_eq!(short_spark.output, codex_spark.output);
        assert_eq!(short_spark.cache_read, codex_spark.cache_read);
        assert_eq!(short_spark.fast_multiplier, codex_spark.fast_multiplier);
    }

    #[test]
    fn pricing_lookup_resolves_configured_private_model_alias() {
        let _aliases =
            crate::model_aliases::set_model_aliases_for_tests([("private-gpt-55", "gpt-5.5")]);
        let pricing = PricingMap::load_embedded();

        let private = pricing.find("private-gpt-55").unwrap();
        let canonical = pricing.find("gpt-5.5").unwrap();

        assert_eq!(private.input, canonical.input);
        assert_eq!(private.output, canonical.output);
        assert_eq!(pricing.context_limit("private-gpt-55"), Some(1_050_000));
    }

    #[test]
    fn pricing_lookup_prefers_known_original_model_before_alias() {
        let _aliases =
            crate::model_aliases::set_model_aliases_for_tests([("claude-opus-4-8", "mythos-5")]);
        let pricing = PricingMap::load_embedded();

        let original = pricing.find_entry("claude-opus-4-8").unwrap();
        let resolved = pricing.find("claude-opus-4-8").unwrap();

        assert_eq!(resolved.input, original.input);
        assert_eq!(
            pricing.context_limit("claude-opus-4-8"),
            pricing.context_limit_entry("claude-opus-4-8")
        );
    }

    #[test]
    fn embedded_pricing_includes_claude_fast_multiplier_for_provider_models() {
        let pricing = PricingMap::load_embedded();

        assert_eq!(
            pricing
                .find("anthropic.claude-opus-4-6-v1")
                .unwrap()
                .fast_multiplier,
            6.0
        );
        assert_eq!(
            pricing
                .find("anthropic.claude-opus-4-7")
                .unwrap()
                .fast_multiplier,
            6.0
        );
        assert_eq!(
            pricing
                .find("anthropic.claude-opus-4-8")
                .unwrap()
                .fast_multiplier,
            2.0
        );
    }

    #[test]
    fn embedded_pricing_resolves_opus_47_dot_model_names() {
        let pricing = PricingMap::load_embedded();

        assert_eq!(
            pricing.find("claude-opus-4.7-20260416").unwrap().input,
            5e-6
        );
        assert_eq!(pricing.context_limit("claude-opus-4.7"), Some(1_000_000));
        assert_eq!(
            pricing
                .find("openrouter/anthropic/claude-opus-4.7")
                .unwrap()
                .input,
            5e-6
        );
    }

    #[test]
    fn embedded_pricing_resolves_opus_48_dot_model_names() {
        let pricing = PricingMap::load_embedded();

        let opus_48 = pricing.find("claude-opus-4.8-20260528").unwrap();
        assert_eq!(opus_48.input, 5e-6);
        assert_eq!(opus_48.output, 25e-6);
        assert_eq!(opus_48.cache_create, 6.25e-6);
        assert_eq!(opus_48.cache_read, 0.5e-6);
        assert_eq!(pricing.context_limit("claude-opus-4.8"), Some(1_000_000));
    }

    #[test]
    fn embedded_pricing_resolves_separator_aliases_for_other_claude_models() {
        let pricing = PricingMap::load_embedded();
        let sonnet_46 = pricing.find("claude-sonnet-4-6").unwrap();
        let haiku_45 = pricing.find("claude-haiku-4-5").unwrap();

        assert_eq!(
            pricing.find("claude-sonnet-4.6-20260416").unwrap().input,
            sonnet_46.input
        );
        assert_eq!(
            pricing.find("claude-haiku-4.5").unwrap().input,
            haiku_45.input
        );
        assert_eq!(
            pricing.context_limit("claude-sonnet-4.6"),
            pricing.context_limit("claude-sonnet-4-6")
        );
        assert_eq!(
            pricing.context_limit("claude-haiku-4.5"),
            pricing.context_limit("claude-haiku-4-5")
        );
    }

    #[test]
    fn fuzzy_match_requires_model_key_boundaries() {
        let mut pricing = PricingMap::default();
        pricing.entries.insert(
            "claude-opus-4-7".to_string(),
            Pricing {
                input: 5e-6,
                output: 25e-6,
                cache_create: 6.25e-6,
                cache_read: 0.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        pricing.entries.insert(
            "claude-opus-4".to_string(),
            Pricing {
                input: 15e-6,
                output: 75e-6,
                cache_create: 18.75e-6,
                cache_read: 1.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );

        assert!(pricing.find("claude-opus-4.70").is_none());
    }

    #[test]
    fn fuzzy_match_does_not_fall_back_across_numeric_model_versions() {
        let mut pricing = PricingMap::default();
        pricing.entries.insert(
            "claude-opus-4".to_string(),
            Pricing {
                input: 15e-6,
                output: 75e-6,
                cache_create: 18.75e-6,
                cache_read: 1.5e-6,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );

        assert!(pricing.find("claude-opus-4.8-20260528").is_none());
        assert!(pricing.find("claude-opus-4-9").is_none());
        assert!(pricing.find("claude-opus-5").is_none());
        assert!(pricing.find("claude-opus-4.70").is_none());
        assert!(pricing.find("claude-opus-4-20250514").is_some());
    }

    #[test]
    fn fuzzy_match_allows_date_like_suffixes_for_known_numeric_model_versions() {
        let pricing = PricingMap::load_embedded();

        assert!(pricing.find("claude-opus-4-8-20270898").is_some());
        assert!(pricing.find("claude-opus-4-9").is_none());
        assert!(pricing.find("claude-opus-5").is_none());
    }

    #[test]
    fn fills_codex_fast_multiplier_when_litellm_pricing_omits_it() {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "gpt-5.5": {
                    "input_cost_per_token": 0.000005,
                    "output_cost_per_token": 0.000030,
                    "cache_read_input_token_cost": 0.0000005
                },
                "gpt-5.4": {
                    "input_cost_per_token": 0.0000025,
                    "output_cost_per_token": 0.000015,
                    "cache_read_input_token_cost": 0.00000025
                },
                "gpt-5.3-codex": {
                    "input_cost_per_token": 0.00000175,
                    "output_cost_per_token": 0.000014,
                    "cache_read_input_token_cost": 0.000000175
                },
                "gpt-5.2-codex": {
                    "input_cost_per_token": 0.00000175,
                    "output_cost_per_token": 0.000014,
                    "cache_read_input_token_cost": 0.000000175
                }
            }"#,
        );

        assert_eq!(pricing.find("gpt-5.5").unwrap().fast_multiplier, 2.5);
        assert_eq!(pricing.find("gpt-5.4").unwrap().fast_multiplier, 2.0);
        assert_eq!(pricing.find("gpt-5.3-codex").unwrap().fast_multiplier, 2.0);
        assert_eq!(pricing.find("gpt-5.2-codex").unwrap().fast_multiplier, 1.0);
    }

    #[test]
    fn fills_claude_fast_multiplier_when_litellm_pricing_omits_it() {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "vertex_ai/claude-opus-4-7@default": {
                    "input_cost_per_token": 0.000005,
                    "output_cost_per_token": 0.000025
                },
                "openrouter/anthropic/claude-opus-4.7": {
                    "input_cost_per_token": 0.000005,
                    "output_cost_per_token": 0.000025
                },
                "claude-opus-4.7-20260416": {
                    "input_cost_per_token": 0.000005,
                    "output_cost_per_token": 0.000025
                },
                "claude-opus-4.8-20260528": {
                    "input_cost_per_token": 0.000005,
                    "output_cost_per_token": 0.000025
                },
                "claude-opus-4-70": {
                    "input_cost_per_token": 0.000005,
                    "output_cost_per_token": 0.000025
                }
            }"#,
        );

        assert_eq!(
            pricing
                .find("vertex_ai/claude-opus-4-7@default")
                .unwrap()
                .fast_multiplier,
            6.0
        );
        assert_eq!(
            pricing
                .find("openrouter/anthropic/claude-opus-4.7")
                .unwrap()
                .fast_multiplier,
            6.0
        );
        assert_eq!(
            pricing
                .find("claude-opus-4.7-20260416")
                .unwrap()
                .fast_multiplier,
            6.0
        );
        assert_eq!(
            pricing
                .find("claude-opus-4.8-20260528")
                .unwrap()
                .fast_multiplier,
            2.0
        );
        assert_eq!(
            pricing.find("claude-opus-4-70").unwrap().fast_multiplier,
            1.0
        );
    }

    #[test]
    fn embedded_build_time_pricing_is_compact() {
        assert!(BUILD_TIME_PRICING_JSON.len() < 200_000);
        assert!(!BUILD_TIME_PRICING_JSON.contains("\"source\""));
        assert!(BUILD_TIME_PRICING_JSON.contains("claude-sonnet-4-20250514"));
        assert!(BUILD_TIME_PRICING_JSON.contains("gemini-3-pro-preview"));
        assert!(BUILD_TIME_PRICING_JSON.contains("glm-5"));
    }

    #[test]
    fn fuzzy_match_prefers_longest_model_key() {
        let mut pricing = PricingMap::default();
        pricing.entries.insert(
            "claude-sonnet-4".to_string(),
            Pricing {
                input: 1.0,
                output: 0.0,
                cache_create: 0.0,
                cache_read: 0.0,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );
        pricing.entries.insert(
            "claude-sonnet-4-20250514".to_string(),
            Pricing {
                input: 2.0,
                output: 0.0,
                cache_create: 0.0,
                cache_read: 0.0,
                cache_read_explicit: true,
                input_above_200k: None,
                output_above_200k: None,
                cache_create_above_200k: None,
                cache_read_above_200k: None,
                fast_multiplier: 1.0,
            },
        );

        let matched = pricing
            .find("claude-sonnet-4-20250514-via-bedrock")
            .unwrap();

        assert_eq!(matched.input, 2.0);
    }
}
