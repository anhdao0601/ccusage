# Codex Source

Data source:

```text
${CODEX_HOME:-~/.codex}/sessions/
```

Relevant JSONL event:

- `type === "event_msg"`
- `payload.type === "token_count"`
- `payload.info.total_token_usage` is cumulative.
- `payload.info.last_token_usage` is the current turn delta.
- If only cumulative totals exist, subtract prior totals to recover deltas.

Token mapping:

- `input_tokens` - total input tokens.
- `cached_input_tokens` - cached prompt tokens.
- `cache_write_tokens` - prompt tokens written to cache, when Codex records them.
- `output_tokens` - completion tokens, including reasoning cost.
- `reasoning_output_tokens` - informational breakdown; already included in output billing.
- `total_tokens` - provided directly or recomputed as input plus output for legacy entries.

Pricing uses model metadata from `turn_context`. GPT-5.6 cache writes use the
model's cache creation rate. Sessions that omit `cache_write_tokens` report zero
cache writes. Early sessions without metadata fall back to `gpt-5`, mark
`isFallbackModel === true`, and expose fallback rows as approximate in aggregate
JSON.
