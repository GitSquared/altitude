import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

// Altitude reads what a harness knows from its structured output, never by reading a
// sentence it wrote. Pi's core has no notion of a subscription window — it passes the
// provider's refusal through as prose — so this asks the provider for the account's
// standing and leaves it in the session as an entry, which arrives on the other side
// as data.
//
// Two rules, because this runs inside every task: it never signs in, refreshes, or
// retries anything on the operator's behalf, and it never lets a failure of its own
// become a failure of the task. A privacy note worth keeping: the request carries the
// operator's own credential to the provider they are already using, and nothing
// leaves the sandbox.

const ANTHROPIC_USAGE_URL = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_BETA = "claude-code-20250219,oauth-2025-04-20";
const REQUEST_TIMEOUT_MS = 5_000;

// The custom type this extension's entries carry, so the reader can pick them out of
// a session's entries without knowing anything else about it.
const ENTRY_TYPE = "altitude.usage";

interface WindowReading {
	window: "session" | "weekly";
	used_percent?: number;
	clears_at_ms?: number;
}

interface Entry {
	provider: string;
	observed_at_ms: number;
	windows: WindowReading[];
}

// A reset time arrives as unix seconds, unix milliseconds, or an ISO timestamp
// depending on the endpoint, and the record Altitude keeps is milliseconds.
function toEpochMs(value: unknown): number | undefined {
	if (typeof value === "number" && Number.isFinite(value) && value > 0) {
		return value > 1_000_000_000_000 ? Math.round(value) : Math.round(value * 1_000);
	}
	if (typeof value === "string" && value.trim() !== "") {
		const parsed = Date.parse(value);
		if (Number.isFinite(parsed)) return parsed;
	}
	return undefined;
}

function usedPercent(value: unknown): number | undefined {
	return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function windowOf(report: unknown, name: "session" | "weekly"): WindowReading | undefined {
	if (typeof report !== "object" || report === null) return undefined;
	const record = report as Record<string, unknown>;
	const percent = usedPercent(record.utilization);
	const clears = toEpochMs(record.resets_at);
	return percent === undefined && clears === undefined
		? undefined
		: { window: name, used_percent: percent, clears_at_ms: clears };
}

async function anthropicWindows(ctx: ExtensionContext): Promise<WindowReading[]> {
	const resolved = await ctx.modelRegistry.getProviderAuth("anthropic");
	const token = resolved?.auth?.apiKey?.trim();
	if (!token) return [];

	const response = await fetch(ANTHROPIC_USAGE_URL, {
		headers: {
			Authorization: `Bearer ${token}`,
			"anthropic-beta": ANTHROPIC_BETA,
			"anthropic-version": "2023-06-01",
			Accept: "application/json",
		},
		signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
	});
	if (!response.ok) return [];

	const body = (await response.json()) as Record<string, unknown>;
	return [
		windowOf(body.five_hour, "session"),
		windowOf(body.seven_day, "weekly"),
	].filter((reading): reading is WindowReading => reading !== undefined);
}

async function standing(ctx: ExtensionContext): Promise<Entry | undefined> {
	const model = ctx.model;
	if (!model) return undefined;
	// Only a subscription has a window to report, and pi is the one who knows which
	// kind of credential this is: an API key is metered by money and has no plan.
	if (model.provider !== "anthropic" || !ctx.modelRegistry.isUsingOAuth(model)) {
		return undefined;
	}
	const windows = await anthropicWindows(ctx);
	if (windows.length === 0) return undefined;
	return { provider: model.provider, observed_at_ms: Date.now(), windows };
}

export default function (pi: ExtensionAPI) {
	// The lookup starts as the session does, but an entry appended during
	// `session_start` is written before anything is listening to the session, so it is
	// handed over on the first event that is: the turn's start, or the settle that
	// ends a turn the provider refused.
	let lookup: Promise<Entry | undefined> | undefined;
	let handedOver = false;

	const handOver = async () => {
		if (handedOver || !lookup) return;
		handedOver = true;
		const entry = await lookup;
		if (entry) pi.appendEntry(ENTRY_TYPE, entry);
	};

	pi.on("session_start", (_event, ctx) => {
		// A turn that will not run because a usage lookup failed would be worse than a
		// turn that runs without knowing the account's standing.
		lookup = standing(ctx).catch(() => undefined);
	});
	pi.on("turn_start", handOver);
	pi.on("agent_settled", handOver);
}
