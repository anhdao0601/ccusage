const TOOL_NAMES = ['claude', 'codex', 'opencode', 'pi', 'amp', 'hermes'] as const;

export type ToolName = (typeof TOOL_NAMES)[number];

export const ALL_TOOL_NAMES = [...TOOL_NAMES] as ToolName[];

export function parseToolArgument(value: string): string {
	return value.trim();
}

export function hasExplicitToolSelection(value: unknown): value is string {
	return typeof value === 'string' && value.trim() !== '';
}

export function normalizeToolSelection(value: string | undefined): ToolName[] {
	if (value == null || value.trim() === '') {
		return ['claude'];
	}

	const rawValues = value
		.split(',')
		.map((entry) => entry.trim().toLowerCase())
		.filter((entry) => entry !== '');

	if (rawValues.length === 0) {
		throw new Error('`--tool` requires at least one tool name.');
	}

	if (rawValues.includes('all')) {
		if (rawValues.length > 1) {
			throw new Error('`all` cannot be combined with other tool names.');
		}
		return [...ALL_TOOL_NAMES];
	}

	const seen = new Set<ToolName>();
	for (const rawValue of rawValues) {
		if (!TOOL_NAMES.includes(rawValue as ToolName)) {
			throw new Error(
				`Unsupported tool "${rawValue}". Expected one of: ${['all', ...TOOL_NAMES].join(', ')}.`,
			);
		}
		seen.add(rawValue as ToolName);
	}

	return [...seen];
}

export function assertClaudeOnlyTools(tools: readonly ToolName[], commandName: string): void {
	if (tools.length === 1 && tools[0] === 'claude') {
		return;
	}

	throw new Error(
		`The "${commandName}" command only supports Claude data in this release. Omit \`--tool\` or use \`--tool claude\`.`,
	);
}

if (import.meta.vitest != null) {
	const { describe, expect, it } = import.meta.vitest;

	describe('parseToolArgument', () => {
		it('trims surrounding whitespace', () => {
			expect(parseToolArgument('  codex,claude  ')).toBe('codex,claude');
		});
	});

	describe('normalizeToolSelection', () => {
		it('defaults to claude when tool is omitted', () => {
			expect(normalizeToolSelection(undefined)).toEqual(['claude']);
		});

		it('expands all', () => {
			expect(normalizeToolSelection('all')).toEqual(ALL_TOOL_NAMES);
		});

		it('deduplicates comma-separated tool lists', () => {
			expect(normalizeToolSelection('codex,claude,codex')).toEqual(['codex', 'claude']);
		});

		it('throws on unsupported names', () => {
			expect(() => normalizeToolSelection('gemini')).toThrow('Unsupported tool');
		});

		it('throws when all is combined with other names', () => {
			expect(() => normalizeToolSelection('all,codex')).toThrow(
				'`all` cannot be combined with other tool names.',
			);
		});
	});

	describe('assertClaudeOnlyTools', () => {
		it('accepts omitted/default claude mode', () => {
			expect(() => assertClaudeOnlyTools(['claude'], 'weekly')).not.toThrow();
		});

		it('rejects non-claude selections for claude-only commands', () => {
			expect(() => assertClaudeOnlyTools(['codex'], 'weekly')).toThrow('only supports Claude data');
		});
	});
}
