# ccusage slopfork

This is a fork of [ryoppippi/ccusage](https://github.com/ryoppippi/ccusage) trimmed for my actual use case:

- one report across multiple coding-agent providers
- better performance with very large local session histories

The main command is:

```bash
ccusage daily --tool all
```

Useful variants:

```bash
ccusage daily --tool all --offline
ccusage daily --tool all --json
ccusage session --tool all
ccusage monthly --tool all
```

Providers currently covered in this fork include Claude, Codex, OpenCode, pi, and Amp.

If you want the original project, use the upstream repo instead:

- [ryoppippi/ccusage](https://github.com/ryoppippi/ccusage)
