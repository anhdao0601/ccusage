import type { TokenUsageEvent } from './_types.ts';
import { mkdir, readFile, rename, unlink, writeFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { createFixture } from 'fs-fixture';

const CACHE_DIRECTORY_NAME = 'ccusage';
const SESSION_INDEX_SUBDIR = 'indexes';
const SESSION_INDEX_FILE_NAME = 'codex-session-index.json';
const SESSION_INDEX_SCHEMA_VERSION = 1;

export type SessionIndexEntry = {
	file: string;
	sessionId: string;
	size: number;
	mtimeMs: number;
	events: TokenUsageEvent[];
};

type SessionIndexEnvelope = {
	schemaVersion: number;
	updatedAt: string;
	entries: SessionIndexEntry[];
};

function getSessionIndexPath(): string {
	const cacheHome = process.env.XDG_CACHE_HOME ?? path.join(homedir(), '.cache');
	return path.join(cacheHome, CACHE_DIRECTORY_NAME, SESSION_INDEX_SUBDIR, SESSION_INDEX_FILE_NAME);
}

function isFiniteNumber(value: unknown): value is number {
	return typeof value === 'number' && Number.isFinite(value);
}

function isTokenUsageEvent(value: unknown): value is TokenUsageEvent {
	if (value == null || typeof value !== 'object') {
		return false;
	}

	const candidate = value as Record<string, unknown>;
	return (
		typeof candidate.timestamp === 'string' &&
		typeof candidate.sessionId === 'string' &&
		isFiniteNumber(candidate.inputTokens) &&
		isFiniteNumber(candidate.cachedInputTokens) &&
		isFiniteNumber(candidate.outputTokens) &&
		isFiniteNumber(candidate.reasoningOutputTokens) &&
		isFiniteNumber(candidate.totalTokens) &&
		(candidate.model == null || typeof candidate.model === 'string') &&
		(candidate.isFallbackModel == null || typeof candidate.isFallbackModel === 'boolean')
	);
}

function isSessionIndexEntry(value: unknown): value is SessionIndexEntry {
	if (value == null || typeof value !== 'object') {
		return false;
	}

	const candidate = value as Record<string, unknown>;
	return (
		typeof candidate.file === 'string' &&
		typeof candidate.sessionId === 'string' &&
		isFiniteNumber(candidate.size) &&
		isFiniteNumber(candidate.mtimeMs) &&
		Array.isArray(candidate.events) &&
		candidate.events.every((event) => isTokenUsageEvent(event))
	);
}

export async function readSessionIndex(): Promise<Map<string, SessionIndexEntry>> {
	try {
		const content = await readFile(getSessionIndexPath(), 'utf8');
		const parsed = JSON.parse(content) as unknown;
		if (parsed == null || typeof parsed !== 'object') {
			return new Map();
		}

		const envelope = parsed as Partial<SessionIndexEnvelope>;
		if (
			envelope.schemaVersion !== SESSION_INDEX_SCHEMA_VERSION ||
			!Array.isArray(envelope.entries)
		) {
			return new Map();
		}

		const entries = envelope.entries.filter((entry): entry is SessionIndexEntry =>
			isSessionIndexEntry(entry),
		);

		return new Map(entries.map((entry) => [entry.file, entry]));
	} catch {
		return new Map();
	}
}

export async function writeSessionIndex(entries: Iterable<SessionIndexEntry>): Promise<void> {
	const indexPath = getSessionIndexPath();
	const tempPath = `${indexPath}.${process.pid}.${Date.now()}.tmp`;
	await mkdir(path.dirname(indexPath), { recursive: true });

	try {
		await writeFile(
			tempPath,
			JSON.stringify(
				{
					schemaVersion: SESSION_INDEX_SCHEMA_VERSION,
					updatedAt: new Date().toISOString(),
					entries: Array.from(entries),
				} satisfies SessionIndexEnvelope,
				null,
				'\t',
			),
		);
		await rename(tempPath, indexPath);
	} finally {
		await unlink(tempPath).catch(() => undefined);
	}
}

export function createSessionIndexEntry(input: {
	file: string;
	sessionId: string;
	size: number;
	mtimeMs: number;
	events: TokenUsageEvent[];
}): SessionIndexEntry {
	return {
		file: input.file,
		sessionId: input.sessionId,
		size: input.size,
		mtimeMs: input.mtimeMs,
		events: input.events,
	};
}

if (import.meta.vitest != null) {
	const { afterEach, describe, expect, it, vi } = import.meta.vitest;

	describe('session index', () => {
		afterEach(() => {
			vi.unstubAllEnvs();
		});

		it('round-trips cached entries', async () => {
			await using fixture = await createFixture({});
			vi.stubEnv('XDG_CACHE_HOME', fixture.path);

			await writeSessionIndex([
				createSessionIndexEntry({
					file: '/tmp/session.jsonl',
					sessionId: 'session',
					size: 123,
					mtimeMs: 456,
					events: [
						{
							timestamp: '2026-01-01T00:00:00.000Z',
							sessionId: 'session',
							model: 'gpt-5',
							inputTokens: 1,
							cachedInputTokens: 2,
							outputTokens: 3,
							reasoningOutputTokens: 4,
							totalTokens: 8,
						},
					],
				}),
			]);

			const index = await readSessionIndex();
			expect(index.get('/tmp/session.jsonl')).toEqual({
				file: '/tmp/session.jsonl',
				sessionId: 'session',
				size: 123,
				mtimeMs: 456,
				events: [
					{
						timestamp: '2026-01-01T00:00:00.000Z',
						sessionId: 'session',
						model: 'gpt-5',
						inputTokens: 1,
						cachedInputTokens: 2,
						outputTokens: 3,
						reasoningOutputTokens: 4,
						totalTokens: 8,
					},
				],
			});
		});

		it('ignores invalid cache contents', async () => {
			await using fixture = await createFixture({
				ccusage: {
					indexes: {
						'codex-session-index.json': JSON.stringify({
							schemaVersion: 999,
							entries: ['invalid'],
						}),
					},
				},
			});
			vi.stubEnv('XDG_CACHE_HOME', fixture.path);

			const index = await readSessionIndex();
			expect(index.size).toBe(0);
		});
	});
}
