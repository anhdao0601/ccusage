# ZCode Data Source

ccusage reads request-level model usage and session metadata from [ZCode](https://zcode.z.ai/en), Z.AI's coding application for GLM models and other providers.

## Focused Views

```bash
ccusage zcode daily
ccusage zcode monthly
ccusage zcode session
```

Use `ccusage zcode session` to group every recorded model request under its ZCode session ID. Unified reports include the same source as `zcode`:

```bash
ccusage daily --tool all --by-model
ccusage daily --tool zcode --by-model
```

## Data Source

ZCode stores sessions and request-level usage in SQLite:

```text
~/.zcode/cli/db/db.sqlite
```

ccusage reads the `model_usage` table and joins its rows to the `session` table. This includes main turns, subagent calls, session-title calls, and every request attempt that recorded token usage. The database is opened in SQLite query-only mode.

Set `ZCODE_DATA_DIR` when the ZCode data root lives elsewhere. Comma-separated roots combine current and archived databases:

```bash
export ZCODE_DATA_DIR="$HOME/.zcode,/backup/zcode"
ccusage zcode session
```

Each root must contain `cli/db/db.sqlite`.

When `ZCODE_DATA_DIR` is unset, ccusage honors ZCode's native `ZCODE_STORAGE_DIR` override before using `~/.zcode`.

## Token Mapping

ZCode records cached input inside `input_tokens`, so ccusage separates the categories before aggregation and pricing:

| ccusage field | ZCode source                                                           |
| ------------- | ---------------------------------------------------------------------- |
| Regular input | `input_tokens - cache_creation_input_tokens - cache_read_input_tokens` |
| Cache create  | `cache_creation_input_tokens`                                          |
| Cache read    | `cache_read_input_tokens`                                              |
| Output        | `output_tokens`                                                        |
| Model         | `model_id`                                                             |
| Session       | `session_id`                                                           |

This split keeps total tokens and estimated cost from counting cached prompt tokens twice. ZCode model identifiers such as `GLM-5.2` and Z.AI Hugging Face model paths resolve through ccusage's embedded GLM pricing. See [Z.AI pricing](https://docs.z.ai/guides/overview/pricing) for the provider's current public rates.

## JSON Output

```bash
ccusage zcode session --json
```

The JSON report includes `inputTokens`, `outputTokens`, `cacheCreationTokens`, `cacheReadTokens`, `totalTokens`, `totalCost`, `modelsUsed`, and `messageCount` for each session.
