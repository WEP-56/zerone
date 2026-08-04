import { lazyStream } from "./api/lazy.ts";
import type {
	Api,
	ApiStreamOptions,
	AssistantMessageEventStream,
	Context,
	Model,
	ModelCostRates,
	ModelThinkingLevel,
	ProviderStreams,
	SimpleStreamOptions,
	Usage,
} from "./types.ts";

/**
 * Curated from Pi's provider layer. Authentication and catalog refresh are
 * intentionally omitted so the API dispatch boundary stays visible.
 */
export interface Provider<TApi extends Api = Api> {
	readonly id: string;
	readonly name: string;
	readonly baseUrl?: string;
	getModels(): readonly Model<TApi>[];
	stream<T extends TApi>(
		model: Model<T>,
		context: Context,
		options?: ApiStreamOptions<T>,
	): AssistantMessageEventStream;
	streamSimple(model: Model<TApi>, context: Context, options?: SimpleStreamOptions): AssistantMessageEventStream;
}

export interface CreateProviderOptions<TApi extends Api = Api> {
	id: string;
	name?: string;
	baseUrl?: string;
	models: readonly Model<TApi>[];
	/** One implementation for all models, or a map selected by model.api. */
	api: ProviderStreams | Partial<Record<TApi, ProviderStreams>>;
}

/** The Agent Loop sees one StreamFn; API-specific switching stays here. */
export function createProvider<TApi extends Api = Api>(input: CreateProviderOptions<TApi>): Provider<TApi> {
	const single =
		typeof (input.api as ProviderStreams).stream === "function" ? (input.api as ProviderStreams) : undefined;
	const byApi = single ? undefined : (input.api as Partial<Record<string, ProviderStreams>>);
	const apiFor = (model: Model<Api>): ProviderStreams | undefined => single ?? byApi?.[model.api];
	const dispatch = (
		model: Model<Api>,
		run: (streams: ProviderStreams) => AssistantMessageEventStream,
	): AssistantMessageEventStream => {
		const streams = apiFor(model);
		if (!streams) {
			return lazyStream(model, async () => {
				throw new Error(`Provider ${input.id} has no API implementation for "${model.api}"`);
			});
		}
		return run(streams);
	};

	return {
		id: input.id,
		name: input.name ?? input.id,
		baseUrl: input.baseUrl,
		getModels: () => input.models,
		stream: (model, context, options) =>
			dispatch(model, (streams) => streams.stream(model, context, options)),
		streamSimple: (model, context, options) =>
			dispatch(model, (streams) => streams.streamSimple(model, context, options)),
	};
}

export function hasApi<TApi extends Api>(model: Model<Api>, api: TApi): model is Model<TApi> {
	return model.api === api;
}

export function calculateCost<TApi extends Api>(model: Model<TApi>, usage: Usage): Usage["cost"] {
	const inputTokens = usage.input + usage.cacheRead + usage.cacheWrite;
	let rates: ModelCostRates = model.cost;
	let matchedThreshold = -1;
	for (const tier of model.cost.tiers ?? []) {
		if (inputTokens > tier.inputTokensAbove && tier.inputTokensAbove > matchedThreshold) {
			rates = tier;
			matchedThreshold = tier.inputTokensAbove;
		}
	}

	const longWrite = usage.cacheWrite1h ?? 0;
	const shortWrite = usage.cacheWrite - longWrite;
	usage.cost.input = (rates.input / 1_000_000) * usage.input;
	usage.cost.output = (rates.output / 1_000_000) * usage.output;
	usage.cost.cacheRead = (rates.cacheRead / 1_000_000) * usage.cacheRead;
	usage.cost.cacheWrite = (rates.cacheWrite * shortWrite + rates.input * 2 * longWrite) / 1_000_000;
	usage.cost.total = usage.cost.input + usage.cost.output + usage.cost.cacheRead + usage.cost.cacheWrite;
	return usage.cost;
}

const THINKING_LEVELS: ModelThinkingLevel[] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

export function getSupportedThinkingLevels<TApi extends Api>(model: Model<TApi>): ModelThinkingLevel[] {
	if (!model.reasoning) return ["off"];
	return THINKING_LEVELS.filter((level) => {
		const mapped = model.thinkingLevelMap?.[level];
		if (mapped === null) return false;
		if (level === "xhigh" || level === "max") return mapped !== undefined;
		return true;
	});
}

export function clampThinkingLevel<TApi extends Api>(
	model: Model<TApi>,
	level: ModelThinkingLevel,
): ModelThinkingLevel {
	const available = getSupportedThinkingLevels(model);
	if (available.includes(level)) return level;
	const requested = THINKING_LEVELS.indexOf(level);
	for (let i = requested; i < THINKING_LEVELS.length; i++) {
		const candidate = THINKING_LEVELS[i];
		if (available.includes(candidate)) return candidate;
	}
	for (let i = requested - 1; i >= 0; i--) {
		const candidate = THINKING_LEVELS[i];
		if (available.includes(candidate)) return candidate;
	}
	return available[0] ?? "off";
}
