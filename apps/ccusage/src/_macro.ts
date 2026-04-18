import type { LiteLLMModelPricing } from '@ccusage/internal/pricing';
import {
	createPricingDataset,
	fetchLiteLLMPricingDataset,
	filterPricingDataset,
} from '@ccusage/internal/pricing-fetch-utils';

function isClaudeModel(modelName: string, _pricing: LiteLLMModelPricing): boolean {
	const normalized = modelName.toLowerCase();

	return (
		normalized.startsWith('claude-') ||
		normalized.startsWith('anthropic.claude-') ||
		normalized.startsWith('anthropic/claude-') ||
		normalized.startsWith('global.anthropic.claude-') ||
		normalized.startsWith('us.anthropic.claude-') ||
		normalized.startsWith('eu.anthropic.claude-') ||
		normalized.startsWith('au.anthropic.claude-')
	);
}

export async function prefetchClaudePricing(): Promise<Record<string, LiteLLMModelPricing>> {
	try {
		const dataset = await fetchLiteLLMPricingDataset();
		return filterPricingDataset(dataset, isClaudeModel);
	} catch (error) {
		console.warn('Failed to prefetch Claude pricing data, proceeding with empty cache.', error);
		return createPricingDataset();
	}
}
