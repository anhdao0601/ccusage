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
import pc from 'picocolors';
import { loadConfig, mergeConfigWithArgs } from '../_config-loader-tokens.ts';
import { groupByProject, groupDataByProject } from '../_daily-grouping.ts';
import { formatDateCompact } from '../_date-utils.ts';
import { processWithJq } from '../_jq-processor.ts';
import { loadMultiToolDailyReport, renderMultiToolDailyTable } from '../_multi-tool-report.ts';
import { formatProjectName } from '../_project-names.ts';
import {
	getClaudeReportSources,
	getMultiToolReportSources,
	withReportCache,
} from '../_report-cache.ts';
import { sharedCommandConfig } from '../_shared-args.ts';
import { hasExplicitToolSelection, normalizeToolSelection } from '../_tool-selection.ts';
import { calculateTotals, createTotalsObject, getTotalTokens } from '../calculate-cost.ts';
import { loadDailyUsageData } from '../data-loader.ts';
import { detectMismatches, printMismatchReport } from '../debug.ts';
import { log, logger } from '../logger.ts';

export const dailyCommand = define({
	name: 'daily',
	description: 'Show usage report grouped by date',
	...sharedCommandConfig,
	args: {
		...sharedCommandConfig.args,
		instances: {
			type: 'boolean',
			short: 'i',
			description: 'Show usage breakdown by project/instance',
			default: false,
		},
		project: {
			type: 'string',
			short: 'p',
			description: 'Filter to specific project name',
		},
		projectAliases: {
			type: 'string',
			description:
				"Comma-separated project aliases (e.g., 'ccusage=Usage Tracker,myproject=My Project')",
			hidden: true,
		},
	},
	async run(ctx) {
		// Load configuration and merge with CLI arguments
		const config = loadConfig(ctx.values.config, ctx.values.debug);
		const mergedOptions = mergeConfigWithArgs(ctx, config, ctx.values.debug);

		// Convert projectAliases to Map if it exists
		// Parse comma-separated key=value pairs
		let projectAliases: Map<string, string> | undefined;
		if (mergedOptions.projectAliases != null && typeof mergedOptions.projectAliases === 'string') {
			projectAliases = new Map();
			const pairs = mergedOptions.projectAliases
				.split(',')
				.map((pair) => pair.trim())
				.filter((pair) => pair !== '');
			for (const pair of pairs) {
				const parts = pair.split('=').map((s) => s.trim());
				const rawName = parts[0];
				const alias = parts[1];
				if (rawName != null && alias != null && rawName !== '' && alias !== '') {
					projectAliases.set(rawName, alias);
				}
			}
		}

		// --jq implies --json
		const useJson = Boolean(mergedOptions.json) || mergedOptions.jq != null;
		if (useJson) {
			logger.level = 0;
		}

		if (hasExplicitToolSelection(mergedOptions.tool)) {
			if (mergedOptions.instances || mergedOptions.project != null) {
				logger.error('`--instances` and `--project` are only supported in Claude-only mode.');
				process.exit(1);
			}

			const tools = normalizeToolSelection(mergedOptions.tool);
			const report = await withReportCache({
				command: 'daily',
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
				sources: getMultiToolReportSources('daily', { tools }),
				pricing: {
					requiresPricing: tools.some((tool) => tool !== 'pi'),
					offline: mergedOptions.offline,
					updatePricing: mergedOptions.updatePricing,
				},
				load: async () =>
					loadMultiToolDailyReport({
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
					daily: [],
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
					daily: report.rows,
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

			logger.box('Multi-Tool Token Usage Report - Daily');
			log(
				renderMultiToolDailyTable(report, {
					compact: ctx.values.compact,
					timezone: mergedOptions.timezone,
					locale: mergedOptions.locale ?? undefined,
					breakdown: mergedOptions.breakdown,
				}),
			);
			return;
		}

		const dailyData = await withReportCache({
			command: 'daily',
			parameters: {
				mode: 'claude',
				since: mergedOptions.since,
				until: mergedOptions.until,
				timezone: mergedOptions.timezone,
				locale: mergedOptions.locale,
				order: mergedOptions.order,
				costMode: mergedOptions.mode,
				groupByProject: Boolean(mergedOptions.instances),
				project: mergedOptions.project ?? null,
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
				loadDailyUsageData({
					...mergedOptions,
					groupByProject: mergedOptions.instances,
				}),
		});

		if (dailyData.length === 0) {
			if (useJson) {
				log(JSON.stringify([]));
			} else {
				logger.warn('No Claude usage data found.');
			}
			process.exit(0);
		}

		// Calculate totals
		const totals = calculateTotals(dailyData);

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
			// Output JSON format - group by project if instances flag is used
			const jsonOutput =
				Boolean(mergedOptions.instances) && dailyData.some((d) => d.project != null)
					? {
							projects: groupByProject(dailyData),
							totals: createTotalsObject(totals),
						}
					: {
							daily: dailyData.map((data) => ({
								date: data.date,
								inputTokens: data.inputTokens,
								outputTokens: data.outputTokens,
								cacheCreationTokens: data.cacheCreationTokens,
								cacheReadTokens: data.cacheReadTokens,
								totalTokens: getTotalTokens(data),
								totalCost: data.totalCost,
								modelsUsed: data.modelsUsed,
								modelBreakdowns: data.modelBreakdowns,
								...(data.project != null && { project: data.project }),
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
			logger.box('Claude Code Token Usage Report - Daily');

			// Create table with compact mode support
			const tableConfig: UsageReportConfig = {
				firstColumnName: 'Date',
				dateFormatter: (dateStr: string) =>
					formatDateCompact(dateStr, mergedOptions.timezone, mergedOptions.locale ?? undefined),
				forceCompact: ctx.values.compact,
			};
			const table = createUsageReportTable(tableConfig);

			// Add daily data - group by project if instances flag is used
			if (Boolean(mergedOptions.instances) && dailyData.some((d) => d.project != null)) {
				// Group data by project for visual separation
				const projectGroups = groupDataByProject(dailyData);

				let isFirstProject = true;
				for (const [projectName, projectData] of Object.entries(projectGroups)) {
					// Add project section header
					if (!isFirstProject) {
						// Add empty row for visual separation between projects
						table.push(['', '', '', '', '', '', '', '']);
					}

					// Add project header row
					table.push([
						pc.cyan(`Project: ${formatProjectName(projectName, projectAliases)}`),
						'',
						'',
						'',
						'',
						'',
						'',
						'',
					]);

					// Add data rows for this project
					for (const data of projectData) {
						const row = formatUsageDataRow(data.date, {
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

					isFirstProject = false;
				}
			} else {
				// Standard display without project grouping
				for (const data of dailyData) {
					// Main row
					const row = formatUsageDataRow(data.date, {
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
