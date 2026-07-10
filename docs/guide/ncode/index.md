# NCode Data Source

ccusage reads NCode's Claude-compatible JSONL transcripts as a separate source. Unified reports show NCode under the `ncode` source key, and focused reports use the same daily, monthly, and session views as other standard sources.

## Focused Views

```bash
ccusage ncode daily
ccusage ncode monthly
ccusage ncode session
```

## Data Source

| Source | Default path         |
| ------ | -------------------- |
| NCode  | `~/.ncode/projects/` |

Set `NCODE_CONFIG_DIR` when NCode logs live outside the default location:

```bash
export NCODE_CONFIG_DIR="/path/to/ncode"
ccusage ncode daily
```

Use comma-separated directories to combine current and archived NCode data:

```bash
export NCODE_CONFIG_DIR="~/.ncode,/backup/ncode-archive"
ccusage ncode monthly
```

`NCODE_CONFIG_DIR` accepts config directories containing `projects/` and direct `projects/` paths.

## Unified Reports

NCode is included in all-source reports:

```bash
ccusage daily --tool all --by-model
ccusage daily --tool ncode --by-model
```

The focused `ccusage ncode ...` command and `--tool ncode` filter both report the same NCode source.
