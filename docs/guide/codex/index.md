# Codex Data Source (Beta)

![ccusage daily report focused on Codex usage](/codex-cli.jpeg)

> ⚠️ Codex log support is experimental while the Codex CLI log format continues to evolve.

ccusage can read OpenAI Codex CLI session logs as one of its supported local data sources. Codex uses the same unified and focused report model as Claude Code, NCode, ZCode, OpenCode, Amp, Droid, Codebuff, Hermes Agent, pi-agent, Goose, OpenClaw, Kilo, Kimi, Qwen, GitHub Copilot CLI, and Gemini CLI.

## Focused Views

```bash
# Daily Codex usage
ccusage codex daily

# Monthly Codex usage
ccusage codex monthly

# Codex sessions
ccusage codex session
```

Most users can start with unified reports such as `ccusage daily`. Add the `codex` namespace only when you want to focus the same report shape on Codex usage or pass Codex-specific options such as `--speed`.

## Data Source

The CLI reads Codex session JSONL files located under `CODEX_HOME` (defaults to `~/.codex`). `CODEX_HOME` can be one directory or a comma-separated list of directories. Entries with a `sessions/` directory are treated as Codex homes; entries without `sessions/` are read directly as JSONL directories, which lets saved `codex exec --json` output live beside normal Codex homes.

```bash
CODEX_HOME="$HOME/.codex,$HOME/.codex-work,$HOME/codex-exec-logs" ccusage codex daily
```

## Report Views

| Focused view            | Description                  | See also                                |
| ----------------------- | ---------------------------- | --------------------------------------- |
| `ccusage codex daily`   | Aggregate usage by date      | [Daily Usage](/guide/daily-reports)     |
| `ccusage codex monthly` | Aggregate usage by month     | [Monthly Usage](/guide/monthly-reports) |
| `ccusage codex session` | Group usage by Codex session | [Session Usage](/guide/session-reports) |

These views support `--json`, `--compact`, `--offline`, and `--speed auto|standard|fast`.

## Monthly Example

![ccusage monthly report focused on Codex usage](/codex-cli-monthly.jpeg)

## What Gets Calculated

- **Token deltas** – Each `event_msg` with `payload.type === "token_count"` reports cumulative totals. The CLI subtracts the previous totals to recover per-turn token usage (input, cache write, cached input, output, reasoning, total).
- **Per-model grouping** – The `turn_context` metadata specifies the active model. We aggregate tokens per day/month and per model. Sessions lacking model metadata (seen in early September 2025 builds) are skipped.
- **Pricing** – Rates come from the embedded and refreshed pricing datasets. Offline pricing includes `gpt-5.6`, `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`.
- **Speed pricing** – `--speed auto` is the default. It reads `config.toml` from each `CODEX_HOME` root and applies fast pricing when any Codex config has `service_tier = "priority"` or legacy `service_tier = "fast"` configured. Fast mode uses the model-specific LiteLLM multiplier when available and otherwise falls back to 2x pricing. Pass `--speed fast` or `--speed standard` to override config-based detection.
- **Legacy fallback** – Early September 2025 logs that never recorded `turn_context` metadata are still included; the CLI assumes `gpt-5` for pricing so you can review the tokens even though the model tag is missing (the JSON output also marks these rows with `"isFallback": true`).
- **Cost formula** – Regular input uses the standard input price, GPT-5.6 cache writes use 1.25x the input price, cached input uses the cache-read price (or the input price when the pricing source omits a cache-read rate), and output tokens use the output price. All prices are per million tokens. Reasoning tokens are shown for reference and remain part of the output charge.
- **Totals and reports** – Daily, monthly, and session views display per-model breakdowns, overall totals, and optional JSON for automation.

## Environment Variables

| Variable     | Description                                                                                                                  |
| ------------ | ---------------------------------------------------------------------------------------------------------------------------- |
| `CODEX_HOME` | Override the root directory, or comma-separated directories, containing Codex homes or saved `codex exec --json` JSONL files |
| `LOG_LEVEL`  | Adjust log verbosity (0 silent … 5 trace)                                                                                    |

When Codex emits a model alias such as `gpt-5.6`, the CLI resolves it to the corresponding model-family price.

## Speed Pricing

Codex logs usually do not include whether a turn used fast mode. By default, `ccusage codex` uses `--speed auto`, reads `config.toml` from each `CODEX_HOME` root, and treats `service_tier = "priority"` or legacy `service_tier = "fast"` as fast pricing when any configured root opts into it. Fast mode uses the model-specific LiteLLM multiplier when available and otherwise falls back to 2x pricing.

```bash
# Default: read Codex config.toml
ccusage codex daily --speed auto

# Force fast pricing
ccusage codex daily --speed fast

# Force standard pricing
ccusage codex daily --speed standard
```

## JSON Output

Codex focused views use the same JSON mode as the shared reports:

```bash
ccusage codex daily --json
ccusage codex monthly --json
ccusage codex session --json
```

Session JSON includes per-model breakdowns, `cacheWriteInputTokens`, cached token counts, `lastActivity`, and `isFallback` flags for events that required the legacy `gpt-5` pricing fallback.

Have feedback or ideas? [Open an issue](https://github.com/ryoppippi/ccusage/issues/new) so we can improve Codex support.

## Troubleshooting

::: details Why do GPT-5.6 cache writes show zero?
Current Codex session `token_count` events omit the API's `cache_write_tokens` field. ccusage reads that field whenever a session records it; existing events therefore contribute zero cache-write tokens. The OpenAI API reports cache writes separately from cache reads as described in the [prompt caching guide](https://developers.openai.com/api/docs/guides/prompt-caching).
:::

::: details Why are there no entries before September 2025?
OpenAI's Codex CLI started emitting `token_count` events in [commit 0269096](https://github.com/openai/codex/commit/0269096229e8c8bd95185173706807dc10838c7a) (2025-09-06). Earlier session logs simply don't contain token usage metrics, so `ccusage codex` has nothing to aggregate. If you need historic data, rerun those sessions after that Codex update.
:::

::: details What if some September 2025 sessions still get skipped?
During the 2025-09 rollouts a few Codex builds emitted `token_count` events without the matching `turn_context` metadata, so the CLI could not determine which model generated the tokens. Those entries are ignored to avoid mispriced reports. If you encounter this, relaunch the Codex CLI to generate fresh logs—the current builds restore the missing metadata.
:::
