import type { ToolName } from './_tool-selection.ts';
import process from 'node:process';
import { mapWithConcurrency } from '@ccusage/internal/concurrency';
import { LiteLLMPricingFetcher } from '@ccusage/internal/pricing';
import {
	addEmptySeparatorRow,
	formatCurrency,
	formatDateCompact,
	formatModelsDisplayMultiline,
	formatNumber,
	ResponsiveTable,
} from '@ccusage/terminal/table';
import pc from 'picocolors';
import { loadAmpUsageEvents } from '../../amp/src/data-loader.ts';
import { AmpPricingSource } from '../../amp/src/pricing.ts';
import { formatModelsList, splitUsageTokens } from '../../codex/src/command-utils.ts';
import { loadTokenUsageEvents as loadCodexTokenUsageEvents } from '../../codex/src/data-loader.ts';
import {
	isWithinRange as isWithinCodexRange,
	toDateKey,
	toMonthKey,
} from '../../codex/src/date-utils.ts';
import { CodexPricingSource } from '../../codex/src/pricing.ts';
import { buildSessionReport as buildCodexSessionReport } from '../../codex/src/session-report.ts';
import { addUsage, calculateCostUSD, createEmptyUsage } from '../../codex/src/token-utils.ts';
import { loadOpenCodeMessages, loadOpenCodeSessions } from '../../opencode/src/data-loader.ts';
import {
	loadPiAgentDailyData,
	loadPiAgentMonthlyData,
	loadPiAgentSessionData,
} from '../../pi/src/data-loader.ts';
import { sortByDate } from './_date-utils.ts';
import { getTotalTokens } from './calculate-cost.ts';
import { loadDailyUsageData, loadMonthlyUsageData, loadSessionData } from './data-loader.ts';

const MULTI_TOOL_CONCURRENCY = Math.max(
	2,
	Math.min(8, Math.ceil((process.stdout.columns ?? 8) / 20)),
);
const FILE_PARSE_CONCURRENCY = 8;

type RowMetrics = {
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	totalTokens: number;
	totalCost: number;
	reasoningOutputTokens?: number;
	credits?: number;
};

type NormalizedModelBreakdown = {
	modelName: string;
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	totalTokens: number;
	cost: number;
	isFallback?: boolean;
	reasoningOutputTokens?: number;
};

export type MultiToolDailyRow = RowMetrics & {
	source: ToolName;
	date: string;
	modelsUsed: string[];
	modelBreakdowns: NormalizedModelBreakdown[];
};

export type MultiToolMonthlyRow = RowMetrics & {
	source: ToolName;
	month: string;
	modelsUsed: string[];
	modelBreakdowns: NormalizedModelBreakdown[];
};

export type MultiToolSessionRow = RowMetrics & {
	source: ToolName;
	sessionId: string;
	displayName: string;
	lastActivity: string;
	modelsUsed: string[];
	modelBreakdowns: NormalizedModelBreakdown[];
};

type MultiToolReport<Row> = {
	rows: Row[];
	totals: RowMetrics;
	totalsBySource: Partial<Record<ToolName, RowMetrics>>;
	includeReasoning: boolean;
	includeCredits: boolean;
};

type MultiToolReportOptions = {
	tools: readonly ToolName[];
	since?: string;
	until?: string;
	timezone?: string;
	locale?: string;
	order?: 'asc' | 'desc';
	offline?: boolean;
	updatePricing?: boolean;
	claudePath?: string;
	codexSessionDirs?: string[];
	piPath?: string;
	ampThreadDirs?: string[];
};

type ModelAggregate = {
	inputTokens: number;
	outputTokens: number;
	cacheCreationTokens: number;
	cacheReadTokens: number;
	totalTokens: number;
	reasoningOutputTokens: number;
	directCost: number;
	isFallback: boolean;
};

type MutableMetrics = Required<RowMetrics>;

function createEmptyMetrics(): MutableMetrics {
	return {
		inputTokens: 0,
		outputTokens: 0,
		cacheCreationTokens: 0,
		cacheReadTokens: 0,
		totalTokens: 0,
		totalCost: 0,
		reasoningOutputTokens: 0,
		credits: 0,
	};
}

function addMetrics(target: MutableMetrics, metrics: RowMetrics): void {
	target.inputTokens += metrics.inputTokens;
	target.outputTokens += metrics.outputTokens;
	target.cacheCreationTokens += metrics.cacheCreationTokens;
	target.cacheReadTokens += metrics.cacheReadTokens;
	target.totalTokens += metrics.totalTokens;
	target.totalCost += metrics.totalCost;
	target.reasoningOutputTokens += metrics.reasoningOutputTokens ?? 0;
	target.credits += metrics.credits ?? 0;
}

function toRowMetrics(metrics: MutableMetrics): RowMetrics {
	return {
		inputTokens: metrics.inputTokens,
		outputTokens: metrics.outputTokens,
		cacheCreationTokens: metrics.cacheCreationTokens,
		cacheReadTokens: metrics.cacheReadTokens,
		totalTokens: metrics.totalTokens,
		totalCost: metrics.totalCost,
		...(metrics.reasoningOutputTokens > 0 && {
			reasoningOutputTokens: metrics.reasoningOutputTokens,
		}),
		...(metrics.credits > 0 && { credits: metrics.credits }),
	};
}

function createCodexFilterDate(value?: string): string | undefined {
	if (value == null || value.length !== 8) {
		return value;
	}

	return `${value.slice(0, 4)}-${value.slice(4, 6)}-${value.slice(6, 8)}`;
}

function createModelAggregate(): ModelAggregate {
	return {
		inputTokens: 0,
		outputTokens: 0,
		cacheCreationTokens: 0,
		cacheReadTokens: 0,
		totalTokens: 0,
		reasoningOutputTokens: 0,
		directCost: 0,
		isFallback: false,
	};
}

function normalizeClaudeBreakdowns(
	breakdowns: Array<{
		modelName: string;
		inputTokens: number;
		outputTokens: number;
		cacheCreationTokens: number;
		cacheReadTokens: number;
		cost: number;
	}>,
): NormalizedModelBreakdown[] {
	return breakdowns.map((breakdown) => ({
		...breakdown,
		totalTokens:
			breakdown.inputTokens +
			breakdown.outputTokens +
			breakdown.cacheCreationTokens +
			breakdown.cacheReadTokens,
	}));
}

async function createCodexBreakdowns(
	models: Record<
		string,
		{
			inputTokens: number;
			cachedInputTokens: number;
			outputTokens: number;
			reasoningOutputTokens: number;
			totalTokens: number;
			isFallback?: boolean;
		}
	>,
	getPricing: (modelName: string) => Promise<Awaited<ReturnType<CodexPricingSource['getPricing']>>>,
): Promise<NormalizedModelBreakdown[]> {
	const modelEntries = Object.entries(models);
	return mapWithConcurrency(modelEntries, FILE_PARSE_CONCURRENCY, async ([modelName, usage]) => {
		const pricing = await getPricing(modelName);
		const split = splitUsageTokens(usage);
		return {
			modelName,
			inputTokens: split.inputTokens,
			outputTokens: split.outputTokens,
			cacheCreationTokens: 0,
			cacheReadTokens: split.cacheReadTokens,
			totalTokens: usage.totalTokens,
			cost: calculateCostUSD(usage, pricing),
			reasoningOutputTokens: split.reasoningTokens,
			...(usage.isFallback === true && { isFallback: true }),
		};
	});
}

function createCachedCodexPricingResolver(
	pricingSource: CodexPricingSource,
): (modelName: string) => Promise<Awaited<ReturnType<CodexPricingSource['getPricing']>>> {
	const cache = new Map<string, Promise<Awaited<ReturnType<CodexPricingSource['getPricing']>>>>();

	return async (modelName: string) => {
		const existing = cache.get(modelName);
		if (existing != null) {
			return existing;
		}

		const pending = pricingSource.getPricing(modelName);
		cache.set(modelName, pending);
		return pending;
	};
}

function normalizeCodexRowUsage(
	usage: Parameters<typeof splitUsageTokens>[0] & { totalTokens: number },
): Pick<
	RowMetrics,
	| 'inputTokens'
	| 'outputTokens'
	| 'cacheCreationTokens'
	| 'cacheReadTokens'
	| 'totalTokens'
	| 'reasoningOutputTokens'
> {
	const split = splitUsageTokens(usage);

	return {
		inputTokens: split.inputTokens,
		outputTokens: split.outputTokens,
		cacheCreationTokens: 0,
		cacheReadTokens: split.cacheReadTokens,
		totalTokens: usage.totalTokens,
		reasoningOutputTokens: split.reasoningTokens,
	};
}

function createMultiToolReport<Row extends RowMetrics & { source: ToolName }>(
	rows: Row[],
	_tools: readonly ToolName[],
): MultiToolReport<Row> {
	const totals = createEmptyMetrics();
	const totalsBySource: Partial<Record<ToolName, MutableMetrics>> = {};

	for (const row of rows) {
		addMetrics(totals, row);
		const sourceTotals = totalsBySource[row.source] ?? createEmptyMetrics();
		addMetrics(sourceTotals, row);
		totalsBySource[row.source] = sourceTotals;
	}

	return {
		rows,
		totals: toRowMetrics(totals),
		totalsBySource: Object.fromEntries(
			Object.entries(totalsBySource).map(([source, metrics]) => [source, toRowMetrics(metrics)]),
		) as Partial<Record<ToolName, RowMetrics>>,
		includeReasoning: rows.some((row) => (row.reasoningOutputTokens ?? 0) > 0),
		includeCredits: rows.some((row) => (row.credits ?? 0) > 0),
	};
}

function sortMonthlyRows<T extends { month: string }>(
	rows: T[],
	order: 'asc' | 'desc' | undefined,
): T[] {
	const direction = order ?? 'desc';
	return [...rows].sort((left, right) =>
		direction === 'asc'
			? left.month.localeCompare(right.month)
			: right.month.localeCompare(left.month),
	);
}

async function loadClaudeDailyRows(options: MultiToolReportOptions): Promise<MultiToolDailyRow[]> {
	const rows = await loadDailyUsageData({
		claudePath: options.claudePath,
		since: options.since,
		until: options.until,
		mode: 'auto',
		offline: options.offline,
		updatePricing: options.updatePricing,
		timezone: options.timezone,
		locale: options.locale,
		order: options.order,
	});

	return rows.map((row) => ({
		source: 'claude',
		date: row.date,
		inputTokens: row.inputTokens,
		outputTokens: row.outputTokens,
		cacheCreationTokens: row.cacheCreationTokens,
		cacheReadTokens: row.cacheReadTokens,
		totalTokens: getTotalTokens(row),
		totalCost: row.totalCost,
		modelsUsed: [...row.modelsUsed],
		modelBreakdowns: normalizeClaudeBreakdowns(row.modelBreakdowns),
	}));
}

async function loadClaudeMonthlyRows(
	options: MultiToolReportOptions,
): Promise<MultiToolMonthlyRow[]> {
	const rows = await loadMonthlyUsageData({
		claudePath: options.claudePath,
		since: options.since,
		until: options.until,
		mode: 'auto',
		offline: options.offline,
		updatePricing: options.updatePricing,
		timezone: options.timezone,
		locale: options.locale,
		order: options.order,
	});

	return rows.map((row) => ({
		source: 'claude',
		month: row.month,
		inputTokens: row.inputTokens,
		outputTokens: row.outputTokens,
		cacheCreationTokens: row.cacheCreationTokens,
		cacheReadTokens: row.cacheReadTokens,
		totalTokens: getTotalTokens(row),
		totalCost: row.totalCost,
		modelsUsed: [...row.modelsUsed],
		modelBreakdowns: normalizeClaudeBreakdowns(row.modelBreakdowns),
	}));
}

async function loadClaudeSessionRows(
	options: MultiToolReportOptions,
): Promise<MultiToolSessionRow[]> {
	const rows = await loadSessionData({
		claudePath: options.claudePath,
		since: options.since,
		until: options.until,
		mode: 'auto',
		offline: options.offline,
		updatePricing: options.updatePricing,
		timezone: options.timezone,
		locale: options.locale,
	});

	return rows.map((row) => ({
		source: 'claude',
		sessionId: row.sessionId,
		displayName: `${row.projectPath}/${row.sessionId}`,
		lastActivity: row.lastActivity,
		inputTokens: row.inputTokens,
		outputTokens: row.outputTokens,
		cacheCreationTokens: row.cacheCreationTokens,
		cacheReadTokens: row.cacheReadTokens,
		totalTokens: getTotalTokens(row),
		totalCost: row.totalCost,
		modelsUsed: [...row.modelsUsed],
		modelBreakdowns: normalizeClaudeBreakdowns(row.modelBreakdowns),
	}));
}

async function loadCodexDailyRows(options: MultiToolReportOptions): Promise<MultiToolDailyRow[]> {
	const { events } = await loadCodexTokenUsageEvents({ sessionDirs: options.codexSessionDirs });
	using pricingSource = new CodexPricingSource({
		offline: options.offline,
	});
	const getPricing = createCachedCodexPricingResolver(pricingSource);

	const summaries = new Map<
		string,
		{
			date: string;
			usage: ReturnType<typeof createEmptyUsage>;
			models: Map<string, ReturnType<typeof createEmptyUsage> & { isFallback?: boolean }>;
		}
	>();

	const since = createCodexFilterDate(options.since);
	const until = createCodexFilterDate(options.until);

	for (const event of events) {
		const modelName = event.model?.trim();
		if (modelName == null || modelName === '') {
			continue;
		}

		const dateKey = toDateKey(event.timestamp, options.timezone);
		if (!isWithinCodexRange(dateKey, since, until)) {
			continue;
		}

		const summary = summaries.get(dateKey) ?? {
			date: dateKey,
			usage: createEmptyUsage(),
			models: new Map<string, ReturnType<typeof createEmptyUsage> & { isFallback?: boolean }>(),
		};
		summaries.set(dateKey, summary);
		addUsage(summary.usage, event);

		const modelUsage = summary.models.get(modelName) ?? { ...createEmptyUsage() };
		addUsage(modelUsage, event);
		if (event.isFallbackModel === true) {
			modelUsage.isFallback = true;
		}
		summary.models.set(modelName, modelUsage);
	}

	const rows = await mapWithConcurrency(
		Array.from(summaries.values()),
		FILE_PARSE_CONCURRENCY,
		async (summary): Promise<MultiToolDailyRow> => {
			const models = Object.fromEntries(summary.models.entries());
			const breakdowns = await createCodexBreakdowns(models, getPricing);
			const cost = breakdowns.reduce((sum, breakdown) => sum + breakdown.cost, 0);
			const normalizedUsage = normalizeCodexRowUsage(summary.usage);

			return {
				source: 'codex',
				date: summary.date,
				...normalizedUsage,
				totalCost: cost,
				modelsUsed: formatModelsList(models),
				modelBreakdowns: breakdowns,
			};
		},
	);

	return sortByDate(rows, (row) => row.date, options.order);
}

async function loadCodexMonthlyRows(
	options: MultiToolReportOptions,
): Promise<MultiToolMonthlyRow[]> {
	const { events } = await loadCodexTokenUsageEvents({ sessionDirs: options.codexSessionDirs });
	using pricingSource = new CodexPricingSource({
		offline: options.offline,
	});
	const getPricing = createCachedCodexPricingResolver(pricingSource);

	const summaries = new Map<
		string,
		{
			month: string;
			usage: ReturnType<typeof createEmptyUsage>;
			models: Map<string, ReturnType<typeof createEmptyUsage> & { isFallback?: boolean }>;
		}
	>();

	const since = createCodexFilterDate(options.since);
	const until = createCodexFilterDate(options.until);

	for (const event of events) {
		const modelName = event.model?.trim();
		if (modelName == null || modelName === '') {
			continue;
		}

		const dateKey = toDateKey(event.timestamp, options.timezone);
		if (!isWithinCodexRange(dateKey, since, until)) {
			continue;
		}

		const monthKey = toMonthKey(event.timestamp, options.timezone);
		const summary = summaries.get(monthKey) ?? {
			month: monthKey,
			usage: createEmptyUsage(),
			models: new Map<string, ReturnType<typeof createEmptyUsage> & { isFallback?: boolean }>(),
		};
		summaries.set(monthKey, summary);
		addUsage(summary.usage, event);

		const modelUsage = summary.models.get(modelName) ?? { ...createEmptyUsage() };
		addUsage(modelUsage, event);
		if (event.isFallbackModel === true) {
			modelUsage.isFallback = true;
		}
		summary.models.set(modelName, modelUsage);
	}

	const rows = await mapWithConcurrency(
		Array.from(summaries.values()),
		FILE_PARSE_CONCURRENCY,
		async (summary): Promise<MultiToolMonthlyRow> => {
			const models = Object.fromEntries(summary.models.entries());
			const breakdowns = await createCodexBreakdowns(models, getPricing);
			const cost = breakdowns.reduce((sum, breakdown) => sum + breakdown.cost, 0);
			const normalizedUsage = normalizeCodexRowUsage(summary.usage);

			return {
				source: 'codex',
				month: summary.month,
				...normalizedUsage,
				totalCost: cost,
				modelsUsed: formatModelsList(models),
				modelBreakdowns: breakdowns,
			};
		},
	);

	return sortMonthlyRows(rows, options.order);
}

async function loadCodexSessionRows(
	options: MultiToolReportOptions,
): Promise<MultiToolSessionRow[]> {
	const { events } = await loadCodexTokenUsageEvents({ sessionDirs: options.codexSessionDirs });
	using pricingSource = new CodexPricingSource({
		offline: options.offline,
	});
	const getPricing = createCachedCodexPricingResolver(pricingSource);

	const rows = await buildCodexSessionReport(events, {
		pricingSource,
		timezone: options.timezone,
		locale: options.locale,
		since: createCodexFilterDate(options.since),
		until: createCodexFilterDate(options.until),
	});

	const normalizedRows = await mapWithConcurrency(
		rows,
		FILE_PARSE_CONCURRENCY,
		async (row): Promise<MultiToolSessionRow> => {
			const normalizedUsage = normalizeCodexRowUsage(row);

			return {
				source: 'codex',
				sessionId: row.sessionId,
				displayName: row.directory === '' ? row.sessionFile : `${row.directory}/${row.sessionFile}`,
				lastActivity: row.lastActivity,
				...normalizedUsage,
				totalCost: row.costUSD,
				modelsUsed: formatModelsList(row.models),
				modelBreakdowns: await createCodexBreakdowns(row.models, getPricing),
			};
		},
	);

	return sortByDate(normalizedRows, (row) => row.lastActivity, options.order);
}

function createOpenCodeModelAggregateEntry(): ModelAggregate {
	return createModelAggregate();
}

async function calculateOpenCodeAggregateCost(
	fetcher: LiteLLMPricingFetcher,
	modelName: string,
	aggregate: ModelAggregate,
): Promise<number> {
	const result = await fetcher.calculateCostFromTokens(
		{
			input_tokens: aggregate.inputTokens,
			output_tokens: aggregate.outputTokens,
			cache_creation_input_tokens: aggregate.cacheCreationTokens,
			cache_read_input_tokens: aggregate.cacheReadTokens,
		},
		modelName,
	);

	return aggregate.directCost + ('value' in result ? result.value : 0);
}

type OpenCodeGroupedRow = {
	key: string;
	sortKey: string;
	lastActivity?: string;
	displayName?: string;
	models: Map<string, ModelAggregate>;
};

async function buildOpenCodeGroupedRows(
	groupedRows: OpenCodeGroupedRow[],
	options: {
		offline?: boolean;
	},
	mapRow: (input: {
		key: string;
		sortKey: string;
		lastActivity?: string;
		displayName?: string;
		modelsUsed: string[];
		breakdowns: NormalizedModelBreakdown[];
		totals: MutableMetrics;
	}) => MultiToolDailyRow | MultiToolMonthlyRow | MultiToolSessionRow,
): Promise<Array<MultiToolDailyRow | MultiToolMonthlyRow | MultiToolSessionRow>> {
	using fetcher = new LiteLLMPricingFetcher({ offline: options.offline ?? false });

	return await mapWithConcurrency(groupedRows, FILE_PARSE_CONCURRENCY, async (row) => {
		const modelsUsed = Array.from(row.models.keys()).sort((left, right) =>
			left.localeCompare(right),
		);
		const breakdowns = await mapWithConcurrency(
			Array.from(row.models.entries()),
			FILE_PARSE_CONCURRENCY,
			async ([modelName, aggregate]): Promise<NormalizedModelBreakdown> => ({
				modelName,
				inputTokens: aggregate.inputTokens,
				outputTokens: aggregate.outputTokens,
				cacheCreationTokens: aggregate.cacheCreationTokens,
				cacheReadTokens: aggregate.cacheReadTokens,
				totalTokens: aggregate.totalTokens,
				cost: await calculateOpenCodeAggregateCost(fetcher, modelName, aggregate),
			}),
		);

		const totals = createEmptyMetrics();
		for (const breakdown of breakdowns) {
			addMetrics(totals, {
				inputTokens: breakdown.inputTokens,
				outputTokens: breakdown.outputTokens,
				cacheCreationTokens: breakdown.cacheCreationTokens,
				cacheReadTokens: breakdown.cacheReadTokens,
				totalTokens: breakdown.totalTokens,
				totalCost: breakdown.cost,
			});
		}

		return mapRow({
			key: row.key,
			sortKey: row.sortKey,
			lastActivity: row.lastActivity,
			displayName: row.displayName,
			modelsUsed,
			breakdowns,
			totals,
		});
	});
}

async function loadOpenCodeDailyRows(
	options: MultiToolReportOptions,
): Promise<MultiToolDailyRow[]> {
	const entries = await loadOpenCodeMessages();
	const groups = new Map<string, OpenCodeGroupedRow>();

	for (const entry of entries) {
		const date = entry.timestamp.toISOString().slice(0, 10);
		if (
			!isWithinCodexRange(
				date,
				createCodexFilterDate(options.since),
				createCodexFilterDate(options.until),
			)
		) {
			continue;
		}

		const group = groups.get(date) ?? {
			key: date,
			sortKey: date,
			models: new Map<string, ModelAggregate>(),
		};
		groups.set(date, group);
		const aggregate = group.models.get(entry.model) ?? createOpenCodeModelAggregateEntry();
		aggregate.inputTokens += entry.usage.inputTokens;
		aggregate.outputTokens += entry.usage.outputTokens;
		aggregate.cacheCreationTokens += entry.usage.cacheCreationInputTokens;
		aggregate.cacheReadTokens += entry.usage.cacheReadInputTokens;
		aggregate.totalTokens +=
			entry.usage.inputTokens +
			entry.usage.outputTokens +
			entry.usage.cacheCreationInputTokens +
			entry.usage.cacheReadInputTokens;
		if (entry.costUSD != null && entry.costUSD > 0) {
			aggregate.directCost += entry.costUSD;
		}
		group.models.set(entry.model, aggregate);
	}

	const rows = await buildOpenCodeGroupedRows(
		Array.from(groups.values()),
		{ offline: options.offline },
		({ key, modelsUsed, breakdowns, totals }) => ({
			source: 'opencode',
			date: key,
			inputTokens: totals.inputTokens,
			outputTokens: totals.outputTokens,
			cacheCreationTokens: totals.cacheCreationTokens,
			cacheReadTokens: totals.cacheReadTokens,
			totalTokens: totals.totalTokens,
			totalCost: totals.totalCost,
			modelsUsed,
			modelBreakdowns: breakdowns,
		}),
	);

	return sortByDate(rows as MultiToolDailyRow[], (row) => row.date, options.order);
}

async function loadOpenCodeMonthlyRows(
	options: MultiToolReportOptions,
): Promise<MultiToolMonthlyRow[]> {
	const entries = await loadOpenCodeMessages();
	const groups = new Map<string, OpenCodeGroupedRow>();

	for (const entry of entries) {
		const date = entry.timestamp.toISOString().slice(0, 10);
		if (
			!isWithinCodexRange(
				date,
				createCodexFilterDate(options.since),
				createCodexFilterDate(options.until),
			)
		) {
			continue;
		}

		const month = date.slice(0, 7);
		const group = groups.get(month) ?? {
			key: month,
			sortKey: month,
			models: new Map<string, ModelAggregate>(),
		};
		groups.set(month, group);
		const aggregate = group.models.get(entry.model) ?? createOpenCodeModelAggregateEntry();
		aggregate.inputTokens += entry.usage.inputTokens;
		aggregate.outputTokens += entry.usage.outputTokens;
		aggregate.cacheCreationTokens += entry.usage.cacheCreationInputTokens;
		aggregate.cacheReadTokens += entry.usage.cacheReadInputTokens;
		aggregate.totalTokens +=
			entry.usage.inputTokens +
			entry.usage.outputTokens +
			entry.usage.cacheCreationInputTokens +
			entry.usage.cacheReadInputTokens;
		if (entry.costUSD != null && entry.costUSD > 0) {
			aggregate.directCost += entry.costUSD;
		}
		group.models.set(entry.model, aggregate);
	}

	const rows = await buildOpenCodeGroupedRows(
		Array.from(groups.values()),
		{ offline: options.offline },
		({ key, modelsUsed, breakdowns, totals }) => ({
			source: 'opencode',
			month: key,
			inputTokens: totals.inputTokens,
			outputTokens: totals.outputTokens,
			cacheCreationTokens: totals.cacheCreationTokens,
			cacheReadTokens: totals.cacheReadTokens,
			totalTokens: totals.totalTokens,
			totalCost: totals.totalCost,
			modelsUsed,
			modelBreakdowns: breakdowns,
		}),
	);

	return sortMonthlyRows(rows as MultiToolMonthlyRow[], options.order);
}

async function loadOpenCodeSessionRows(
	options: MultiToolReportOptions,
): Promise<MultiToolSessionRow[]> {
	const [entries, sessionMetadata] = await Promise.all([
		loadOpenCodeMessages(),
		loadOpenCodeSessions(),
	]);
	const groups = new Map<string, OpenCodeGroupedRow>();

	for (const entry of entries) {
		const date = entry.timestamp.toISOString().slice(0, 10);
		if (
			!isWithinCodexRange(
				date,
				createCodexFilterDate(options.since),
				createCodexFilterDate(options.until),
			)
		) {
			continue;
		}

		const metadata = sessionMetadata.get(entry.sessionID);
		const group = groups.get(entry.sessionID) ?? {
			key: entry.sessionID,
			sortKey: entry.timestamp.toISOString(),
			lastActivity: entry.timestamp.toISOString(),
			displayName: metadata?.title ?? entry.sessionID,
			models: new Map<string, ModelAggregate>(),
		};
		groups.set(entry.sessionID, group);
		if (entry.timestamp.toISOString() > (group.lastActivity ?? '')) {
			group.lastActivity = entry.timestamp.toISOString();
			group.sortKey = group.lastActivity;
		}
		const aggregate = group.models.get(entry.model) ?? createOpenCodeModelAggregateEntry();
		aggregate.inputTokens += entry.usage.inputTokens;
		aggregate.outputTokens += entry.usage.outputTokens;
		aggregate.cacheCreationTokens += entry.usage.cacheCreationInputTokens;
		aggregate.cacheReadTokens += entry.usage.cacheReadInputTokens;
		aggregate.totalTokens +=
			entry.usage.inputTokens +
			entry.usage.outputTokens +
			entry.usage.cacheCreationInputTokens +
			entry.usage.cacheReadInputTokens;
		if (entry.costUSD != null && entry.costUSD > 0) {
			aggregate.directCost += entry.costUSD;
		}
		group.models.set(entry.model, aggregate);
	}

	const rows = await buildOpenCodeGroupedRows(
		Array.from(groups.values()),
		{ offline: options.offline },
		({ key, lastActivity, displayName, modelsUsed, breakdowns, totals }) => ({
			source: 'opencode',
			sessionId: key,
			displayName: displayName ?? key,
			lastActivity: lastActivity ?? '',
			inputTokens: totals.inputTokens,
			outputTokens: totals.outputTokens,
			cacheCreationTokens: totals.cacheCreationTokens,
			cacheReadTokens: totals.cacheReadTokens,
			totalTokens: totals.totalTokens,
			totalCost: totals.totalCost,
			modelsUsed,
			modelBreakdowns: breakdowns,
		}),
	);

	return sortByDate(rows as MultiToolSessionRow[], (row) => row.lastActivity, options.order);
}

async function loadAmpDailyRows(options: MultiToolReportOptions): Promise<MultiToolDailyRow[]> {
	const { events } = await loadAmpUsageEvents({ threadDirs: options.ampThreadDirs });
	using pricingSource = new AmpPricingSource({ offline: options.offline });

	const groups = new Map<
		string,
		{
			date: string;
			models: Map<string, ModelAggregate>;
			credits: number;
		}
	>();

	for (const event of events) {
		const date = event.timestamp.slice(0, 10);
		if (
			!isWithinCodexRange(
				date,
				createCodexFilterDate(options.since),
				createCodexFilterDate(options.until),
			)
		) {
			continue;
		}

		const group = groups.get(date) ?? {
			date,
			models: new Map<string, ModelAggregate>(),
			credits: 0,
		};
		groups.set(date, group);
		const aggregate = group.models.get(event.model) ?? createModelAggregate();
		aggregate.inputTokens += event.inputTokens;
		aggregate.outputTokens += event.outputTokens;
		aggregate.cacheCreationTokens += event.cacheCreationInputTokens;
		aggregate.cacheReadTokens += event.cacheReadInputTokens;
		aggregate.totalTokens += event.totalTokens;
		group.credits += event.credits;
		group.models.set(event.model, aggregate);
	}

	const rows = await mapWithConcurrency(
		Array.from(groups.values()),
		FILE_PARSE_CONCURRENCY,
		async (group): Promise<MultiToolDailyRow> => {
			const breakdowns = await mapWithConcurrency(
				Array.from(group.models.entries()),
				FILE_PARSE_CONCURRENCY,
				async ([modelName, aggregate]): Promise<NormalizedModelBreakdown> => ({
					modelName,
					inputTokens: aggregate.inputTokens,
					outputTokens: aggregate.outputTokens,
					cacheCreationTokens: aggregate.cacheCreationTokens,
					cacheReadTokens: aggregate.cacheReadTokens,
					totalTokens: aggregate.totalTokens,
					cost: await pricingSource.calculateCost(modelName, {
						inputTokens: aggregate.inputTokens,
						outputTokens: aggregate.outputTokens,
						cacheCreationInputTokens: aggregate.cacheCreationTokens,
						cacheReadInputTokens: aggregate.cacheReadTokens,
					}),
				}),
			);
			const totals = createEmptyMetrics();
			for (const breakdown of breakdowns) {
				addMetrics(totals, {
					inputTokens: breakdown.inputTokens,
					outputTokens: breakdown.outputTokens,
					cacheCreationTokens: breakdown.cacheCreationTokens,
					cacheReadTokens: breakdown.cacheReadTokens,
					totalTokens: breakdown.totalTokens,
					totalCost: breakdown.cost,
				});
			}
			totals.credits = group.credits;

			return {
				source: 'amp',
				date: group.date,
				inputTokens: totals.inputTokens,
				outputTokens: totals.outputTokens,
				cacheCreationTokens: totals.cacheCreationTokens,
				cacheReadTokens: totals.cacheReadTokens,
				totalTokens: totals.totalTokens,
				totalCost: totals.totalCost,
				credits: group.credits,
				modelsUsed: Array.from(group.models.keys()).sort(),
				modelBreakdowns: breakdowns,
			};
		},
	);

	return sortByDate(rows, (row) => row.date, options.order);
}

async function loadAmpMonthlyRows(options: MultiToolReportOptions): Promise<MultiToolMonthlyRow[]> {
	const dailyRows = await loadAmpDailyRows(options);
	const groups = new Map<
		string,
		{
			month: string;
			breakdowns: Map<string, NormalizedModelBreakdown>;
			credits: number;
		}
	>();

	for (const row of dailyRows) {
		const month = row.date.slice(0, 7);
		const group = groups.get(month) ?? {
			month,
			breakdowns: new Map<string, NormalizedModelBreakdown>(),
			credits: 0,
		};
		groups.set(month, group);
		group.credits += row.credits ?? 0;
		for (const breakdown of row.modelBreakdowns) {
			const existing = group.breakdowns.get(breakdown.modelName) ?? {
				...breakdown,
				inputTokens: 0,
				outputTokens: 0,
				cacheCreationTokens: 0,
				cacheReadTokens: 0,
				totalTokens: 0,
				cost: 0,
			};
			existing.inputTokens += breakdown.inputTokens;
			existing.outputTokens += breakdown.outputTokens;
			existing.cacheCreationTokens += breakdown.cacheCreationTokens;
			existing.cacheReadTokens += breakdown.cacheReadTokens;
			existing.totalTokens += breakdown.totalTokens;
			existing.cost += breakdown.cost;
			group.breakdowns.set(breakdown.modelName, existing);
		}
	}

	const rows = Array.from(groups.values()).map<MultiToolMonthlyRow>((group) => {
		const totals = createEmptyMetrics();
		const breakdowns = Array.from(group.breakdowns.values());
		for (const breakdown of breakdowns) {
			addMetrics(totals, {
				inputTokens: breakdown.inputTokens,
				outputTokens: breakdown.outputTokens,
				cacheCreationTokens: breakdown.cacheCreationTokens,
				cacheReadTokens: breakdown.cacheReadTokens,
				totalTokens: breakdown.totalTokens,
				totalCost: breakdown.cost,
			});
		}
		totals.credits = group.credits;

		return {
			source: 'amp',
			month: group.month,
			inputTokens: totals.inputTokens,
			outputTokens: totals.outputTokens,
			cacheCreationTokens: totals.cacheCreationTokens,
			cacheReadTokens: totals.cacheReadTokens,
			totalTokens: totals.totalTokens,
			totalCost: totals.totalCost,
			credits: group.credits,
			modelsUsed: breakdowns.map((breakdown) => breakdown.modelName).sort(),
			modelBreakdowns: breakdowns,
		};
	});

	return sortMonthlyRows(rows, options.order);
}

async function loadAmpSessionRows(options: MultiToolReportOptions): Promise<MultiToolSessionRow[]> {
	const { events, threads } = await loadAmpUsageEvents({ threadDirs: options.ampThreadDirs });
	using pricingSource = new AmpPricingSource({ offline: options.offline });

	const groups = new Map<
		string,
		{
			threadId: string;
			lastActivity: string;
			title: string;
			models: Map<string, ModelAggregate>;
			credits: number;
		}
	>();

	for (const event of events) {
		const date = event.timestamp.slice(0, 10);
		if (
			!isWithinCodexRange(
				date,
				createCodexFilterDate(options.since),
				createCodexFilterDate(options.until),
			)
		) {
			continue;
		}

		const group = groups.get(event.threadId) ?? {
			threadId: event.threadId,
			lastActivity: event.timestamp,
			title: threads.get(event.threadId)?.title ?? 'Untitled',
			models: new Map<string, ModelAggregate>(),
			credits: 0,
		};
		groups.set(event.threadId, group);
		if (event.timestamp > group.lastActivity) {
			group.lastActivity = event.timestamp;
		}
		const aggregate = group.models.get(event.model) ?? createModelAggregate();
		aggregate.inputTokens += event.inputTokens;
		aggregate.outputTokens += event.outputTokens;
		aggregate.cacheCreationTokens += event.cacheCreationInputTokens;
		aggregate.cacheReadTokens += event.cacheReadInputTokens;
		aggregate.totalTokens += event.totalTokens;
		group.credits += event.credits;
		group.models.set(event.model, aggregate);
	}

	const rows = await mapWithConcurrency(
		Array.from(groups.values()),
		FILE_PARSE_CONCURRENCY,
		async (group): Promise<MultiToolSessionRow> => {
			const breakdowns = await mapWithConcurrency(
				Array.from(group.models.entries()),
				FILE_PARSE_CONCURRENCY,
				async ([modelName, aggregate]): Promise<NormalizedModelBreakdown> => ({
					modelName,
					inputTokens: aggregate.inputTokens,
					outputTokens: aggregate.outputTokens,
					cacheCreationTokens: aggregate.cacheCreationTokens,
					cacheReadTokens: aggregate.cacheReadTokens,
					totalTokens: aggregate.totalTokens,
					cost: await pricingSource.calculateCost(modelName, {
						inputTokens: aggregate.inputTokens,
						outputTokens: aggregate.outputTokens,
						cacheCreationInputTokens: aggregate.cacheCreationTokens,
						cacheReadInputTokens: aggregate.cacheReadTokens,
					}),
				}),
			);
			const totals = createEmptyMetrics();
			for (const breakdown of breakdowns) {
				addMetrics(totals, {
					inputTokens: breakdown.inputTokens,
					outputTokens: breakdown.outputTokens,
					cacheCreationTokens: breakdown.cacheCreationTokens,
					cacheReadTokens: breakdown.cacheReadTokens,
					totalTokens: breakdown.totalTokens,
					totalCost: breakdown.cost,
				});
			}
			totals.credits = group.credits;

			return {
				source: 'amp',
				sessionId: group.threadId,
				displayName: group.title,
				lastActivity: group.lastActivity,
				inputTokens: totals.inputTokens,
				outputTokens: totals.outputTokens,
				cacheCreationTokens: totals.cacheCreationTokens,
				cacheReadTokens: totals.cacheReadTokens,
				totalTokens: totals.totalTokens,
				totalCost: totals.totalCost,
				credits: group.credits,
				modelsUsed: Array.from(group.models.keys()).sort(),
				modelBreakdowns: breakdowns,
			};
		},
	);

	return sortByDate(rows, (row) => row.lastActivity, options.order);
}

async function loadPiDailyRows(options: MultiToolReportOptions): Promise<MultiToolDailyRow[]> {
	const rows = await loadPiAgentDailyData({
		piPath: options.piPath,
		since: options.since,
		until: options.until,
		timezone: options.timezone,
		order: options.order,
	});

	return rows.map((row) => ({
		source: 'pi',
		date: row.date,
		inputTokens: row.inputTokens,
		outputTokens: row.outputTokens,
		cacheCreationTokens: row.cacheCreationTokens,
		cacheReadTokens: row.cacheReadTokens,
		totalTokens: row.inputTokens + row.outputTokens + row.cacheCreationTokens + row.cacheReadTokens,
		totalCost: row.totalCost,
		modelsUsed: [...row.modelsUsed],
		modelBreakdowns: normalizeClaudeBreakdowns(row.modelBreakdowns),
	}));
}

async function loadPiMonthlyRows(options: MultiToolReportOptions): Promise<MultiToolMonthlyRow[]> {
	const rows = await loadPiAgentMonthlyData({
		piPath: options.piPath,
		since: options.since,
		until: options.until,
		timezone: options.timezone,
		order: options.order,
	});

	return rows.map((row) => ({
		source: 'pi',
		month: row.month,
		inputTokens: row.inputTokens,
		outputTokens: row.outputTokens,
		cacheCreationTokens: row.cacheCreationTokens,
		cacheReadTokens: row.cacheReadTokens,
		totalTokens: row.inputTokens + row.outputTokens + row.cacheCreationTokens + row.cacheReadTokens,
		totalCost: row.totalCost,
		modelsUsed: [...row.modelsUsed],
		modelBreakdowns: normalizeClaudeBreakdowns(row.modelBreakdowns),
	}));
}

async function loadPiSessionRows(options: MultiToolReportOptions): Promise<MultiToolSessionRow[]> {
	const rows = await loadPiAgentSessionData({
		piPath: options.piPath,
		since: options.since,
		until: options.until,
		timezone: options.timezone,
		order: options.order,
	});

	return rows.map((row) => ({
		source: 'pi',
		sessionId: row.sessionId,
		displayName: `${row.projectPath}/${row.sessionId}`,
		lastActivity: row.lastActivity,
		inputTokens: row.inputTokens,
		outputTokens: row.outputTokens,
		cacheCreationTokens: row.cacheCreationTokens,
		cacheReadTokens: row.cacheReadTokens,
		totalTokens: row.inputTokens + row.outputTokens + row.cacheCreationTokens + row.cacheReadTokens,
		totalCost: row.totalCost,
		modelsUsed: [...row.modelsUsed],
		modelBreakdowns: normalizeClaudeBreakdowns(row.modelBreakdowns),
	}));
}

export async function loadMultiToolDailyReport(
	options: MultiToolReportOptions,
): Promise<MultiToolReport<MultiToolDailyRow>> {
	const rowsByTool = await mapWithConcurrency(
		options.tools,
		MULTI_TOOL_CONCURRENCY,
		async (tool) => {
			switch (tool) {
				case 'claude':
					return loadClaudeDailyRows(options);
				case 'codex':
					return loadCodexDailyRows(options);
				case 'opencode':
					return loadOpenCodeDailyRows(options);
				case 'pi':
					return loadPiDailyRows(options);
				case 'amp':
					return loadAmpDailyRows(options);
			}
		},
	);

	return createMultiToolReport(
		sortByDate(rowsByTool.flat(), (row) => row.date, options.order),
		options.tools,
	);
}

export async function loadMultiToolMonthlyReport(
	options: MultiToolReportOptions,
): Promise<MultiToolReport<MultiToolMonthlyRow>> {
	const rowsByTool = await mapWithConcurrency(
		options.tools,
		MULTI_TOOL_CONCURRENCY,
		async (tool) => {
			switch (tool) {
				case 'claude':
					return loadClaudeMonthlyRows(options);
				case 'codex':
					return loadCodexMonthlyRows(options);
				case 'opencode':
					return loadOpenCodeMonthlyRows(options);
				case 'pi':
					return loadPiMonthlyRows(options);
				case 'amp':
					return loadAmpMonthlyRows(options);
			}
		},
	);

	return createMultiToolReport(sortMonthlyRows(rowsByTool.flat(), options.order), options.tools);
}

export async function loadMultiToolSessionReport(
	options: MultiToolReportOptions,
): Promise<MultiToolReport<MultiToolSessionRow>> {
	const rowsByTool = await mapWithConcurrency(
		options.tools,
		MULTI_TOOL_CONCURRENCY,
		async (tool) => {
			switch (tool) {
				case 'claude':
					return loadClaudeSessionRows(options);
				case 'codex':
					return loadCodexSessionRows(options);
				case 'opencode':
					return loadOpenCodeSessionRows(options);
				case 'pi':
					return loadPiSessionRows(options);
				case 'amp':
					return loadAmpSessionRows(options);
			}
		},
	);

	return createMultiToolReport(
		sortByDate(rowsByTool.flat(), (row) => row.lastActivity, options.order),
		options.tools,
	);
}

function pushMultiToolBreakdownRows(
	table: ResponsiveTable,
	row: { modelBreakdowns: NormalizedModelBreakdown[] },
	options: {
		includeReasoning: boolean;
		includeCredits: boolean;
	},
): void {
	for (const breakdown of row.modelBreakdowns) {
		const cells = [
			'',
			'',
			`  ${breakdown.isFallback === true ? `${breakdown.modelName} (fallback)` : breakdown.modelName}`,
			formatNumber(breakdown.inputTokens),
			formatNumber(breakdown.outputTokens),
		];

		if (options.includeReasoning) {
			cells.push(formatNumber(breakdown.reasoningOutputTokens ?? 0));
		}

		cells.push(formatNumber(breakdown.cacheCreationTokens));
		cells.push(formatNumber(breakdown.cacheReadTokens));
		cells.push(formatNumber(breakdown.totalTokens));

		if (options.includeCredits) {
			cells.push('');
		}

		cells.push(formatCurrency(breakdown.cost));
		table.push(cells);
	}
}

export function renderMultiToolDailyTable(
	report: MultiToolReport<MultiToolDailyRow>,
	options: { compact?: boolean; timezone?: string; locale?: string; breakdown?: boolean },
): string {
	const head = ['Date', 'Source', 'Models', 'Input', 'Output'];
	const compactHead = ['Date', 'Source', 'Models', 'Input', 'Output', 'Cost (USD)'];
	const colAligns: Array<'left' | 'right'> = ['left', 'left', 'left', 'right', 'right'];
	const compactColAligns: Array<'left' | 'right'> = [
		'left',
		'left',
		'left',
		'right',
		'right',
		'right',
	];

	if (report.includeReasoning) {
		head.push('Reasoning');
		colAligns.push('right');
	}

	head.push('Cache Create', 'Cache Read', 'Total Tokens');
	colAligns.push('right', 'right', 'right');

	if (report.includeCredits) {
		head.push('Credits');
		colAligns.push('right');
	}

	head.push('Cost (USD)');
	colAligns.push('right');

	const table = new ResponsiveTable({
		head,
		colAligns,
		compactHead,
		compactColAligns,
		compactThreshold: 100,
		forceCompact: options.compact,
		minColumnWidths: [10, 8, 18],
		wideColumns: [2],
		style: { head: ['cyan'] },
		dateFormatter: (dateStr: string) =>
			formatDateCompact(dateStr, options.timezone, options.locale ?? undefined),
	});

	for (const row of report.rows) {
		const cells = [
			row.date,
			row.source,
			formatModelsDisplayMultiline(row.modelsUsed),
			formatNumber(row.inputTokens),
			formatNumber(row.outputTokens),
		];

		if (report.includeReasoning) {
			cells.push(formatNumber(row.reasoningOutputTokens ?? 0));
		}

		cells.push(
			formatNumber(row.cacheCreationTokens),
			formatNumber(row.cacheReadTokens),
			formatNumber(row.totalTokens),
		);

		if (report.includeCredits) {
			cells.push(row.credits != null ? row.credits.toFixed(2) : '');
		}

		cells.push(formatCurrency(row.totalCost));
		table.push(cells);

		if (options.breakdown === true) {
			pushMultiToolBreakdownRows(table, row, report);
		}
	}

	addEmptySeparatorRow(table, head.length);

	const totals = report.totals;
	const totalCells = [pc.yellow('Total'), '', ''];
	totalCells.push(
		pc.yellow(formatNumber(totals.inputTokens)),
		pc.yellow(formatNumber(totals.outputTokens)),
	);

	if (report.includeReasoning) {
		totalCells.push(pc.yellow(formatNumber(totals.reasoningOutputTokens ?? 0)));
	}

	totalCells.push(
		pc.yellow(formatNumber(totals.cacheCreationTokens)),
		pc.yellow(formatNumber(totals.cacheReadTokens)),
		pc.yellow(formatNumber(totals.totalTokens)),
	);

	if (report.includeCredits) {
		totalCells.push(pc.yellow((totals.credits ?? 0).toFixed(2)));
	}

	totalCells.push(pc.yellow(formatCurrency(totals.totalCost)));
	table.push(totalCells);

	return table.toString();
}

export function renderMultiToolMonthlyTable(
	report: MultiToolReport<MultiToolMonthlyRow>,
	options: { compact?: boolean; timezone?: string; locale?: string; breakdown?: boolean },
): string {
	const head = ['Month', 'Source', 'Models', 'Input', 'Output'];
	const compactHead = ['Month', 'Source', 'Models', 'Input', 'Output', 'Cost (USD)'];
	const colAligns: Array<'left' | 'right'> = ['left', 'left', 'left', 'right', 'right'];
	const compactColAligns: Array<'left' | 'right'> = [
		'left',
		'left',
		'left',
		'right',
		'right',
		'right',
	];

	if (report.includeReasoning) {
		head.push('Reasoning');
		colAligns.push('right');
	}

	head.push('Cache Create', 'Cache Read', 'Total Tokens');
	colAligns.push('right', 'right', 'right');

	if (report.includeCredits) {
		head.push('Credits');
		colAligns.push('right');
	}

	head.push('Cost (USD)');
	colAligns.push('right');

	const table = new ResponsiveTable({
		head,
		colAligns,
		compactHead,
		compactColAligns,
		compactThreshold: 100,
		forceCompact: options.compact,
		minColumnWidths: [10, 8, 18],
		wideColumns: [2],
		style: { head: ['cyan'] },
		dateFormatter: (dateStr: string) =>
			formatDateCompact(`${dateStr}-01`, options.timezone, options.locale ?? undefined),
	});

	for (const row of report.rows) {
		const cells = [
			row.month,
			row.source,
			formatModelsDisplayMultiline(row.modelsUsed),
			formatNumber(row.inputTokens),
			formatNumber(row.outputTokens),
		];

		if (report.includeReasoning) {
			cells.push(formatNumber(row.reasoningOutputTokens ?? 0));
		}

		cells.push(
			formatNumber(row.cacheCreationTokens),
			formatNumber(row.cacheReadTokens),
			formatNumber(row.totalTokens),
		);

		if (report.includeCredits) {
			cells.push(row.credits != null ? row.credits.toFixed(2) : '');
		}

		cells.push(formatCurrency(row.totalCost));
		table.push(cells);

		if (options.breakdown === true) {
			pushMultiToolBreakdownRows(table, row, report);
		}
	}

	addEmptySeparatorRow(table, head.length);

	const totals = report.totals;
	const totalCells = [pc.yellow('Total'), '', ''];
	totalCells.push(
		pc.yellow(formatNumber(totals.inputTokens)),
		pc.yellow(formatNumber(totals.outputTokens)),
	);

	if (report.includeReasoning) {
		totalCells.push(pc.yellow(formatNumber(totals.reasoningOutputTokens ?? 0)));
	}

	totalCells.push(
		pc.yellow(formatNumber(totals.cacheCreationTokens)),
		pc.yellow(formatNumber(totals.cacheReadTokens)),
		pc.yellow(formatNumber(totals.totalTokens)),
	);

	if (report.includeCredits) {
		totalCells.push(pc.yellow((totals.credits ?? 0).toFixed(2)));
	}

	totalCells.push(pc.yellow(formatCurrency(totals.totalCost)));
	table.push(totalCells);

	return table.toString();
}

export function renderMultiToolSessionTable(
	report: MultiToolReport<MultiToolSessionRow>,
	options: { compact?: boolean; breakdown?: boolean },
): string {
	const head = ['Source', 'Session', 'Models', 'Input', 'Output'];
	const compactHead = ['Source', 'Session', 'Input', 'Output', 'Cost (USD)'];
	const colAligns: Array<'left' | 'right'> = ['left', 'left', 'left', 'right', 'right'];
	const compactColAligns: Array<'left' | 'right'> = ['left', 'left', 'right', 'right', 'right'];

	if (report.includeReasoning) {
		head.push('Reasoning');
		colAligns.push('right');
	}

	head.push('Cache Create', 'Cache Read', 'Total Tokens');
	colAligns.push('right', 'right', 'right');

	if (report.includeCredits) {
		head.push('Credits');
		colAligns.push('right');
	}

	head.push('Cost (USD)', 'Last Activity');
	colAligns.push('right', 'left');

	const table = new ResponsiveTable({
		head,
		colAligns,
		compactHead,
		compactColAligns,
		compactThreshold: 100,
		forceCompact: options.compact,
		minColumnWidths: [8, 16, 18],
		wideColumns: [1, 2],
		style: { head: ['cyan'] },
	});

	for (const row of report.rows) {
		const cells = [
			row.source,
			row.displayName,
			formatModelsDisplayMultiline(row.modelsUsed),
			formatNumber(row.inputTokens),
			formatNumber(row.outputTokens),
		];

		if (report.includeReasoning) {
			cells.push(formatNumber(row.reasoningOutputTokens ?? 0));
		}

		cells.push(
			formatNumber(row.cacheCreationTokens),
			formatNumber(row.cacheReadTokens),
			formatNumber(row.totalTokens),
		);

		if (report.includeCredits) {
			cells.push(row.credits != null ? row.credits.toFixed(2) : '');
		}

		cells.push(formatCurrency(row.totalCost), row.lastActivity);
		table.push(cells);

		if (options.breakdown === true) {
			pushMultiToolBreakdownRows(table, row, report);
		}
	}

	addEmptySeparatorRow(table, head.length);

	const totals = report.totals;
	const totalCells = [pc.yellow('Total'), '', ''];
	totalCells.push(
		pc.yellow(formatNumber(totals.inputTokens)),
		pc.yellow(formatNumber(totals.outputTokens)),
	);

	if (report.includeReasoning) {
		totalCells.push(pc.yellow(formatNumber(totals.reasoningOutputTokens ?? 0)));
	}

	totalCells.push(
		pc.yellow(formatNumber(totals.cacheCreationTokens)),
		pc.yellow(formatNumber(totals.cacheReadTokens)),
		pc.yellow(formatNumber(totals.totalTokens)),
	);

	if (report.includeCredits) {
		totalCells.push(pc.yellow((totals.credits ?? 0).toFixed(2)));
	}

	totalCells.push(pc.yellow(formatCurrency(totals.totalCost)), '');
	table.push(totalCells);

	return table.toString();
}

if (import.meta.vitest != null) {
	const { describe, expect, it } = import.meta.vitest;

	describe('multi-tool reports', () => {
		it('merges Claude and Codex daily/monthly JSON rows with totalsBySource', async () => {
			const { createFixture } = await import('fs-fixture');

			await using fixture = await createFixture({
				claude: {
					projects: {
						proj: {
							sessionA: {
								'usage.jsonl': JSON.stringify({
									timestamp: '2025-01-10T10:00:00.000Z',
									sessionId: 'session-a',
									message: {
										id: 'msg-1',
										usage: {
											input_tokens: 100,
											output_tokens: 50,
											cache_creation_input_tokens: 0,
											cache_read_input_tokens: 0,
										},
										model: 'claude-sonnet-4-20250514',
									},
									requestId: 'req-1',
									costUSD: 0.1,
								}),
							},
						},
					},
				},
				codex: {
					sessions: {
						dir: {
							'codex-session.jsonl': [
								JSON.stringify({
									timestamp: '2025-01-10T11:00:00.000Z',
									type: 'turn_context',
									payload: { model: 'gpt-5' },
								}),
								JSON.stringify({
									timestamp: '2025-01-10T11:00:01.000Z',
									type: 'event_msg',
									payload: {
										type: 'token_count',
										info: {
											last_token_usage: {
												input_tokens: 200,
												cached_input_tokens: 20,
												output_tokens: 30,
												reasoning_output_tokens: 5,
												total_tokens: 230,
											},
											model: 'gpt-5',
										},
									},
								}),
							].join('\n'),
						},
					},
				},
			});

			const dailyReport = await loadMultiToolDailyReport({
				tools: ['claude', 'codex'],
				claudePath: fixture.getPath('claude'),
				codexSessionDirs: [fixture.getPath('codex/sessions')],
				offline: true,
			});

			expect(dailyReport.rows).toHaveLength(2);
			expect(dailyReport.rows.map((row) => row.source)).toEqual(['claude', 'codex']);
			expect(dailyReport.rows.find((row) => row.source === 'codex')).toMatchObject({
				inputTokens: 180,
				cacheReadTokens: 20,
				totalTokens: 230,
			});
			expect(dailyReport.totalsBySource.claude).toMatchObject({
				inputTokens: 100,
				cacheReadTokens: 0,
				totalTokens: 150,
			});
			expect(dailyReport.totalsBySource.codex).toMatchObject({
				inputTokens: 180,
				cacheReadTokens: 20,
				totalTokens: 230,
			});
			expect(dailyReport.totals).toMatchObject({
				inputTokens: 280,
				cacheReadTokens: 20,
				totalTokens: 380,
			});
			expect(dailyReport.totalsBySource.claude?.totalTokens).toBe(150);
			expect(dailyReport.totalsBySource.codex?.totalTokens).toBe(230);
			expect(dailyReport.totals.totalTokens).toBe(380);

			const monthlyReport = await loadMultiToolMonthlyReport({
				tools: ['claude', 'codex'],
				claudePath: fixture.getPath('claude'),
				codexSessionDirs: [fixture.getPath('codex/sessions')],
				offline: true,
			});

			expect(monthlyReport.rows).toHaveLength(2);
			expect(monthlyReport.rows.map((row) => row.month)).toEqual(['2025-01', '2025-01']);
			expect(monthlyReport.rows.find((row) => row.source === 'codex')).toMatchObject({
				inputTokens: 180,
				cacheReadTokens: 20,
				totalTokens: 230,
			});
			expect(monthlyReport.totalsBySource.claude).toMatchObject({
				inputTokens: 100,
				cacheReadTokens: 0,
				totalTokens: 150,
			});
			expect(monthlyReport.totalsBySource.codex).toMatchObject({
				inputTokens: 180,
				cacheReadTokens: 20,
				totalTokens: 230,
			});
			expect(monthlyReport.totals).toMatchObject({
				inputTokens: 280,
				cacheReadTokens: 20,
				totalTokens: 380,
			});
			expect(monthlyReport.totalsBySource.claude?.totalTokens).toBe(150);
			expect(monthlyReport.totalsBySource.codex?.totalTokens).toBe(230);
		});

		it('merges Claude and Codex session rows', async () => {
			const { createFixture } = await import('fs-fixture');

			await using fixture = await createFixture({
				claude: {
					projects: {
						proj: {
							sessionA: {
								'usage.jsonl': JSON.stringify({
									timestamp: '2025-01-10T10:00:00.000Z',
									sessionId: 'session-a',
									message: {
										id: 'msg-1',
										usage: {
											input_tokens: 100,
											output_tokens: 50,
											cache_creation_input_tokens: 0,
											cache_read_input_tokens: 0,
										},
										model: 'claude-sonnet-4-20250514',
									},
									requestId: 'req-1',
									costUSD: 0.1,
								}),
							},
						},
					},
				},
				codex: {
					sessions: {
						dir: {
							'codex-session.jsonl': [
								JSON.stringify({
									timestamp: '2025-01-10T11:00:00.000Z',
									type: 'turn_context',
									payload: { model: 'gpt-5' },
								}),
								JSON.stringify({
									timestamp: '2025-01-10T11:00:01.000Z',
									type: 'event_msg',
									payload: {
										type: 'token_count',
										info: {
											last_token_usage: {
												input_tokens: 200,
												cached_input_tokens: 20,
												output_tokens: 30,
												reasoning_output_tokens: 5,
												total_tokens: 230,
											},
											model: 'gpt-5',
										},
									},
								}),
							].join('\n'),
						},
					},
				},
			});

			const report = await loadMultiToolSessionReport({
				tools: ['claude', 'codex'],
				claudePath: fixture.getPath('claude'),
				codexSessionDirs: [fixture.getPath('codex/sessions')],
				offline: true,
			});

			expect(report.rows).toHaveLength(2);
			expect(report.rows.map((row) => row.source).sort()).toEqual(['claude', 'codex']);
			expect(report.rows.find((row) => row.source === 'claude')?.displayName).toContain(
				'proj/sessionA',
			);
			expect(report.rows.find((row) => row.source === 'codex')).toMatchObject({
				displayName: 'dir/codex-session',
				inputTokens: 180,
				cacheReadTokens: 20,
				totalTokens: 230,
			});
		});

		it('renders normalized Codex input and cache read values in daily table', async () => {
			const { createFixture } = await import('fs-fixture');

			await using fixture = await createFixture({
				codex: {
					sessions: {
						dir: {
							'codex-session.jsonl': [
								JSON.stringify({
									timestamp: '2025-01-10T11:00:00.000Z',
									type: 'turn_context',
									payload: { model: 'gpt-5' },
								}),
								JSON.stringify({
									timestamp: '2025-01-10T11:00:01.000Z',
									type: 'event_msg',
									payload: {
										type: 'token_count',
										info: {
											last_token_usage: {
												input_tokens: 200,
												cached_input_tokens: 20,
												output_tokens: 30,
												reasoning_output_tokens: 5,
												total_tokens: 230,
											},
											model: 'gpt-5',
										},
									},
								}),
							].join('\n'),
						},
					},
				},
			});

			const report = await loadMultiToolDailyReport({
				tools: ['codex'],
				codexSessionDirs: [fixture.getPath('codex/sessions')],
				offline: true,
			});
			const originalColumns = process.stdout.columns;
			Object.defineProperty(process.stdout, 'columns', {
				configurable: true,
				value: 240,
			});

			try {
				const table = renderMultiToolDailyTable(report, {});

				expect(table).toContain('codex');
				expect(table).toContain('gpt-5');
				expect(table).toContain('180');
				expect(table).toContain('30');
				expect(table).toContain('5');
				expect(table).toContain('0');
				expect(table).toContain('20');
				expect(table).toContain('230');
				expect(table).not.toMatch(/\b200\b/);
			} finally {
				Object.defineProperty(process.stdout, 'columns', {
					configurable: true,
					value: originalColumns,
				});
			}
		});

		it('renders source, reasoning, and credits columns when present', () => {
			const originalColumns = process.stdout.columns;
			Object.defineProperty(process.stdout, 'columns', {
				configurable: true,
				value: 240,
			});

			try {
				const table = renderMultiToolDailyTable(
					{
						rows: [
							{
								source: 'codex',
								date: '2025-01-10',
								inputTokens: 10,
								outputTokens: 5,
								cacheCreationTokens: 0,
								cacheReadTokens: 2,
								totalTokens: 15,
								totalCost: 0.01,
								reasoningOutputTokens: 1,
								modelsUsed: ['gpt-5'],
								modelBreakdowns: [],
							},
							{
								source: 'amp',
								date: '2025-01-10',
								inputTokens: 8,
								outputTokens: 4,
								cacheCreationTokens: 1,
								cacheReadTokens: 1,
								totalTokens: 12,
								totalCost: 0.02,
								credits: 3,
								modelsUsed: ['claude-haiku-4-5-20251001'],
								modelBreakdowns: [],
							},
						],
						totals: {
							inputTokens: 18,
							outputTokens: 9,
							cacheCreationTokens: 1,
							cacheReadTokens: 3,
							totalTokens: 27,
							totalCost: 0.03,
							reasoningOutputTokens: 1,
							credits: 3,
						},
						totalsBySource: {},
						includeReasoning: true,
						includeCredits: true,
					},
					{},
				);

				expect(table).toContain('Source');
				expect(table).toContain('Reasoning');
				expect(table).toContain('Credits');
			} finally {
				Object.defineProperty(process.stdout, 'columns', {
					configurable: true,
					value: originalColumns,
				});
			}
		});
	});
}
