import type { ToolName } from './_tool-selection.ts';
import { createHash } from 'node:crypto';
import { mkdir, readFile, rename, stat, unlink, writeFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { toArray } from '@antfu/utils';
import { mapWithConcurrency } from '@ccusage/internal/concurrency';
import { isPricingCacheFreshForDate, readPricingCacheEnvelope } from '@ccusage/internal/pricing';
import { glob } from 'tinyglobby';
import { AMP_THREAD_GLOB, AMP_THREADS_DIR_NAME, DEFAULT_AMP_DIR } from '../../amp/src/_consts.ts';
import {
	CODEX_HOME_ENV,
	DEFAULT_ARCHIVED_SESSION_SUBDIR,
	DEFAULT_CODEX_DIR,
	DEFAULT_SESSION_SUBDIR,
	SESSION_GLOB,
} from '../../codex/src/_consts.ts';
import { getOpenCodePath } from '../../opencode/src/data-loader.ts';
import { getPiAgentPaths } from '../../pi/src/_pi-agent.ts';
import { CLAUDE_PROJECTS_DIR_NAME, USAGE_DATA_GLOB_PATTERN } from './_consts.ts';
import { getClaudePaths } from './data-loader.ts';

const REPORT_CACHE_SCHEMA_VERSION = 1;
const CACHE_DIRECTORY_NAME = 'ccusage';
const REPORT_CACHE_SUBDIR = 'reports';
const FILE_FINGERPRINT_CONCURRENCY = 32;

type ReportCommand = 'daily' | 'monthly' | 'session';

type ReportCacheEntry<T> = {
	schemaVersion: number;
	createdAt: string;
	payload: T;
};

type CacheSource = {
	id: string;
	dir: string;
	patterns: string[];
};

type PricingState = {
	allowRead: boolean;
	fingerprint: string;
};

export type ReportCacheOptions<T> = {
	command: ReportCommand;
	parameters: unknown;
	sources: CacheSource[];
	pricing: {
		requiresPricing: boolean;
		offline?: boolean;
		updatePricing?: boolean;
	};
	load: () => Promise<T>;
};

function getReportCacheDirectory(): string {
	const cacheHome = process.env.XDG_CACHE_HOME ?? path.join(homedir(), '.cache');
	return path.join(cacheHome, CACHE_DIRECTORY_NAME, REPORT_CACHE_SUBDIR);
}

function createCachePath(key: string): string {
	return path.join(getReportCacheDirectory(), `${key}.json`);
}

function normalizePatterns(patterns: string | string[]): string[] {
	return Array.isArray(patterns) ? patterns : [patterns];
}

async function readReportCache<T>(key: string): Promise<T | null> {
	try {
		const content = await readFile(createCachePath(key), 'utf-8');
		const parsed = JSON.parse(content) as ReportCacheEntry<T>;
		if (
			typeof parsed !== 'object' ||
			parsed == null ||
			parsed.schemaVersion !== REPORT_CACHE_SCHEMA_VERSION ||
			!('payload' in parsed)
		) {
			return null;
		}

		return parsed.payload;
	} catch {
		return null;
	}
}

async function writeReportCache<T>(key: string, payload: T): Promise<void> {
	const cachePath = createCachePath(key);
	const tempPath = `${cachePath}.${process.pid}.${Date.now()}.tmp`;
	await mkdir(path.dirname(cachePath), { recursive: true });

	try {
		await writeFile(
			tempPath,
			JSON.stringify(
				{
					schemaVersion: REPORT_CACHE_SCHEMA_VERSION,
					createdAt: new Date().toISOString(),
					payload,
				} satisfies ReportCacheEntry<T>,
				null,
				'\t',
			),
		);
		await rename(tempPath, cachePath);
	} finally {
		await unlink(tempPath).catch(() => undefined);
	}
}

function createCacheKey(input: unknown): string {
	return createHash('sha256').update(JSON.stringify(input)).digest('hex');
}

async function fingerprintSource(source: CacheSource): Promise<{
	id: string;
	dir: string;
	fileCount: number;
	newestMtimeMs: number;
}> {
	const resolvedDir = path.resolve(source.dir);

	try {
		const dirStat = await stat(resolvedDir);
		if (!dirStat.isDirectory()) {
			return {
				id: source.id,
				dir: resolvedDir,
				fileCount: 0,
				newestMtimeMs: 0,
			};
		}
	} catch {
		return {
			id: source.id,
			dir: resolvedDir,
			fileCount: 0,
			newestMtimeMs: 0,
		};
	}

	const files = await glob(source.patterns, {
		cwd: resolvedDir,
		absolute: true,
	});

	if (files.length === 0) {
		return {
			id: source.id,
			dir: resolvedDir,
			fileCount: 0,
			newestMtimeMs: 0,
		};
	}

	const mtimes = await mapWithConcurrency(files, FILE_FINGERPRINT_CONCURRENCY, async (file) => {
		try {
			const fileStat = await stat(file);
			return fileStat.mtimeMs;
		} catch {
			return 0;
		}
	});

	return {
		id: source.id,
		dir: resolvedDir,
		fileCount: files.length,
		newestMtimeMs: Math.max(0, ...mtimes),
	};
}

async function computeSourceFingerprint(sources: CacheSource[]): Promise<string> {
	const snapshots = await mapWithConcurrency(
		sources,
		Math.min(Math.max(sources.length, 1), 8),
		async (source) => fingerprintSource(source),
	);

	const normalized = snapshots.sort((left, right) => {
		const idCompare = left.id.localeCompare(right.id);
		return idCompare !== 0 ? idCompare : left.dir.localeCompare(right.dir);
	});

	return createCacheKey({
		schemaVersion: REPORT_CACHE_SCHEMA_VERSION,
		sources: normalized,
	});
}

async function resolvePricingState(
	options: ReportCacheOptions<unknown>['pricing'],
): Promise<PricingState> {
	if (!options.requiresPricing) {
		return {
			allowRead: true,
			fingerprint: 'pricing:none',
		};
	}

	if (options.updatePricing === true) {
		return {
			allowRead: false,
			fingerprint: 'pricing:force-refresh',
		};
	}

	const envelope = await readPricingCacheEnvelope();
	if (options.offline === true) {
		return {
			allowRead: true,
			fingerprint:
				envelope != null ? `pricing:offline:${envelope.fetchedAt}` : 'pricing:offline:none',
		};
	}

	if (envelope == null) {
		return {
			allowRead: false,
			fingerprint: 'pricing:online:missing',
		};
	}

	const isFresh = isPricingCacheFreshForDate(envelope.fetchedAt);
	return {
		allowRead: isFresh,
		fingerprint: isFresh
			? `pricing:online:${envelope.fetchedAt}`
			: `pricing:online:stale:${envelope.fetchedAt}`,
	};
}

export async function withReportCache<T>(options: ReportCacheOptions<T>): Promise<T> {
	const sourceFingerprint = await computeSourceFingerprint(options.sources);
	const initialPricingState = await resolvePricingState(options.pricing);
	const initialKey = createCacheKey({
		schemaVersion: REPORT_CACHE_SCHEMA_VERSION,
		command: options.command,
		parameters: options.parameters,
		sourceFingerprint,
		pricing: initialPricingState.fingerprint,
	});

	if (initialPricingState.allowRead) {
		const cached = await readReportCache<T>(initialKey);
		if (cached != null) {
			return cached;
		}
	}

	const payload = await options.load();
	const finalPricingState = await resolvePricingState(options.pricing);
	const finalKey = createCacheKey({
		schemaVersion: REPORT_CACHE_SCHEMA_VERSION,
		command: options.command,
		parameters: options.parameters,
		sourceFingerprint,
		pricing: finalPricingState.fingerprint,
	});
	await writeReportCache(finalKey, payload);
	return payload;
}

export function getClaudeReportSources(claudePath?: string | string[]): CacheSource[] {
	return toArray(claudePath ?? getClaudePaths()).map((dir, index) => ({
		id: `claude:${index}`,
		dir,
		patterns: [path.join(CLAUDE_PROJECTS_DIR_NAME, USAGE_DATA_GLOB_PATTERN)],
	}));
}

export function getCodexReportSources(sessionDirs?: string[]): CacheSource[] {
	const providedDirs =
		sessionDirs != null && sessionDirs.length > 0
			? sessionDirs.map((dir) => path.resolve(dir))
			: undefined;

	const codexHomeEnv = process.env[CODEX_HOME_ENV]?.trim();
	const codexHome =
		codexHomeEnv != null && codexHomeEnv !== '' ? path.resolve(codexHomeEnv) : DEFAULT_CODEX_DIR;

	const dirs = providedDirs ?? [
		path.join(codexHome, DEFAULT_SESSION_SUBDIR),
		path.join(codexHome, DEFAULT_ARCHIVED_SESSION_SUBDIR),
	];

	return dirs.map((dir, index) => ({
		id: `codex:${index}`,
		dir,
		patterns: normalizePatterns(SESSION_GLOB),
	}));
}

export function getOpenCodeReportSources(command: ReportCommand): CacheSource[] {
	const openCodePath = getOpenCodePath();
	if (openCodePath == null) {
		return [];
	}

	return [
		{
			id: 'opencode:messages',
			dir: openCodePath,
			patterns: ['storage/message/**/*.json'],
		},
		...(command === 'session'
			? [
					{
						id: 'opencode:sessions',
						dir: openCodePath,
						patterns: ['storage/session/**/*.json'],
					},
				]
			: []),
	];
}

export function getPiReportSources(piPath?: string): CacheSource[] {
	return getPiAgentPaths(piPath).map((dir, index) => ({
		id: `pi:${index}`,
		dir,
		patterns: ['**/*.jsonl'],
	}));
}

export function getAmpReportSources(threadDirs?: string[]): CacheSource[] {
	const resolvedDirs =
		threadDirs != null && threadDirs.length > 0
			? threadDirs.map((dir) => path.resolve(dir))
			: [path.join(DEFAULT_AMP_DIR, AMP_THREADS_DIR_NAME)];

	return resolvedDirs.map((dir, index) => ({
		id: `amp:${index}`,
		dir,
		patterns: normalizePatterns(AMP_THREAD_GLOB),
	}));
}

export function getMultiToolReportSources(
	command: ReportCommand,
	options: {
		tools: readonly ToolName[];
		claudePath?: string | string[];
		codexSessionDirs?: string[];
		piPath?: string;
		ampThreadDirs?: string[];
	},
): CacheSource[] {
	const sources: CacheSource[] = [];
	for (const tool of options.tools) {
		switch (tool) {
			case 'claude':
				sources.push(...getClaudeReportSources(options.claudePath));
				break;
			case 'codex':
				sources.push(...getCodexReportSources(options.codexSessionDirs));
				break;
			case 'opencode':
				sources.push(...getOpenCodeReportSources(command));
				break;
			case 'pi':
				sources.push(...getPiReportSources(options.piPath));
				break;
			case 'amp':
				sources.push(...getAmpReportSources(options.ampThreadDirs));
				break;
		}
	}
	return sources;
}

if (import.meta.vitest != null) {
	const { describe, expect, it } = import.meta.vitest;

	describe('withReportCache', () => {
		it('reuses cached payload when source fingerprint and pricing state match', async () => {
			const { createFixture } = await import('fs-fixture');

			await using fixture = await createFixture({
				cache: {},
				source: {
					'usage.jsonl': '{}',
				},
			});

			vi.stubEnv('XDG_CACHE_HOME', fixture.getPath('cache'));

			let loadCount = 0;
			const sources = [
				{
					id: 'test',
					dir: fixture.getPath('source'),
					patterns: ['**/*.jsonl'],
				},
			];

			const first = await withReportCache({
				command: 'daily',
				parameters: { tool: 'claude' },
				sources,
				pricing: {
					requiresPricing: false,
				},
				load: async () => {
					loadCount += 1;
					return { value: loadCount };
				},
			});

			const second = await withReportCache({
				command: 'daily',
				parameters: { tool: 'claude' },
				sources,
				pricing: {
					requiresPricing: false,
				},
				load: async () => {
					loadCount += 1;
					return { value: loadCount };
				},
			});

			expect(first).toEqual({ value: 1 });
			expect(second).toEqual({ value: 1 });
			expect(loadCount).toBe(1);
		});

		it('invalidates cache when source files change', async () => {
			const { createFixture } = await import('fs-fixture');
			const { utimes, writeFile } = await import('node:fs/promises');

			await using fixture = await createFixture({
				cache: {},
				source: {
					'usage.jsonl': '{}',
				},
			});

			vi.stubEnv('XDG_CACHE_HOME', fixture.getPath('cache'));

			let loadCount = 0;
			const filePath = fixture.getPath('source/usage.jsonl');
			const sources = [
				{
					id: 'test',
					dir: fixture.getPath('source'),
					patterns: ['**/*.jsonl'],
				},
			];

			await withReportCache({
				command: 'daily',
				parameters: { tool: 'claude' },
				sources,
				pricing: { requiresPricing: false },
				load: async () => ({ value: ++loadCount }),
			});

			await writeFile(fixture.getPath('source/another.jsonl'), '{}');
			await utimes(filePath, new Date('2026-01-01T00:00:00Z'), new Date('2026-01-01T00:00:00Z'));

			const second = await withReportCache({
				command: 'daily',
				parameters: { tool: 'claude' },
				sources,
				pricing: { requiresPricing: false },
				load: async () => ({ value: ++loadCount }),
			});

			expect(second).toEqual({ value: 2 });
		});
	});
}
