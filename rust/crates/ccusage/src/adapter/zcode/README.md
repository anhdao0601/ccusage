# ZCode Source

Default database:

```text
${ZCODE_DATA_DIR:-~/.zcode}/cli/db/db.sqlite
```

`ZCODE_DATA_DIR` accepts comma-separated ZCode roots. When it is unset, the adapter honors ZCode's native `ZCODE_STORAGE_DIR` before using `~/.zcode`.

The `model_usage` table is the authoritative request-level source. ZCode records cached tokens inside `input_tokens`; this adapter subtracts cache writes and reads from that value before mapping the three input categories to ccusage.
