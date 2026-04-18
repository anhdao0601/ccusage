import type { LiteLLMModelPricing } from '@ccusage/internal/pricing';
import { LiteLLMPricingFetcher } from '@ccusage/internal/pricing';
import { Result } from '@praha/byethrow';
import { createFixture } from 'fs-fixture';
import { prefetchClaudePricing } from './_macro.ts' with { type: 'macro' };
import { readPricingCache, writePricingCache } from './_pricing-cache.ts';
import { logger } from './logger.ts';

const CLAUDE_PROVIDER_PREFIXES = [
	'anthropic/',
	'claude-3-5-',
	'claude-3-',
	'claude-',
	'openrouter/openai/',
];

const PREFETCHED_CLAUDE_PRICING = prefetchClaudePricing();

async function loadCachedPricing(): Promise<Record<string, LiteLLMModelPricing>> {
	const cachedPricing = await readPricingCache();
	if (cachedPricing != null) {
		return cachedPricing;
	}
	return PREFETCHED_CLAUDE_PRICING;
}

export class PricingFetcher extends LiteLLMPricingFetcher {
	constructor(offline = false, updatePricing = false) {
		super({
			offline,
			offlineLoader: loadCachedPricing,
			logger,
			providerPrefixes: CLAUDE_PROVIDER_PREFIXES,
			forceRefresh: !offline && updatePricing,
		});
	}
}

if (import.meta.vitest != null) {
	describe('PricingFetcher', () => {
		afterEach(() => {
			LiteLLMPricingFetcher.clearSharedCaches();
			vi.unstubAllEnvs();
			vi.restoreAllMocks();
		});

		it('loads offline pricing when offline flag is true', async () => {
			using fetcher = new PricingFetcher(true);
			const pricing = await Result.unwrap(fetcher.fetchModelPricing());
			expect(pricing.size).toBeGreaterThan(0);
		});

		it('calculates cost for Claude model tokens', async () => {
			using fetcher = new PricingFetcher(true);
			const pricing = await Result.unwrap(fetcher.getModelPricing('claude-sonnet-4-20250514'));
			const cost = fetcher.calculateCostFromPricing(
				{
					input_tokens: 1000,
					output_tokens: 500,
					cache_read_input_tokens: 300,
				},
				pricing!,
			);

			expect(cost).toBeGreaterThan(0);
		});

		it('reuses same-day cached pricing by default', async () => {
			await using fixture = await createFixture({});
			vi.stubEnv('XDG_CACHE_HOME', fixture.path);
			await writePricingCache(
				{
					'anthropic/claude-sonnet-4-20250514': {
						input_cost_per_token: 3e-6,
						output_cost_per_token: 1.5e-5,
					},
				},
				{
					fetchedAt: new Date().toISOString(),
				},
			);

			const fetchSpy = vi
				.spyOn(globalThis, 'fetch')
				.mockResolvedValue(new Response(JSON.stringify({}), { status: 200 }));
			using fetcher = new PricingFetcher(false, false);
			await Result.unwrap(fetcher.fetchModelPricing());
			expect(fetchSpy).not.toHaveBeenCalled();
		});

		it('uses disk cache when available', async () => {
			await using fixture = await createFixture({});
			vi.stubEnv('XDG_CACHE_HOME', fixture.path);
			await writePricingCache(
				{
					'anthropic/claude-sonnet-4-20250514': {
						input_cost_per_token: 3e-6,
						output_cost_per_token: 1.5e-5,
					},
				},
				{
					fetchedAt: new Date().toISOString(),
				},
			);

			const fetchSpy = vi
				.spyOn(globalThis, 'fetch')
				.mockResolvedValue(new Response(JSON.stringify({}), { status: 200 }));

			using fetcher = new PricingFetcher(false, false);
			const pricing = await Result.unwrap(fetcher.getModelPricing('claude-sonnet-4-20250514'));
			expect(pricing?.input_cost_per_token).toBe(3e-6);
			expect(fetchSpy).not.toHaveBeenCalled();
		});

		it('refreshes pricing and persists cache when updatePricing is true', async () => {
			await using fixture = await createFixture({});
			vi.stubEnv('XDG_CACHE_HOME', fixture.path);

			const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
				new Response(
					JSON.stringify({
						'anthropic/claude-sonnet-4-20250514': {
							input_cost_per_token: 9e-6,
							output_cost_per_token: 1.8e-5,
						},
					}),
					{ status: 200 },
				),
			);

			using fetcher = new PricingFetcher(false, true);
			await Result.unwrap(fetcher.fetchModelPricing());

			expect(fetchSpy).toHaveBeenCalledTimes(1);
			const cached = await readPricingCache();
			expect(cached?.['anthropic/claude-sonnet-4-20250514']?.input_cost_per_token).toBe(9e-6);
		});

		it('falls back to cached pricing when refresh fails', async () => {
			await using fixture = await createFixture({});
			vi.stubEnv('XDG_CACHE_HOME', fixture.path);
			await writePricingCache(
				{
					'anthropic/claude-sonnet-4-20250514': {
						input_cost_per_token: 4e-6,
						output_cost_per_token: 1.2e-5,
					},
				},
				{
					fetchedAt: new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString(),
				},
			);

			const fetchSpy = vi.spyOn(globalThis, 'fetch').mockRejectedValue(new Error('network down'));

			using fetcher = new PricingFetcher(false, true);
			const pricing = await Result.unwrap(fetcher.getModelPricing('claude-sonnet-4-20250514'));
			expect(fetchSpy).toHaveBeenCalledTimes(1);
			expect(pricing?.input_cost_per_token).toBe(4e-6);
		});

		it('blocks network refresh when offline is true even with updatePricing', async () => {
			const fetchSpy = vi
				.spyOn(globalThis, 'fetch')
				.mockResolvedValue(new Response(JSON.stringify({}), { status: 200 }));

			using fetcher = new PricingFetcher(true, true);
			await Result.unwrap(fetcher.fetchModelPricing());
			expect(fetchSpy).not.toHaveBeenCalled();
		});

		it('refreshes stale pricing once per day on normal online runs', async () => {
			await using fixture = await createFixture({});
			vi.stubEnv('XDG_CACHE_HOME', fixture.path);
			await writePricingCache(
				{
					'anthropic/claude-sonnet-4-20250514': {
						input_cost_per_token: 3e-6,
						output_cost_per_token: 1.5e-5,
					},
				},
				{
					fetchedAt: new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString(),
				},
			);

			const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
				new Response(
					JSON.stringify({
						'anthropic/claude-sonnet-4-20250514': {
							input_cost_per_token: 9e-6,
							output_cost_per_token: 1.8e-5,
						},
					}),
					{ status: 200 },
				),
			);

			using fetcher = new PricingFetcher(false, false);
			await Result.unwrap(fetcher.fetchModelPricing());

			expect(fetchSpy).toHaveBeenCalledTimes(1);
			const cached = await readPricingCache();
			expect(cached?.['anthropic/claude-sonnet-4-20250514']?.input_cost_per_token).toBe(9e-6);
		});
	});
}
