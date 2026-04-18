import type { TokenUsageDelta, TokenUsageEvent } from './_types.ts';
import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { createInterface } from 'node:readline';
import { mapWithConcurrency } from '@ccusage/internal/concurrency';
import { Result } from '@praha/byethrow';
import { createFixture } from 'fs-fixture';
import { glob } from 'tinyglobby';
import * as v from 'valibot';
import {
	CODEX_HOME_ENV,
	DEFAULT_ARCHIVED_SESSION_SUBDIR,
	DEFAULT_CODEX_DIR,
	DEFAULT_SESSION_SUBDIR,
	SESSION_GLOB,
} from './_consts.ts';
import { createSessionIndexEntry, readSessionIndex, writeSessionIndex } from './_session-index.ts';
import { logger } from './logger.ts';

type RawUsage = {
	input_tokens: number;
	cached_input_tokens: number;
	output_tokens: number;
	reasoning_output_tokens: number;
	total_tokens: number;
};

function ensureNumber(value: unknown): number {
	return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

/**
 * Normalize Codex `token_count` payloads into a predictable shape.
 *
 * Codex reports four counters:
 *   - input_tokens
 *   - cached_input_tokens (a.k.a cache_read_input_tokens)
 *   - output_tokens (this already includes any reasoning charge)
 *   - reasoning_output_tokens (informational only)
 *
 * Modern JSONL entries also provide `total_tokens`, but legacy ones may omit it.
 * When that happens we mirror Codex' billing behavior and synthesize
 * `input + output` (reasoning is treated as part of output, not an extra charge).
 */
function normalizeRawUsage(value: unknown): RawUsage | null {
	if (value == null || typeof value !== 'object') {
		return null;
	}

	const record = value as Record<string, unknown>;
	const input = ensureNumber(record.input_tokens);
	const cached = ensureNumber(record.cached_input_tokens ?? record.cache_read_input_tokens);
	const output = ensureNumber(record.output_tokens);
	const reasoning = ensureNumber(record.reasoning_output_tokens);
	const total = ensureNumber(record.total_tokens);

	return {
		input_tokens: input,
		cached_input_tokens: cached,
		output_tokens: output,
		reasoning_output_tokens: reasoning,
		// LiteLLM pricing treats reasoning tokens as part of the normal output price. Codex
		// includes them as a separate field but does not add them to total_tokens, so when we
		// have to synthesize a total (legacy logs), we mirror that behavior with input+output.
		total_tokens: total > 0 ? total : input + output,
	};
}

function subtractRawUsage(current: RawUsage, previous: RawUsage | null): RawUsage {
	return {
		input_tokens: Math.max(current.input_tokens - (previous?.input_tokens ?? 0), 0),
		cached_input_tokens: Math.max(
			current.cached_input_tokens - (previous?.cached_input_tokens ?? 0),
			0,
		),
		output_tokens: Math.max(current.output_tokens - (previous?.output_tokens ?? 0), 0),
		reasoning_output_tokens: Math.max(
			current.reasoning_output_tokens - (previous?.reasoning_output_tokens ?? 0),
			0,
		),
		total_tokens: Math.max(current.total_tokens - (previous?.total_tokens ?? 0), 0),
	};
}

/**
 * Convert cumulative usage into a per-event delta.
 *
 * Codex includes the cost of reasoning inside `output_tokens`. The
 * `reasoning_output_tokens` field is useful for display/debug purposes, but we
 * must not add it to the billable output again. For legacy totals we therefore
 * fallback to `input + output`.
 */
function convertToDelta(raw: RawUsage): TokenUsageDelta {
	const total = raw.total_tokens > 0 ? raw.total_tokens : raw.input_tokens + raw.output_tokens;

	const cached = Math.min(raw.cached_input_tokens, raw.input_tokens);

	return {
		inputTokens: raw.input_tokens,
		cachedInputTokens: cached,
		outputTokens: raw.output_tokens,
		reasoningOutputTokens: raw.reasoning_output_tokens,
		totalTokens: total,
	};
}

const recordSchema = v.record(v.string(), v.unknown());
const LEGACY_FALLBACK_MODEL = 'gpt-5';
const FORK_BOOTSTRAP_MARKER = 'You are the newly spawned agent.';
const SESSION_FILE_LOAD_CONCURRENCY = 8;

const entrySchema = v.object({
	type: v.string(),
	payload: v.optional(v.unknown()),
	timestamp: v.optional(v.string()),
});

const tokenCountPayloadSchema = v.object({
	type: v.literal('token_count'),
	info: v.optional(recordSchema),
});

const userMessagePayloadSchema = v.object({
	type: v.literal('user_message'),
});

const sessionMetaPayloadSchema = v.object({
	forked_from_id: v.optional(v.string()),
});

function extractModel(value: unknown): string | undefined {
	const parsed = v.safeParse(recordSchema, value);
	if (!parsed.success) {
		return undefined;
	}

	const payload = parsed.output;

	const infoCandidate = payload.info;
	if (infoCandidate != null) {
		const infoParsed = v.safeParse(recordSchema, infoCandidate);
		if (infoParsed.success) {
			const info = infoParsed.output;
			const directCandidates = [info.model, info.model_name];
			for (const candidate of directCandidates) {
				const model = asNonEmptyString(candidate);
				if (model != null) {
					return model;
				}
			}

			if (info.metadata != null) {
				const metadataParsed = v.safeParse(recordSchema, info.metadata);
				if (metadataParsed.success) {
					const model = asNonEmptyString(metadataParsed.output.model);
					if (model != null) {
						return model;
					}
				}
			}
		}
	}

	const fallbackModel = asNonEmptyString(payload.model);
	if (fallbackModel != null) {
		return fallbackModel;
	}

	if (payload.metadata != null) {
		const metadataParsed = v.safeParse(recordSchema, payload.metadata);
		if (metadataParsed.success) {
			const model = asNonEmptyString(metadataParsed.output.model);
			if (model != null) {
				return model;
			}
		}
	}

	return undefined;
}

function asNonEmptyString(value: unknown): string | undefined {
	if (typeof value !== 'string') {
		return undefined;
	}

	const trimmed = value.trim();
	return trimmed === '' ? undefined : trimmed;
}

export type LoadOptions = {
	sessionDirs?: string[];
};

export type LoadResult = {
	events: TokenUsageEvent[];
	missingDirectories: string[];
};

type SessionDirectorySource = {
	path: string;
	reportMissing: boolean;
};

type SessionFileCandidate = {
	file: string;
	sessionId: string;
	size: number;
	mtimeMs: number;
};

function extractEntryModelFromParsedEntry(
	entry: v.InferOutput<typeof entrySchema>,
): string | undefined {
	if (entry.type === 'turn_context') {
		const payloadRecord = v.safeParse(recordSchema, entry.payload ?? null);
		if (payloadRecord.success) {
			return extractModel(payloadRecord.output);
		}

		return undefined;
	}

	if (entry.payload == null) {
		return undefined;
	}

	const payloadRecord = v.safeParse(recordSchema, entry.payload);
	if (!payloadRecord.success) {
		return undefined;
	}

	const infoRecord = v.safeParse(recordSchema, payloadRecord.output.info ?? null);
	const extractionSource = infoRecord.success
		? Object.assign({}, payloadRecord.output, { info: infoRecord.output })
		: payloadRecord.output;

	return extractModel(extractionSource);
}

type SessionProcessingResult = {
	events: TokenUsageEvent[];
	legacyFallbackUsed: boolean;
	retryReason?: 'missing_bootstrap' | 'missing_child_user';
	forkedFromId?: string;
};

async function processSessionStream(
	file: string,
	sessionId: string,
	forceFullTranscript = false,
): Promise<SessionProcessingResult> {
	const fileStream = createReadStream(file, { encoding: 'utf8' });
	const rl = createInterface({ input: fileStream, crlfDelay: Infinity });

	const events: TokenUsageEvent[] = [];
	let previousTotals: RawUsage | null = null;
	let currentModel: string | undefined;
	let currentModelIsFallback = false;
	let legacyFallbackUsed = false;
	let lineNumber = 0;
	let forkedFromId: string | undefined;
	let isForkedSession = false;
	let activated = forceFullTranscript;
	let sawBootstrapMarker = false;
	let sawChildUserMessage = false;
	let lastModelBeforeActivation: string | undefined;

	for await (const line of rl) {
		lineNumber += 1;
		const trimmed = line.trim();
		if (trimmed === '') {
			continue;
		}

		const parseLine = Result.try({
			try: () => JSON.parse(trimmed) as unknown,
			catch: (error) => error,
		});
		const parsedResult = parseLine();
		if (Result.isFailure(parsedResult)) {
			continue;
		}

		const entryParse = v.safeParse(entrySchema, parsedResult.value);
		if (!entryParse.success) {
			continue;
		}

		const entry = entryParse.output;
		if (lineNumber === 1 && entry.type === 'session_meta') {
			const sessionMetaPayload = v.safeParse(sessionMetaPayloadSchema, entry.payload ?? null);
			forkedFromId = sessionMetaPayload.success
				? asNonEmptyString(sessionMetaPayload.output.forked_from_id)
				: undefined;
			isForkedSession = forkedFromId != null;
		}

		if (isForkedSession && !forceFullTranscript && !activated) {
			const modelBeforeActivation = extractEntryModelFromParsedEntry(entry);
			if (modelBeforeActivation != null) {
				lastModelBeforeActivation = modelBeforeActivation;
			}

			if (trimmed.includes(FORK_BOOTSTRAP_MARKER)) {
				sawBootstrapMarker = true;
			}

			if (
				sawBootstrapMarker &&
				entry.type === 'event_msg' &&
				v.safeParse(userMessagePayloadSchema, entry.payload ?? null).success
			) {
				activated = true;
				sawChildUserMessage = true;
				currentModel = lastModelBeforeActivation;
				currentModelIsFallback = false;
			}

			continue;
		}

		if (entry.type === 'turn_context') {
			const contextPayload = v.safeParse(recordSchema, entry.payload ?? null);
			if (contextPayload.success) {
				const contextModel = extractModel(contextPayload.output);
				if (contextModel != null) {
					currentModel = contextModel;
					currentModelIsFallback = false;
				}
			}
			continue;
		}

		if (entry.type !== 'event_msg') {
			continue;
		}

		const tokenPayloadResult = v.safeParse(tokenCountPayloadSchema, entry.payload ?? undefined);
		if (!tokenPayloadResult.success || entry.timestamp == null) {
			continue;
		}

		const info = tokenPayloadResult.output.info;
		const lastUsage = normalizeRawUsage(info?.last_token_usage);
		const totalUsage = normalizeRawUsage(info?.total_token_usage);

		let raw = lastUsage;
		if (raw == null && totalUsage != null) {
			raw = subtractRawUsage(totalUsage, previousTotals);
		}

		if (totalUsage != null) {
			previousTotals = totalUsage;
		}

		if (raw == null) {
			continue;
		}

		const delta = convertToDelta(raw);
		if (
			delta.inputTokens === 0 &&
			delta.cachedInputTokens === 0 &&
			delta.outputTokens === 0 &&
			delta.reasoningOutputTokens === 0
		) {
			continue;
		}

		const payloadRecordResult = v.safeParse(recordSchema, entry.payload ?? undefined);
		const extractionSource = payloadRecordResult.success
			? Object.assign({}, payloadRecordResult.output, { info })
			: { info };
		const extractedModel = extractModel(extractionSource);
		let isFallbackModel = false;
		if (extractedModel != null) {
			currentModel = extractedModel;
			currentModelIsFallback = false;
		}

		let model = extractedModel ?? currentModel;
		if (model == null) {
			model = LEGACY_FALLBACK_MODEL;
			isFallbackModel = true;
			legacyFallbackUsed = true;
			currentModel = model;
			currentModelIsFallback = true;
		} else if (extractedModel == null && currentModelIsFallback) {
			isFallbackModel = true;
		}

		const event: TokenUsageEvent = {
			sessionId,
			timestamp: entry.timestamp,
			model,
			inputTokens: delta.inputTokens,
			cachedInputTokens: delta.cachedInputTokens,
			outputTokens: delta.outputTokens,
			reasoningOutputTokens: delta.reasoningOutputTokens,
			totalTokens: delta.totalTokens,
		};

		if (isFallbackModel) {
			event.isFallbackModel = true;
		}

		events.push(event);
	}

	if (isForkedSession && !forceFullTranscript && !activated) {
		return {
			events: [],
			legacyFallbackUsed,
			forkedFromId,
			retryReason:
				sawBootstrapMarker || sawChildUserMessage ? 'missing_child_user' : 'missing_bootstrap',
		};
	}

	return { events, legacyFallbackUsed };
}

async function loadEventsFromSessionFile(
	file: string,
	sessionId: string,
): Promise<SessionProcessingResult> {
	const firstPass = await processSessionStream(file, sessionId, false);
	if (firstPass.retryReason == null) {
		return firstPass;
	}

	if (firstPass.retryReason === 'missing_bootstrap') {
		logger.debug('Forked Codex session missing bootstrap marker; counting full transcript', {
			file,
			forkedFromId: firstPass.forkedFromId,
		});
	} else {
		logger.debug(
			'Forked Codex session missing post-bootstrap user message; counting full transcript',
			{
				file,
				forkedFromId: firstPass.forkedFromId,
			},
		);
	}

	return processSessionStream(file, sessionId, true);
}

export async function loadTokenUsageEvents(options: LoadOptions = {}): Promise<LoadResult> {
	const providedDirs =
		options.sessionDirs != null && options.sessionDirs.length > 0
			? options.sessionDirs.map((dir) => path.resolve(dir))
			: undefined;

	const codexHomeEnv = process.env[CODEX_HOME_ENV]?.trim();
	const codexHome =
		codexHomeEnv != null && codexHomeEnv !== '' ? path.resolve(codexHomeEnv) : DEFAULT_CODEX_DIR;
	const sessionDirectories: SessionDirectorySource[] =
		providedDirs != null
			? providedDirs.map((dir) => ({ path: dir, reportMissing: true }))
			: [
					{
						path: path.join(codexHome, DEFAULT_SESSION_SUBDIR),
						reportMissing: true,
					},
					{
						path: path.join(codexHome, DEFAULT_ARCHIVED_SESSION_SUBDIR),
						reportMissing: false,
					},
				];

	const events: TokenUsageEvent[] = [];
	const missingDirectories: string[] = [];
	const loadedFiles = new Set<string>();
	const sessionFiles: Array<{
		file: string;
		sessionId: string;
	}> = [];

	for (const directory of sessionDirectories) {
		const directoryPath = path.resolve(directory.path);
		const statResult = await Result.try({
			try: stat(directoryPath),
			catch: (error) => error,
		});

		if (Result.isFailure(statResult)) {
			if (directory.reportMissing) {
				missingDirectories.push(directoryPath);
			}
			continue;
		}

		if (!statResult.value.isDirectory()) {
			if (directory.reportMissing) {
				missingDirectories.push(directoryPath);
			}
			continue;
		}

		const files = await glob(SESSION_GLOB, {
			cwd: directoryPath,
			absolute: true,
		});

		for (const file of files) {
			if (loadedFiles.has(file)) {
				continue;
			}

			loadedFiles.add(file);
			const relativeSessionPath = path.relative(directoryPath, file);
			const normalizedSessionPath = relativeSessionPath.split(path.sep).join('/');
			const sessionId = normalizedSessionPath.replace(/\.jsonl$/i, '');
			sessionFiles.push({ file, sessionId });
		}
	}

	const indexedEntries = await readSessionIndex();
	const sessionCandidates = (
		await mapWithConcurrency(
			sessionFiles,
			SESSION_FILE_LOAD_CONCURRENCY,
			async ({ file, sessionId }) => {
				const fileStat = await Result.try({
					try: stat(file),
					catch: (error) => error,
				});
				if (Result.isFailure(fileStat) || !fileStat.value.isFile()) {
					return null;
				}

				return {
					file,
					sessionId,
					size: fileStat.value.size,
					mtimeMs: fileStat.value.mtimeMs,
				} satisfies SessionFileCandidate;
			},
		)
	).filter((candidate): candidate is SessionFileCandidate => candidate != null);

	const filesToLoad: SessionFileCandidate[] = [];
	for (const candidate of sessionCandidates) {
		const cached = indexedEntries.get(candidate.file);
		if (
			cached != null &&
			cached.sessionId === candidate.sessionId &&
			cached.size === candidate.size &&
			cached.mtimeMs === candidate.mtimeMs
		) {
			events.push(...cached.events);
			continue;
		}

		filesToLoad.push(candidate);
	}

	const sessionResults = await mapWithConcurrency(
		filesToLoad,
		SESSION_FILE_LOAD_CONCURRENCY,
		async ({ file, sessionId, size, mtimeMs }) => ({
			file,
			sessionId,
			size,
			mtimeMs,
			sessionResult: await Result.try({
				try: loadEventsFromSessionFile(file, sessionId),
				catch: (error) => error,
			}),
		}),
	);

	const shouldWriteIndex = filesToLoad.length > 0;
	for (const { file, sessionId, size, mtimeMs, sessionResult } of sessionResults) {
		if (Result.isFailure(sessionResult)) {
			logger.debug('Failed to read Codex session file', sessionResult.error);
			indexedEntries.delete(file);
			continue;
		}

		events.push(...sessionResult.value.events);
		indexedEntries.set(
			file,
			createSessionIndexEntry({
				file,
				sessionId,
				size,
				mtimeMs,
				events: sessionResult.value.events,
			}),
		);

		if (sessionResult.value.legacyFallbackUsed) {
			logger.debug('Legacy Codex session lacked model metadata; applied fallback', {
				file,
				model: LEGACY_FALLBACK_MODEL,
			});
		}
	}

	if (shouldWriteIndex) {
		await writeSessionIndex(indexedEntries.values());
	}

	events.sort((a, b) => new Date(a.timestamp).getTime() - new Date(b.timestamp).getTime());

	return { events, missingDirectories };
}

if (import.meta.vitest != null) {
	describe('loadTokenUsageEvents', () => {
		it('parses token_count events and skips entries without model metadata', async () => {
			await using fixture = await createFixture({
				sessions: {
					'project-1.jsonl': [
						JSON.stringify({
							timestamp: '2025-09-11T18:25:30.000Z',
							type: 'turn_context',
							payload: {
								model: 'gpt-5',
							},
						}),
						JSON.stringify({
							timestamp: '2025-09-11T18:25:40.670Z',
							type: 'event_msg',
							payload: {
								type: 'token_count',
								info: {
									total_token_usage: {
										input_tokens: 1_200,
										cached_input_tokens: 200,
										output_tokens: 500,
										reasoning_output_tokens: 0,
										total_tokens: 1_700,
									},
									last_token_usage: {
										input_tokens: 1_200,
										cached_input_tokens: 200,
										output_tokens: 500,
										reasoning_output_tokens: 0,
										total_tokens: 1_700,
									},
									model: 'gpt-5',
								},
							},
						}),
						JSON.stringify({
							timestamp: '2025-09-11T18:40:00.000Z',
							type: 'turn_context',
							payload: {
								model: 'gpt-5',
							},
						}),
						JSON.stringify({
							timestamp: '2025-09-12T00:00:00.000Z',
							type: 'event_msg',
							payload: {
								type: 'token_count',
								info: {
									total_token_usage: {
										input_tokens: 2_000,
										cached_input_tokens: 300,
										output_tokens: 800,
										reasoning_output_tokens: 0,
										total_tokens: 2_800,
									},
								},
							},
						}),
					].join('\n'),
				},
			});

			expect(await fixture.exists('sessions/project-1.jsonl')).toBe(true);

			const { events, missingDirectories } = await loadTokenUsageEvents({
				sessionDirs: [fixture.getPath('sessions')],
			});
			expect(missingDirectories).toEqual([]);

			expect(events).toHaveLength(2);
			const first = events[0]!;
			expect(first.model).toBe('gpt-5');
			expect(first.inputTokens).toBe(1_200);
			expect(first.cachedInputTokens).toBe(200);
			const second = events[1]!;
			expect(second.model).toBe('gpt-5');
			expect(second.inputTokens).toBe(800);
			expect(second.cachedInputTokens).toBe(100);
		});

		it('falls back to legacy model when metadata is missing entirely', async () => {
			await using fixture = await createFixture({
				sessions: {
					'legacy.jsonl': [
						JSON.stringify({
							timestamp: '2025-09-15T13:00:00.000Z',
							type: 'event_msg',
							payload: {
								type: 'token_count',
								info: {
									total_token_usage: {
										input_tokens: 5_000,
										cached_input_tokens: 0,
										output_tokens: 1_000,
										reasoning_output_tokens: 0,
										total_tokens: 6_000,
									},
								},
							},
						}),
					].join('\n'),
				},
			});

			const { events } = await loadTokenUsageEvents({
				sessionDirs: [fixture.getPath('sessions')],
			});
			expect(events).toHaveLength(1);
			expect(events[0]!.model).toBe('gpt-5');
			expect(events[0]!.isFallbackModel).toBe(true);
		});

		it('loads both active and archived sessions by default CODEX_HOME paths', async () => {
			await using fixture = await createFixture({
				sessions: {
					'2026/02/10/active.jsonl': JSON.stringify({
						timestamp: '2026-02-10T10:00:00.000Z',
						type: 'event_msg',
						payload: {
							type: 'token_count',
							info: {
								last_token_usage: {
									input_tokens: 100,
									cached_input_tokens: 10,
									output_tokens: 20,
									reasoning_output_tokens: 0,
									total_tokens: 120,
								},
								model: 'gpt-5',
							},
						},
					}),
				},
				archived_sessions: {
					'archived.jsonl': JSON.stringify({
						timestamp: '2026-02-10T11:00:00.000Z',
						type: 'event_msg',
						payload: {
							type: 'token_count',
							info: {
								last_token_usage: {
									input_tokens: 200,
									cached_input_tokens: 0,
									output_tokens: 40,
									reasoning_output_tokens: 0,
									total_tokens: 240,
								},
								model: 'gpt-5',
							},
						},
					}),
				},
			});

			const previousCodexHome = process.env[CODEX_HOME_ENV];
			process.env[CODEX_HOME_ENV] = fixture.getPath('.');

			try {
				const { events, missingDirectories } = await loadTokenUsageEvents();
				expect(missingDirectories).toEqual([]);
				expect(events).toHaveLength(2);
				expect(events.map((event) => event.sessionId)).toEqual(['2026/02/10/active', 'archived']);
			} finally {
				if (previousCodexHome == null) {
					delete process.env[CODEX_HOME_ENV];
				} else {
					process.env[CODEX_HOME_ENV] = previousCodexHome;
				}
			}
		});

		it('treats archived session directory as optional in default discovery', async () => {
			await using fixture = await createFixture({
				sessions: {
					'2026/02/10/active.jsonl': JSON.stringify({
						timestamp: '2026-02-10T10:00:00.000Z',
						type: 'event_msg',
						payload: {
							type: 'token_count',
							info: {
								last_token_usage: {
									input_tokens: 100,
									cached_input_tokens: 10,
									output_tokens: 20,
									reasoning_output_tokens: 0,
									total_tokens: 120,
								},
								model: 'gpt-5',
							},
						},
					}),
				},
			});

			const previousCodexHome = process.env[CODEX_HOME_ENV];
			process.env[CODEX_HOME_ENV] = fixture.getPath('.');

			try {
				const { events, missingDirectories } = await loadTokenUsageEvents();
				expect(events).toHaveLength(1);
				expect(missingDirectories).toEqual([]);
			} finally {
				if (previousCodexHome == null) {
					delete process.env[CODEX_HOME_ENV];
				} else {
					process.env[CODEX_HOME_ENV] = previousCodexHome;
				}
			}
		});

		it('ignores inherited fork transcript before the child task starts', async () => {
			await using fixture = await createFixture({
				sessions: {
					'forked-child.jsonl': [
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.000Z',
							type: 'session_meta',
							payload: {
								id: 'forked-child',
								forked_from_id: 'parent-session',
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.100Z',
							type: 'event_msg',
							payload: {
								type: 'user_message',
								message: 'parent prompt',
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.200Z',
							type: 'turn_context',
							payload: {
								model: 'gpt-5.4',
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.300Z',
							type: 'event_msg',
							payload: {
								type: 'token_count',
								info: {
									last_token_usage: {
										input_tokens: 400_000,
										cached_input_tokens: 390_000,
										output_tokens: 1_000,
										reasoning_output_tokens: 200,
										total_tokens: 401_000,
									},
									model: 'gpt-5.4',
								},
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.400Z',
							type: 'response_item',
							payload: {
								type: 'function_call_output',
								output:
									'You are the newly spawned agent. The prior conversation history was forked from your parent agent.',
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.500Z',
							type: 'event_msg',
							payload: {
								type: 'user_message',
								message: 'child prompt',
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.600Z',
							type: 'event_msg',
							payload: {
								type: 'token_count',
								info: {
									last_token_usage: {
										input_tokens: 1_200,
										cached_input_tokens: 800,
										output_tokens: 120,
										reasoning_output_tokens: 20,
										total_tokens: 1_320,
									},
									total_token_usage: {
										input_tokens: 1_200,
										cached_input_tokens: 800,
										output_tokens: 120,
										reasoning_output_tokens: 20,
										total_tokens: 1_320,
									},
								},
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.700Z',
							type: 'event_msg',
							payload: {
								type: 'token_count',
								info: {
									total_token_usage: {
										input_tokens: 2_000,
										cached_input_tokens: 1_400,
										output_tokens: 300,
										reasoning_output_tokens: 30,
										total_tokens: 2_300,
									},
								},
							},
						}),
					].join('\n'),
				},
			});

			const { events } = await loadTokenUsageEvents({
				sessionDirs: [fixture.getPath('sessions')],
			});

			expect(events).toHaveLength(2);
			expect(events.map((event) => event.inputTokens)).toEqual([1_200, 800]);
			expect(events.map((event) => event.cachedInputTokens)).toEqual([800, 600]);
			expect(events.map((event) => event.outputTokens)).toEqual([120, 180]);
			expect(events.map((event) => event.model)).toEqual(['gpt-5.4', 'gpt-5.4']);
			expect(events.some((event) => event.isFallbackModel === true)).toBe(false);
		});

		it('falls back to the full transcript when a forked session lacks the bootstrap marker', async () => {
			await using fixture = await createFixture({
				sessions: {
					'forked-without-marker.jsonl': [
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.000Z',
							type: 'session_meta',
							payload: {
								id: 'forked-without-marker',
								forked_from_id: 'parent-session',
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.100Z',
							type: 'turn_context',
							payload: {
								model: 'gpt-5.4',
							},
						}),
						JSON.stringify({
							timestamp: '2026-03-08T10:26:42.200Z',
							type: 'event_msg',
							payload: {
								type: 'token_count',
								info: {
									last_token_usage: {
										input_tokens: 900,
										cached_input_tokens: 600,
										output_tokens: 90,
										reasoning_output_tokens: 9,
										total_tokens: 990,
									},
									model: 'gpt-5.4',
								},
							},
						}),
					].join('\n'),
				},
			});

			const loggerDebug = vi.spyOn(logger, 'debug').mockImplementation(() => {});

			const { events } = await loadTokenUsageEvents({
				sessionDirs: [fixture.getPath('sessions')],
			});

			expect(events).toHaveLength(1);
			expect(events[0]!.inputTokens).toBe(900);
			expect(events[0]!.cachedInputTokens).toBe(600);
			expect(events[0]!.outputTokens).toBe(90);
			expect(loggerDebug).toHaveBeenCalledWith(
				'Forked Codex session missing bootstrap marker; counting full transcript',
				expect.objectContaining({
					file: fixture.getPath('sessions/forked-without-marker.jsonl'),
					forkedFromId: 'parent-session',
				}),
			);
			loggerDebug.mockRestore();
		});
	});
}
