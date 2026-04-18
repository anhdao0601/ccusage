const DEFAULT_TIMEZONE_CACHE_KEY = '__default__';
const timezoneCache = new Map<string, string>();
const dateKeyFormatterCache = new Map<string, Intl.DateTimeFormat>();
const monthKeyFormatterCache = new Map<string, Intl.DateTimeFormat>();
const displayDateFormatterCache = new Map<string, Intl.DateTimeFormat>();
const displayMonthFormatterCache = new Map<string, Intl.DateTimeFormat>();
const displayDateTimeFormatterCache = new Map<string, Intl.DateTimeFormat>();

function safeTimeZone(timezone?: string): string {
	const cacheKey =
		timezone == null || timezone.trim() === '' ? DEFAULT_TIMEZONE_CACHE_KEY : timezone;
	const cached = timezoneCache.get(cacheKey);
	if (cached != null) {
		return cached;
	}

	let resolvedTimezone: string;
	if (cacheKey === DEFAULT_TIMEZONE_CACHE_KEY) {
		resolvedTimezone = Intl.DateTimeFormat().resolvedOptions().timeZone ?? 'UTC';
	} else {
		// Validate timezone by creating a formatter
		try {
			Intl.DateTimeFormat('en-US', { timeZone: timezone });
			resolvedTimezone = timezone!;
		} catch {
			resolvedTimezone = 'UTC';
		}
	}

	timezoneCache.set(cacheKey, resolvedTimezone);
	return resolvedTimezone;
}

function getDateKeyFormatter(timezone?: string): Intl.DateTimeFormat {
	const tz = safeTimeZone(timezone);
	const cached = dateKeyFormatterCache.get(tz);
	if (cached != null) {
		return cached;
	}

	const formatter = new Intl.DateTimeFormat('en-CA', {
		year: 'numeric',
		month: '2-digit',
		day: '2-digit',
		timeZone: tz,
	});
	dateKeyFormatterCache.set(tz, formatter);
	return formatter;
}

function getMonthKeyFormatter(timezone?: string): Intl.DateTimeFormat {
	const tz = safeTimeZone(timezone);
	const cached = monthKeyFormatterCache.get(tz);
	if (cached != null) {
		return cached;
	}

	const formatter = new Intl.DateTimeFormat('en-CA', {
		year: 'numeric',
		month: '2-digit',
		timeZone: tz,
	});
	monthKeyFormatterCache.set(tz, formatter);
	return formatter;
}

function getDisplayDateFormatter(locale?: string): Intl.DateTimeFormat {
	const key = locale ?? 'en-US';
	const cached = displayDateFormatterCache.get(key);
	if (cached != null) {
		return cached;
	}

	const formatter = new Intl.DateTimeFormat(key, {
		year: 'numeric',
		month: 'short',
		day: '2-digit',
		timeZone: 'UTC',
	});
	displayDateFormatterCache.set(key, formatter);
	return formatter;
}

function getDisplayMonthFormatter(locale?: string): Intl.DateTimeFormat {
	const key = locale ?? 'en-US';
	const cached = displayMonthFormatterCache.get(key);
	if (cached != null) {
		return cached;
	}

	const formatter = new Intl.DateTimeFormat(key, {
		year: 'numeric',
		month: 'short',
		timeZone: 'UTC',
	});
	displayMonthFormatterCache.set(key, formatter);
	return formatter;
}

function getDisplayDateTimeFormatter(locale?: string, timezone?: string): Intl.DateTimeFormat {
	const tz = safeTimeZone(timezone);
	const key = `${locale ?? 'en-US'}\x00${tz}`;
	const cached = displayDateTimeFormatterCache.get(key);
	if (cached != null) {
		return cached;
	}

	const formatter = new Intl.DateTimeFormat(locale ?? 'en-US', {
		dateStyle: 'short',
		timeStyle: 'short',
		timeZone: tz,
	});
	displayDateTimeFormatterCache.set(key, formatter);
	return formatter;
}

export function toDateKey(timestamp: string, timezone?: string): string {
	const date = new Date(timestamp);
	const formatter = getDateKeyFormatter(timezone);
	return formatter.format(date);
}

export function normalizeFilterDate(value?: string): string | undefined {
	if (value == null) {
		return undefined;
	}

	const compact = value.replaceAll('-', '').trim();
	if (!/^\d{8}$/.test(compact)) {
		throw new Error(`Invalid date format: ${value}. Expected YYYYMMDD or YYYY-MM-DD.`);
	}

	return `${compact.slice(0, 4)}-${compact.slice(4, 6)}-${compact.slice(6, 8)}`;
}

export function isWithinRange(dateKey: string, since?: string, until?: string): boolean {
	const value = dateKey.replaceAll('-', '');
	const sinceValue = since?.replaceAll('-', '');
	const untilValue = until?.replaceAll('-', '');

	if (sinceValue != null && value < sinceValue) {
		return false;
	}

	if (untilValue != null && value > untilValue) {
		return false;
	}

	return true;
}

export function formatDisplayDate(dateKey: string, locale?: string, _timezone?: string): string {
	// dateKey is already computed for the target timezone via toDateKey().
	// Treat it as a plain calendar date and avoid shifting it by applying a timezone.
	const [yearStr = '0', monthStr = '1', dayStr = '1'] = dateKey.split('-');
	const year = Number.parseInt(yearStr, 10);
	const month = Number.parseInt(monthStr, 10);
	const day = Number.parseInt(dayStr, 10);
	const date = new Date(Date.UTC(year, month - 1, day));
	const formatter = getDisplayDateFormatter(locale);
	return formatter.format(date);
}

export function toMonthKey(timestamp: string, timezone?: string): string {
	const date = new Date(timestamp);
	const formatter = getMonthKeyFormatter(timezone);
	const [year, month] = formatter.format(date).split('-');
	return `${year}-${month}`;
}

export function formatDisplayMonth(monthKey: string, locale?: string, _timezone?: string): string {
	// monthKey is already derived in the target timezone via toMonthKey().
	// Render it as a calendar month without shifting by timezone.
	const [yearStr = '0', monthStr = '1'] = monthKey.split('-');
	const year = Number.parseInt(yearStr, 10);
	const month = Number.parseInt(monthStr, 10);
	const date = new Date(Date.UTC(year, month - 1, 1));
	const formatter = getDisplayMonthFormatter(locale);
	return formatter.format(date);
}

export function formatDisplayDateTime(
	timestamp: string,
	locale?: string,
	timezone?: string,
): string {
	const date = new Date(timestamp);
	const formatter = getDisplayDateTimeFormatter(locale, timezone);
	return formatter.format(date);
}
