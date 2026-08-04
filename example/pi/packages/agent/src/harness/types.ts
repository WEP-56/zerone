import type { ImageContent, TextContent, Usage } from "@earendil-works/pi-ai";
import type { Static, TSchema } from "typebox";
import type { AgentMessage, AgentTool, AgentToolResult, AgentToolUpdateCallback } from "../types.ts";

/** Expected adapter failures are values; programming failures may still throw. */
export type Result<TValue, TError> = { ok: true; value: TValue } | { ok: false; error: TError };

export function ok<TValue, TError>(value: TValue): Result<TValue, TError> {
	return { ok: true, value };
}

export function err<TValue, TError>(error: TError): Result<TValue, TError> {
	return { ok: false, error };
}

export function getOrThrow<TValue, TError>(result: Result<TValue, TError>): TValue {
	if (!result.ok) throw result.error;
	return result.value;
}

export function toError(error: unknown): Error {
	if (error instanceof Error) return error;
	if (typeof error === "string") return new Error(error);
	try {
		return new Error(JSON.stringify(error));
	} catch {
		return new Error(String(error));
	}
}

/** Tool definition whose environment is supplied by the harness, not the Agent Loop. */
export type AgentHarnessTool<
	TContext extends object | undefined,
	TParameters extends TSchema = TSchema,
	TDetails = unknown,
> = Omit<AgentTool<TParameters, TDetails>, "execute"> & {
	execute(
		toolCallId: string,
		params: Static<TParameters>,
		signal: AbortSignal | undefined,
		onUpdate: AgentToolUpdateCallback<TDetails> | undefined,
		context: TContext,
	): Promise<AgentToolResult<TDetails>>;
};

export type FileKind = "file" | "directory" | "symlink";
export type FileErrorCode =
	| "aborted"
	| "not_found"
	| "permission_denied"
	| "not_directory"
	| "is_directory"
	| "invalid"
	| "not_supported"
	| "unknown";

export class FileError extends Error {
	public code: FileErrorCode;
	public path?: string;

	constructor(code: FileErrorCode, message: string, path?: string, cause?: Error) {
		super(message, cause === undefined ? undefined : { cause });
		this.name = "FileError";
		this.code = code;
		this.path = path;
	}
}

export type ExecutionErrorCode =
	| "aborted"
	| "timeout"
	| "shell_unavailable"
	| "spawn_error"
	| "callback_error"
	| "unknown";

export class ExecutionError extends Error {
	public code: ExecutionErrorCode;

	constructor(code: ExecutionErrorCode, message: string, cause?: Error) {
		super(message, cause === undefined ? undefined : { cause });
		this.name = "ExecutionError";
		this.code = code;
	}
}

export type SessionErrorCode =
	| "not_found"
	| "invalid_session"
	| "invalid_entry"
	| "invalid_fork_target"
	| "storage"
	| "unknown";

export class SessionError extends Error {
	public code: SessionErrorCode;

	constructor(code: SessionErrorCode, message: string, cause?: Error) {
		super(message, cause === undefined ? undefined : { cause });
		this.name = "SessionError";
		this.code = code;
	}
}

export interface FileInfo {
	name: string;
	path: string;
	kind: FileKind;
	size: number;
	mtimeMs: number;
}

export interface FileSystem {
	cwd: string;
	absolutePath(path: string, abortSignal?: AbortSignal): Promise<Result<string, FileError>>;
	joinPath(parts: string[], abortSignal?: AbortSignal): Promise<Result<string, FileError>>;
	readTextFile(path: string, abortSignal?: AbortSignal): Promise<Result<string, FileError>>;
	readTextLines(
		path: string,
		options?: { maxLines?: number; abortSignal?: AbortSignal },
	): Promise<Result<string[], FileError>>;
	readBinaryFile(path: string, abortSignal?: AbortSignal): Promise<Result<Uint8Array, FileError>>;
	writeFile(path: string, content: string | Uint8Array, abortSignal?: AbortSignal): Promise<Result<void, FileError>>;
	appendFile(path: string, content: string | Uint8Array, abortSignal?: AbortSignal): Promise<Result<void, FileError>>;
	fileInfo(path: string, abortSignal?: AbortSignal): Promise<Result<FileInfo, FileError>>;
	listDir(path: string, abortSignal?: AbortSignal): Promise<Result<FileInfo[], FileError>>;
	canonicalPath(path: string, abortSignal?: AbortSignal): Promise<Result<string, FileError>>;
	exists(path: string, abortSignal?: AbortSignal): Promise<Result<boolean, FileError>>;
	createDir(
		path: string,
		options?: { recursive?: boolean; abortSignal?: AbortSignal },
	): Promise<Result<void, FileError>>;
	remove(
		path: string,
		options?: { recursive?: boolean; force?: boolean; abortSignal?: AbortSignal },
	): Promise<Result<void, FileError>>;
	createTempDir(prefix?: string, abortSignal?: AbortSignal): Promise<Result<string, FileError>>;
	createTempFile(options?: {
		prefix?: string;
		suffix?: string;
		abortSignal?: AbortSignal;
	}): Promise<Result<string, FileError>>;
	cleanup(): Promise<void>;
}

export interface ShellExecOptions {
	cwd?: string;
	env?: Record<string, string>;
	inheritEnv?: boolean;
	timeout?: number;
	abortSignal?: AbortSignal;
	onStdout?: (chunk: string) => void;
	onStderr?: (chunk: string) => void;
}

export interface Shell {
	exec(
		command: string,
		options?: ShellExecOptions,
	): Promise<Result<{ stdout: string; stderr: string; exitCode: number }, ExecutionError>>;
	cleanup(): Promise<void>;
}

/** Workspace capabilities injected into tools. */
export interface ExecutionEnv extends FileSystem, Shell {}

export interface SessionTreeEntryBase {
	type: string;
	id: string;
	parentId: string | null;
	timestamp: string;
}

export interface MessageEntry extends SessionTreeEntryBase {
	type: "message";
	message: AgentMessage;
}

export interface ThinkingLevelChangeEntry extends SessionTreeEntryBase {
	type: "thinking_level_change";
	thinkingLevel: string;
}

export interface ModelChangeEntry extends SessionTreeEntryBase {
	type: "model_change";
	provider: string;
	modelId: string;
}

export interface ActiveToolsChangeEntry extends SessionTreeEntryBase {
	type: "active_tools_change";
	activeToolNames: string[];
}

export interface CompactionEntry<T = unknown> extends SessionTreeEntryBase {
	type: "compaction";
	summary: string;
	firstKeptEntryId?: string;
	tokensBefore: number;
	retainedTail?: AgentMessage[];
	details?: T;
	usage?: Usage;
	fromHook?: boolean;
}

export interface BranchSummaryEntry<T = unknown> extends SessionTreeEntryBase {
	type: "branch_summary";
	fromId: string;
	summary: string;
	details?: T;
	usage?: Usage;
	fromHook?: boolean;
}

export interface CustomEntry<T = unknown> extends SessionTreeEntryBase {
	type: "custom";
	customType: string;
	data?: T;
}

export interface CustomMessageEntry<T = unknown> extends SessionTreeEntryBase {
	type: "custom_message";
	customType: string;
	content: string | (TextContent | ImageContent)[];
	details?: T;
	display: boolean;
}

export interface LabelEntry extends SessionTreeEntryBase {
	type: "label";
	targetId: string;
	label: string | undefined;
}

export interface SessionInfoEntry extends SessionTreeEntryBase {
	type: "session_info";
	name?: string;
}

export interface LeafEntry extends SessionTreeEntryBase {
	type: "leaf";
	targetId: string | null;
}

export type SessionTreeEntry =
	| MessageEntry
	| ThinkingLevelChangeEntry
	| ModelChangeEntry
	| ActiveToolsChangeEntry
	| CompactionEntry
	| BranchSummaryEntry
	| CustomEntry
	| CustomMessageEntry
	| LabelEntry
	| SessionInfoEntry
	| LeafEntry;

export interface SessionContext {
	messages: AgentMessage[];
	thinkingLevel: string;
	model: { provider: string; modelId: string } | null;
	activeToolNames: string[] | null;
}

export interface SessionStats {
	messageCount: number;
	cachedTokens: number;
	uncachedTokens: number;
	totalTokens: number;
	costTotal: number;
}

export interface SessionMetadata {
	id: string;
	createdAt: string;
}

export interface JsonlSessionMetadata extends SessionMetadata {
	cwd: string;
	path: string;
	parentSessionPath?: string;
	metadata?: Record<string, unknown>;
}

export interface SessionEntryCursorOptions {
	afterEntrySeq?: number;
	limit?: number;
}

export interface SessionCreateOptions {
	id?: string;
}

export interface SessionForkOptions {
	entryId?: string;
	position?: "before" | "at";
	id?: string;
}

export type SessionForkSelection =
	| { kind: "all" }
	| { kind: "before_user_message"; entryId: string }
	| { kind: "through_entry"; entryId: string };

export interface SessionBranchQuery {
	start?: string | null;
	stopAtType?: SessionTreeEntry["type"];
	stopAtId?: string;
	type?: SessionTreeEntry["type"];
	customType?: string;
	order?: "newestFirst" | "oldestFirst";
	limit?: number;
}

export interface SessionHead {
	leafId: string | null;
}

/** Storage owns bytes; Session owns conversation-tree semantics. */
export interface SessionStorage<TMetadata extends SessionMetadata = SessionMetadata> {
	readonly metadata: TMetadata;
	readHead(): Promise<SessionHead>;
	readEntry(id: string): Promise<SessionTreeEntry | undefined>;
	readEntries(options?: SessionEntryCursorOptions): Promise<readonly SessionTreeEntry[]>;
	appendEntry(entry: SessionTreeEntry): Promise<void>;
	findEntriesOnBranch(query: SessionBranchQuery & { start: string | null }): Promise<readonly SessionTreeEntry[]>;
	readPathToRootOrCompaction(leafId: string | null): Promise<readonly SessionTreeEntry[]>;
	getLabel(id: string): Promise<string | undefined>;
	getName(): Promise<string | undefined>;
	getStats(): Promise<SessionStats>;
}

export interface JsonlSessionCreateOptions extends SessionCreateOptions {
	cwd: string;
	parentSessionPath?: string;
	metadata?: Record<string, unknown>;
}

export interface JsonlSessionListOptions {
	cwd?: string;
}
