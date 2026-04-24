import type { UsageReportConfig } from '@ccusage/terminal/table';
import process from 'node:process';
import {
	addEmptySeparatorRow,
	createUsageReportTable,
	formatTotalsRow,
	formatUsageDataRow,
	pushBreakdownRows,
} from '@ccusage/terminal/table';
import { Result } from '@praha/byethrow';
import { define } from 'gunshi';
import { loadConfig, mergeConfigWithArgs } from '../_config-loader-tokens.ts';
import { DEFAULT_LOCALE } from '../_consts.ts';
import { formatDateCompact } from '../_date-utils.ts';
import { processWithJq } from '../_jq-processor.ts';
import { loadMultiToolSessionReport, renderMultiToolSessionTable } from '../_multi-tool-report.ts';
import {
	getClaudeReportSources,
	getMultiToolReportSources,
	withReportCache,
} from '../_report-cache.ts';
import { sharedCommandConfig } from '../_shared-args.ts';
import { hasExplicitToolSelection, normalizeToolSelection } from '../_tool-selection.ts';
import { calculateTotals, createTotalsObject, getTotalTokens } from '../calculate-cost.ts';
import { loadSessionData } from '../data-loader.ts';
import { detectMismatches, printMismatchReport } from '../debug.ts';
import { log, logger } from '../logger.ts';
import { handleSessionIdLookup } from './_session_id.ts';

// eslint-disable-next-line ts/no-unused-vars
const { order: _, ...sharedArgs } = sharedCommandConfig.args;

export const sessionCommand = define({
	name: 'session',
	description: 'Show usage report grouped by conversation session',
	...sharedCommandConfig,
	args: {
		...sharedArgs,
		id: {
			type: 'string',
			short: 'i',
			description: 'Load usage data for a specific session ID',
		},
	},
	toKebab: true,
	async run(ctx): Promise<void> {
		// Load configuration and merge with CLI arguments
		const config = loadConfig(ctx.values.config, ctx.values.debug);
		const mergedOptions: typeof ctx.values = mergeConfigWithArgs(ctx, config, ctx.values.debug);

		// --jq implies --json
		const useJson = mergedOptions.json || mergedOptions.jq != null;
		if (useJson) {
			logger.level = 0;
		}

		if (mergedOptions.id != null && hasExplicitToolSelection(mergedOptions.tool)) {
			const tools = normalizeToolSelection(mergedOptions.tool);
			if (tools.length !== 1 || tools[0] !== 'claude') {
				logger.error('`session --id` is only supported in Claude-only mode.');
				process.exit(1);
			}
		}

		// Handle specific session ID lookup
		if (mergedOptions.id != null) {
			return handleSessionIdLookup(
				{
					values: {
						id: mergedOptions.id,
						mode: mergedOptions.mode,
						offline: mergedOptions.offline,
						updatePricing: mergedOptions.updatePricing,
						jq: mergedOptions.jq,
						timezone: mergedOptions.timezone,
						locale: mergedOptions.locale ?? DEFAULT_LOCALE,
					},
				},
				useJson,
			);
		}

		if (hasExplicitToolSelection(mergedOptions.tool)) {
			const tools = normalizeToolSelection(mergedOptions.tool);
			const report = await withReportCache({
				command: 'session',
				parameters: {
					mode: 'multi-tool',
					tools,
					since: mergedOptions.since,
					until: mergedOptions.until,
					timezone: mergedOptions.timezone,
					locale: mergedOptions.locale,
					offline: Boolean(mergedOptions.offline),
					updatePricing: Boolean(mergedOptions.updatePricing),
					byModel: Boolean(mergedOptions.byModel),
				},
				sources: getMultiToolReportSources('session', { tools }),
				pricing: {
					requiresPricing: tools.some((tool) => tool !== 'pi'),
					offline: mergedOptions.offline,
					updatePricing: mergedOptions.updatePricing,
				},
				load: async () =>
					loadMultiToolSessionReport({
						tools,
						since: mergedOptions.since,
						until: mergedOptions.until,
						timezone: mergedOptions.timezone,
						locale: mergedOptions.locale,
						offline: mergedOptions.offline,
						updatePricing: mergedOptions.updatePricing,
						byModel: mergedOptions.byModel,
					}),
			});

			if (report.rows.length === 0) {
				const emptyOutput = {
					sessions: [],
					totals: report.totals,
					totalsBySource: report.totalsBySource,
				};
				if (useJson) {
					log(JSON.stringify(emptyOutput, null, 2));
				} else {
					logger.warn('No usage data found for the selected tools.');
				}
				process.exit(0);
			}

			if (useJson) {
				const jsonOutput = {
					sessions: report.rows,
					totals: report.totals,
					totalsBySource: report.totalsBySource,
				};

				if (mergedOptions.jq != null) {
					const jqResult = await processWithJq(jsonOutput, mergedOptions.jq);
					if (Result.isFailure(jqResult)) {
						logger.error(jqResult.error.message);
						process.exit(1);
					}
					log(jqResult.value);
				} else {
					log(JSON.stringify(jsonOutput, null, 2));
				}
				return;
			}

			logger.box('Multi-Tool Token Usage Report - Session');
			log(
				renderMultiToolSessionTable(report, {
					compact: mergedOptions.compact,
					breakdown: mergedOptions.breakdown,
				}),
			);
			return;
		}

		// Original session listing logic
		const sessionData = await withReportCache({
			command: 'session',
			parameters: {
				mode: 'claude',
				since: mergedOptions.since,
				until: mergedOptions.until,
				timezone: mergedOptions.timezone,
				locale: mergedOptions.locale,
				costMode: mergedOptions.mode,
				offline: Boolean(mergedOptions.offline),
				updatePricing: Boolean(mergedOptions.updatePricing),
			},
			sources: getClaudeReportSources(),
			pricing: {
				requiresPricing: mergedOptions.mode !== 'display',
				offline: mergedOptions.offline,
				updatePricing: mergedOptions.updatePricing,
			},
			load: async () =>
				loadSessionData({
					since: mergedOptions.since,
					until: mergedOptions.until,
					mode: mergedOptions.mode,
					offline: mergedOptions.offline,
					updatePricing: mergedOptions.updatePricing,
					timezone: mergedOptions.timezone,
					locale: mergedOptions.locale,
				}),
		});

		if (sessionData.length === 0) {
			if (useJson) {
				log(JSON.stringify([]));
			} else {
				logger.warn('No Claude usage data found.');
			}
			process.exit(0);
		}

		// Calculate totals
		const totals = calculateTotals(sessionData);

		// Show debug information if requested
		if (mergedOptions.debug && !useJson) {
			const mismatchStats = await detectMismatches(
				undefined,
				Boolean(mergedOptions.offline),
				Boolean(mergedOptions.updatePricing),
			);
			printMismatchReport(mismatchStats, mergedOptions.debugSamples);
		}

		if (useJson) {
			// Output JSON format
			const jsonOutput = {
				sessions: sessionData.map((data) => ({
					sessionId: data.sessionId,
					inputTokens: data.inputTokens,
					outputTokens: data.outputTokens,
					cacheCreationTokens: data.cacheCreationTokens,
					cacheReadTokens: data.cacheReadTokens,
					totalTokens: getTotalTokens(data),
					totalCost: data.totalCost,
					lastActivity: data.lastActivity,
					modelsUsed: data.modelsUsed,
					modelBreakdowns: data.modelBreakdowns,
					projectPath: data.projectPath,
				})),
				totals: createTotalsObject(totals),
			};

			// Process with jq if specified
			if (mergedOptions.jq != null) {
				const jqResult = await processWithJq(jsonOutput, mergedOptions.jq);
				if (Result.isFailure(jqResult)) {
					logger.error(jqResult.error.message);
					process.exit(1);
				}
				log(jqResult.value);
			} else {
				log(JSON.stringify(jsonOutput, null, 2));
			}
		} else {
			// Print header
			logger.box('Claude Code Token Usage Report - By Session');

			// Create table with compact mode support
			const tableConfig: UsageReportConfig = {
				firstColumnName: 'Session',
				includeLastActivity: true,
				dateFormatter: (dateStr: string) =>
					formatDateCompact(dateStr, mergedOptions.timezone, mergedOptions.locale),
				forceCompact: mergedOptions.compact,
			};
			const table = createUsageReportTable(tableConfig);

			// Add session data
			let maxSessionLength = 0;
			for (const data of sessionData) {
				const sessionDisplay = data.sessionId.split('-').slice(-2).join('-'); // Display last two parts of session ID

				maxSessionLength = Math.max(maxSessionLength, sessionDisplay.length);

				// Main row
				const row = formatUsageDataRow(
					sessionDisplay,
					{
						inputTokens: data.inputTokens,
						outputTokens: data.outputTokens,
						cacheCreationTokens: data.cacheCreationTokens,
						cacheReadTokens: data.cacheReadTokens,
						totalCost: data.totalCost,
						modelsUsed: data.modelsUsed,
					},
					data.lastActivity,
				);
				table.push(row);

				// Add model breakdown rows if flag is set
				if (mergedOptions.breakdown) {
					// Session has 1 extra column before data and 1 trailing column
					pushBreakdownRows(table, data.modelBreakdowns, 1, 1);
				}
			}

			// Add empty row for visual separation before totals
			addEmptySeparatorRow(table, 9);

			// Add totals
			const totalsRow = formatTotalsRow(
				{
					inputTokens: totals.inputTokens,
					outputTokens: totals.outputTokens,
					cacheCreationTokens: totals.cacheCreationTokens,
					cacheReadTokens: totals.cacheReadTokens,
					totalCost: totals.totalCost,
				},
				true,
			); // Include Last Activity column
			table.push(totalsRow);

			log(table.toString());

			// Show guidance message if in compact mode
			if (table.isCompactMode()) {
				logger.info('\nRunning in Compact Mode');
				logger.info('Expand terminal width to see cache metrics and total tokens');
			}
		}
	},
});

// Note: Tests for --id functionality are covered by the existing loadSessionUsageById tests
// in data-loader.ts, since this command directly uses that function.
