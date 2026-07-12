# ccusage slopfork

This is a fork of [ryoppippi/ccusage](https://github.com/ryoppippi/ccusage) trimmed for my actual use case:

- one report across multiple coding-agent providers
- better performance with very large local session histories

The main command is:

```bash
ccusage daily --tool all
```

This fork also publishes the `ccstats` command alias, so the same report works as:

```bash
ccstats daily --tool all
```

Useful variants:

```bash
ccusage daily --tool all --offline
ccusage daily --tool all --json
ccusage daily --tool all --by-provider
ccusage monthly --tool all --by-model
ccusage monthly --tool all --by-model --model opus
ccusage session --tool all
ccusage monthly --tool all
```

`--by-model` canonicalizes model aliases across providers, so pi usage such as
`[pi] anthropic/claude-opus-4.6` is pooled with Claude Code usage reported as
`claude-opus-4-6`. Use `--model opus` to filter every provider to the Opus
family, or pass an exact model such as `--model opus-4-6`.

Providers currently covered in this fork include Claude, NCode, ZCode, Codex, OpenCode, pi, and Amp.

If you want the original project, use the upstream repo instead:

- [ryoppippi/ccusage](https://github.com/ryoppippi/ccusage)
