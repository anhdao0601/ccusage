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
import { loadMultiToolMonthlyReport, renderMultiToolMonthlyTable } from '../_multi-tool-report.ts';
import {
	getClaudeReportSources,
	getMultiToolReportSources,
	withReportCache,
} from '../_report-cache.ts';
import { sharedCommandConfig } from '../_shared-args.ts';
import { hasExplicitToolSelection, normalizeToolSelection } from '../_tool-selection.ts';
import { calculateTotals, createTotalsObject, getTotalTokens } from '../calculate-cost.ts';
import { loadMonthlyUsageData } from '../data-loader.ts';
import { detectMismatches, printMismatchReport } from '../debug.ts';
import { log, logger } from '../logger.ts';

export const monthlyCommand = define({
	name: 'monthly',
	description: 'Show usage report grouped by month',
	...sharedCommandConfig,
	async run(ctx) {
		// Load configuration and merge with CLI arguments
		const config = loadConfig(ctx.values.config, ctx.values.debug);
		const mergedOptions = mergeConfigWithArgs(ctx, config, ctx.values.debug);

		// --jq implies --json
		const useJson = Boolean(mergedOptions.json) || mergedOptions.jq != null;
		if (useJson) {
			logger.level = 0;
		}

		if (hasExplicitToolSelection(mergedOptions.tool)) {
			const tools = normalizeToolSelection(mergedOptions.tool);
			const report = await withReportCache({
				command: 'monthly',
				parameters: {
					mode: 'multi-tool',
					tools,
					since: mergedOptions.since,
					until: mergedOptions.until,
					timezone: mergedOptions.timezone,
					locale: mergedOptions.locale,
					order: mergedOptions.order,
					offline: Boolean(mergedOptions.offline),
					updatePricing: Boolean(mergedOptions.updatePricing),
				},
				sources: getMultiToolReportSources('monthly', { tools }),
				pricing: {
					requiresPricing: tools.some((tool) => tool !== 'pi'),
					offline: mergedOptions.offline,
					updatePricing: mergedOptions.updatePricing,
				},
				load: async () =>
					loadMultiToolMonthlyReport({
						tools,
						since: mergedOptions.since,
						until: mergedOptions.until,
						timezone: mergedOptions.timezone,
						locale: mergedOptions.locale,
						order: mergedOptions.order,
						offline: mergedOptions.offline,
						updatePricing: mergedOptions.updatePricing,
					}),
			});

			if (report.rows.length === 0) {
				const emptyOutput = {
					monthly: [],
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
					monthly: report.rows,
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

			logger.box('Multi-Tool Token Usage Report - Monthly');
			log(
				renderMultiToolMonthlyTable(report, {
					compact: ctx.values.compact,
					timezone: mergedOptions.timezone,
					locale: mergedOptions.locale ?? DEFAULT_LOCALE,
					breakdown: mergedOptions.breakdown,
				}),
			);
			return;
		}

		const monthlyData = await withReportCache({
			command: 'monthly',
			parameters: {
				mode: 'claude',
				since: mergedOptions.since,
				until: mergedOptions.until,
				timezone: mergedOptions.timezone,
				locale: mergedOptions.locale,
				order: mergedOptions.order,
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
			load: async () => loadMonthlyUsageData(mergedOptions),
		});

		if (monthlyData.length === 0) {
			if (useJson) {
				const emptyOutput = {
					monthly: [],
					totals: {
						inputTokens: 0,
						outputTokens: 0,
						cacheCreationTokens: 0,
						cacheReadTokens: 0,
						totalTokens: 0,
						totalCost: 0,
					},
				};
				log(JSON.stringify(emptyOutput, null, 2));
			} else {
				logger.warn('No Claude usage data found.');
			}
			process.exit(0);
		}

		// Calculate totals
		const totals = calculateTotals(monthlyData);

		// Show debug information if requested
		if (mergedOptions.debug && !useJson) {
			const mismatchStats = await detectMismatches(
				undefined,
				Boolean(mergedOptions.offline),
				Boolean(mergedOptions.updatePricing),
			);
			printMismatchReport(mismatchStats, mergedOptions.debugSamples as number | undefined);
		}

		if (useJson) {
			// Output JSON format
			const jsonOutput = {
				monthly: monthlyData.map((data) => ({
					month: data.month,
					inputTokens: data.inputTokens,
					outputTokens: data.outputTokens,
					cacheCreationTokens: data.cacheCreationTokens,
					cacheReadTokens: data.cacheReadTokens,
					totalTokens: getTotalTokens(data),
					totalCost: data.totalCost,
					modelsUsed: data.modelsUsed,
					modelBreakdowns: data.modelBreakdowns,
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
			logger.box('Claude Code Token Usage Report - Monthly');

			// Create table with compact mode support
			const tableConfig: UsageReportConfig = {
				firstColumnName: 'Month',
				dateFormatter: (dateStr: string) =>
					formatDateCompact(
						dateStr,
						mergedOptions.timezone,
						mergedOptions.locale ?? DEFAULT_LOCALE,
					),
				forceCompact: ctx.values.compact,
			};
			const table = createUsageReportTable(tableConfig);

			// Add monthly data
			for (const data of monthlyData) {
				// Main row
				const row = formatUsageDataRow(data.month, {
					inputTokens: data.inputTokens,
					outputTokens: data.outputTokens,
					cacheCreationTokens: data.cacheCreationTokens,
					cacheReadTokens: data.cacheReadTokens,
					totalCost: data.totalCost,
					modelsUsed: data.modelsUsed,
				});
				table.push(row);

				// Add model breakdown rows if flag is set
				if (mergedOptions.breakdown) {
					pushBreakdownRows(table, data.modelBreakdowns);
				}
			}

			// Add empty row for visual separation before totals
			addEmptySeparatorRow(table, 8);

			// Add totals
			const totalsRow = formatTotalsRow({
				inputTokens: totals.inputTokens,
				outputTokens: totals.outputTokens,
				cacheCreationTokens: totals.cacheCreationTokens,
				cacheReadTokens: totals.cacheReadTokens,
				totalCost: totals.totalCost,
			});
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
