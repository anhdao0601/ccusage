export async function mapWithConcurrency<T, R>(
	items: readonly T[],
	limit: number,
	mapper: (item: T, index: number) => Promise<R>,
): Promise<R[]> {
	if (items.length === 0) {
		return [];
	}

	const normalizedLimit = Math.max(1, Math.floor(limit));
	const results = Array.from<R | undefined>({ length: items.length });
	let nextIndex = 0;

	const worker = async (): Promise<void> => {
		while (true) {
			const currentIndex = nextIndex;
			nextIndex += 1;

			if (currentIndex >= items.length) {
				return;
			}

			results[currentIndex] = await mapper(items[currentIndex]!, currentIndex);
		}
	};

	await Promise.all(
		Array.from({ length: Math.min(normalizedLimit, items.length) }, async () => worker()),
	);

	return results.map((result) => result!);
}

if (import.meta.vitest != null) {
	const { describe, expect, it } = import.meta.vitest;

	describe('mapWithConcurrency', () => {
		it('preserves input order', async () => {
			const results = await mapWithConcurrency([3, 1, 2], 2, async (value) => value * 2);
			expect(results).toEqual([6, 2, 4]);
		});

		it('runs with a minimum concurrency of one', async () => {
			const results = await mapWithConcurrency([1, 2], 0, async (value) => value + 1);
			expect(results).toEqual([2, 3]);
		});
	});
}
