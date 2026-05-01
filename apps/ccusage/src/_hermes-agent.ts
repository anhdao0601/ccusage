/**
 * @fileoverview Hermes Agent data loader
 *
 * Reads usage data from Hermes Agent's SQLite database (~/.hermes/state.db).
 * Uses the sqlite3 CLI tool for portable database access (works in both bun and node).
 * Hermes stores per-session aggregated token counts (not per-message).
 */

import { execFileSync } from 'node:child_process';
import path from 'node:path';
import process from 'node:process';
import { isDirectorySync } from 'path-type';

const USER_HOME_DIR = process.env.HOME ?? process.env.USERPROFILE ?? '';
const DEFAULT_HERMES_PATH = '.hermes';
const HERMES_DB_FILENAME = 'state.db';
const HERMES_DIR_ENV = 'HERMES_AGENT_DIR';

type HermesSessionRow = {
	id: string;
	source: string;
	model: string | null;
	started_at: number;
	ended_at: number | null;
	input_tokens: number;
	output_tokens: number;
	cache_read_tokens: number;
	cache_write_tokens: number;
	reasoning_tokens: number;
	estimated_cost_usd: number | null;
	actual_cost_usd: number | null;
	title: string | null;
	api_call_count: number;
};

export type HermesEntryData = {
	sessionId: string;
	timestamp: string;
	model: string;
	rawModel: string;
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	cost: number;
	source: string;
	title: string | null;
};

export type HermesLoadOptions = {
	hermesPath?: string;
	since?: string;
	until?: string;
	timezone?: string;
	order?: 'asc' | 'desc';
};

/**
 * Get the path to the Hermes Agent state database
 */
export function getHermesDbPath(customPath?: string): string | null {
	if (customPath != null && customPath !== '') {
		const resolved = path.resolve(customPath);
		try {
			if (isDirectorySync(resolved)) {
				return path.join(resolved, HERMES_DB_FILENAME);
			}
		} catch {
			// ignore
		}
		return null;
	}

	const envPath = (process.env[HERMES_DIR_ENV] ?? '').trim();
	if (envPath !== '') {
		const resolved = path.resolve(envPath);
		if (isDirectorySync(resolved)) {
			return path.join(resolved, HERMES_DB_FILENAME);
		}
	}

	const defaultPath = path.join(USER_HOME_DIR, DEFAULT_HERMES_PATH);
	if (isDirectorySync(defaultPath)) {
		return path.join(defaultPath, HERMES_DB_FILENAME);
	}

	return null;
}

/**
 * Execute a sqlite3 query and parse JSON output
 */
function querySqlite(dbPath: string, sql: string): HermesSessionRow[] {
	try {
		const output = execFileSync('sqlite3', ['-json', dbPath, sql], {
			encoding: 'utf-8',
			timeout: 10_000,
			stdio: ['pipe', 'pipe', 'pipe'],
		});

		const trimmed = output.trim();
		if (trimmed === '' || trimmed === '[]') {
			return [];
		}

		return JSON.parse(trimmed) as HermesSessionRow[];
	} catch {
		// sqlite3 CLI not available or DB unreadable - silently skip
		return [];
	}
}

/**
 * Convert Unix timestamp (seconds) to ISO date string
 */
function unixToISODate(unixSeconds: number, timezone?: string): string {
	const date = new Date(unixSeconds * 1000);
	const tz = timezone ?? Intl.DateTimeFormat().resolvedOptions().timeZone;
	return date.toLocaleDateString('en-CA', { timeZone: tz });
}

/**
 * Convert Unix timestamp (seconds) to ISO month string (YYYY-MM)
 */
function unixToISOMonth(unixSeconds: number, timezone?: string): string {
	return unixToISODate(unixSeconds, timezone).slice(0, 7);
}

function normalizeDate(value: string): string {
	return value.replace(/-/g, '');
}

function isInDateRange(date: string, since?: string, until?: string): boolean {
	const dateKey = normalizeDate(date);
	if (since != null && dateKey < normalizeDate(since)) {
		return false;
	}
	if (until != null && dateKey > normalizeDate(until)) {
		return false;
	}
	return true;
}

/**
 * Normalize Hermes model names for display
 * e.g. "eu.anthropic.claude-opus-4-6-v1" -> "[hermes] claude-opus-4-6"
 */
function stripProviderPrefix(rawModel: string): string {
	return rawModel.replace(
		/^(?:(?:global|us|eu|au|ap|aws|azure)\.)?(?:anthropic|openai|google|vertex|xai|meta|mistral)\./,
		'',
	);
}

function normalizeModelName(rawModel: string | null): string {
	if (rawModel == null || rawModel === '') {
		return '[hermes] unknown';
	}

	let model = stripProviderPrefix(rawModel);

	// Strip provider version suffixes like Claude Bedrock "-v1".
	model = model.replace(/-v\d+$/, '');

	return `[hermes] ${model}`;
}

/**
 * Convert a raw Hermes model name to a LiteLLM-compatible pricing key.
 * e.g. "eu.anthropic.claude-opus-4-6-v1" -> "claude-opus-4-6"
 * This stripped name is tried directly and with "anthropic/" prefix for pricing lookup.
 */
export function toLiteLLMModelName(rawModel: string): string {
	if (rawModel === '') {
		return '';
	}

	let model = stripProviderPrefix(rawModel);

	// Strip provider version suffixes like Claude Bedrock "-v1".
	model = model.replace(/-v\d+$/, '');

	return model;
}

/**
 * Load all Hermes sessions from the SQLite database
 */
export function loadHermesData(options?: HermesLoadOptions): HermesEntryData[] {
	const dbPath = getHermesDbPath(options?.hermesPath);
	if (dbPath == null) {
		return [];
	}

	const rows = querySqlite(
		dbPath,
		`SELECT
			id, source, model, started_at, ended_at,
			input_tokens, output_tokens,
			cache_read_tokens, cache_write_tokens,
			reasoning_tokens,
			estimated_cost_usd, actual_cost_usd,
			title, api_call_count
		FROM sessions
		WHERE input_tokens > 0 OR output_tokens > 0 OR cache_read_tokens > 0
		ORDER BY started_at DESC`,
	);

	const entries: HermesEntryData[] = [];

	for (const row of rows) {
		const date = unixToISODate(row.started_at, options?.timezone);
		if (!isInDateRange(date, options?.since, options?.until)) {
			continue;
		}

		// Use actual cost if available, otherwise estimated
		const cost = row.actual_cost_usd ?? row.estimated_cost_usd ?? 0;

		entries.push({
			sessionId: row.id,
			timestamp: new Date(row.started_at * 1000).toISOString(),
			model: normalizeModelName(row.model),
			rawModel: row.model ?? '',
			inputTokens: row.input_tokens,
			outputTokens: row.output_tokens,
			cacheCreationTokens: row.cache_write_tokens,
			cacheReadTokens: row.cache_read_tokens,
			cost,
			source: row.source,
			title: row.title,
		});
	}

	return entries;
}

type ModelBreakdown = {
	modelName: string;
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	cost: number;
};

function aggregateByModel(entries: HermesEntryData[]): Map<string, ModelBreakdown> {
	const modelMap = new Map<string, ModelBreakdown>();

	for (const entry of entries) {
		const existing = modelMap.get(entry.model) ?? {
			modelName: entry.model,
			inputTokens: 0,
			outputTokens: 0,
			cacheCreationTokens: 0,
			cacheReadTokens: 0,
			cost: 0,
		};

		existing.inputTokens += entry.inputTokens;
		existing.outputTokens += entry.outputTokens;
		existing.cacheCreationTokens += entry.cacheCreationTokens;
		existing.cacheReadTokens += entry.cacheReadTokens;
		existing.cost += entry.cost;

		modelMap.set(entry.model, existing);
	}

	return modelMap;
}

function calculateTotals(entries: HermesEntryData[]): {
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	totalCost: number;
} {
	let inputTokens = 0;
	let outputTokens = 0;
	let cacheCreationTokens = 0;
	let cacheReadTokens = 0;
	let totalCost = 0;

	for (const entry of entries) {
		inputTokens += entry.inputTokens;
		outputTokens += entry.outputTokens;
		cacheCreationTokens += entry.cacheCreationTokens;
		cacheReadTokens += entry.cacheReadTokens;
		totalCost += entry.cost;
	}

	return { inputTokens, outputTokens, cacheCreationTokens, cacheReadTokens, totalCost };
}

export type HermesDailyUsage = {
	date: string;
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	totalCost: number;
	modelsUsed: string[];
	modelBreakdowns: ModelBreakdown[];
};

export type HermesSessionUsage = {
	sessionId: string;
	displayName: string;
	lastActivity: string;
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	totalCost: number;
	modelsUsed: string[];
	modelBreakdowns: ModelBreakdown[];
};

export type HermesMonthlyUsage = {
	month: string;
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	totalCost: number;
	modelsUsed: string[];
	modelBreakdowns: ModelBreakdown[];
};

export function loadHermesDailyData(options?: HermesLoadOptions): HermesDailyUsage[] {
	const entries = loadHermesData(options);

	const grouped = new Map<string, HermesEntryData[]>();
	for (const entry of entries) {
		const date = unixToISODate(new Date(entry.timestamp).getTime() / 1000, options?.timezone);
		const existing = grouped.get(date) ?? [];
		existing.push(entry);
		grouped.set(date, existing);
	}

	const results: HermesDailyUsage[] = [];
	for (const [date, dateEntries] of grouped) {
		const modelMap = aggregateByModel(dateEntries);
		const totals = calculateTotals(dateEntries);
		const modelsUsed = Array.from(modelMap.keys());
		const modelBreakdowns = Array.from(modelMap.values());

		results.push({
			date,
			...totals,
			modelsUsed,
			modelBreakdowns,
		});
	}

	const order = options?.order ?? 'desc';
	results.sort((a, b) =>
		order === 'asc' ? a.date.localeCompare(b.date) : b.date.localeCompare(a.date),
	);

	return results;
}

export function loadHermesSessionData(options?: HermesLoadOptions): HermesSessionUsage[] {
	const entries = loadHermesData(options);

	const results: HermesSessionUsage[] = [];
	for (const entry of entries) {
		const date = unixToISODate(new Date(entry.timestamp).getTime() / 1000, options?.timezone);
		const modelMap = aggregateByModel([entry]);
		const modelsUsed = Array.from(modelMap.keys());
		const modelBreakdowns = Array.from(modelMap.values());

		// Use title if available, otherwise use source/sessionId
		const displayName = entry.title ?? `${entry.source}/${entry.sessionId}`;

		results.push({
			sessionId: entry.sessionId,
			displayName,
			lastActivity: date,
			inputTokens: entry.inputTokens,
			outputTokens: entry.outputTokens,
			cacheCreationTokens: entry.cacheCreationTokens,
			cacheReadTokens: entry.cacheReadTokens,
			totalCost: entry.cost,
			modelsUsed,
			modelBreakdowns,
		});
	}

	const order = options?.order ?? 'desc';
	results.sort((a, b) =>
		order === 'asc'
			? a.lastActivity.localeCompare(b.lastActivity)
			: b.lastActivity.localeCompare(a.lastActivity),
	);

	return results;
}

export function loadHermesMonthlyData(options?: HermesLoadOptions): HermesMonthlyUsage[] {
	const entries = loadHermesData(options);

	const grouped = new Map<string, HermesEntryData[]>();
	for (const entry of entries) {
		const month = unixToISOMonth(new Date(entry.timestamp).getTime() / 1000, options?.timezone);
		const existing = grouped.get(month) ?? [];
		existing.push(entry);
		grouped.set(month, existing);
	}

	const results: HermesMonthlyUsage[] = [];
	for (const [month, monthEntries] of grouped) {
		const modelMap = aggregateByModel(monthEntries);
		const totals = calculateTotals(monthEntries);
		const modelsUsed = Array.from(modelMap.keys());
		const modelBreakdowns = Array.from(modelMap.values());

		results.push({
			month,
			...totals,
			modelsUsed,
			modelBreakdowns,
		});
	}

	const order = options?.order ?? 'desc';
	results.sort((a, b) =>
		order === 'asc' ? a.month.localeCompare(b.month) : b.month.localeCompare(a.month),
	);

	return results;
}

if (import.meta.vitest != null) {
	describe('Hermes Agent model normalization', () => {
		it('keeps decimal model versions intact', () => {
			expect(normalizeModelName('gpt-5.5')).toBe('[hermes] gpt-5.5');
			expect(toLiteLLMModelName('gpt-5.5')).toBe('gpt-5.5');
		});

		it('strips known provider prefixes without treating decimal versions as prefixes', () => {
			expect(normalizeModelName('eu.anthropic.claude-opus-4-6-v1')).toBe(
				'[hermes] claude-opus-4-6',
			);
			expect(toLiteLLMModelName('eu.anthropic.claude-opus-4-6-v1')).toBe('claude-opus-4-6');
			expect(normalizeModelName('openai.gpt-5.5')).toBe('[hermes] gpt-5.5');
			expect(toLiteLLMModelName('openai.gpt-5.5')).toBe('gpt-5.5');
		});
	});
}
