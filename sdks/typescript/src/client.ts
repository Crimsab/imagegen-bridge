import { BridgeAPIError, BridgeProtocolError, BridgeTransportError, record } from "./errors.js";
import { SseDecoder } from "./sse.js";
import type {
  EditImageRequest,
  GenerateImageRequest,
  ImageJob,
  ImageJobPage,
  ImageJobUpdate,
  ImageRequest,
  ImageResponse,
  JobListOptions,
  JsonValue,
  OperatorDiagnostics,
  ProviderCapabilities,
  ProviderPage,
  RequestOptions,
  SessionMetadata,
  StreamEvent,
} from "./types.js";

export interface ImagegenBridgeClientOptions {
  baseUrl: string;
  bearerToken?: string;
  timeoutMs?: number;
  maxSseEventBytes?: number;
  headers?: Readonly<Record<string, string>>;
  fetch?: typeof globalThis.fetch;
}

interface OpenRequest {
  response: Response;
  cleanup: () => void;
}

const MAX_PARTIAL_PREVIEW_BYTES = 16 * 1024 * 1024;
const IMAGE_CONTENT_TYPES = new Set(["image/png", "image/jpeg", "image/webp"]);

export class ImagesResource {
  readonly #client: ImagegenBridgeClient;

  constructor(client: ImagegenBridgeClient) {
    this.#client = client;
  }

  generate(request: GenerateImageRequest, options: RequestOptions = {}): Promise<ImageResponse> {
    return this.#client.executeImage(request, options);
  }

  edit(request: EditImageRequest, options: RequestOptions = {}): Promise<ImageResponse> {
    return this.#client.executeImage(request, options);
  }

  stream(request: ImageRequest, options: RequestOptions = {}): AsyncIterable<StreamEvent> {
    return this.#client.streamImage(request, options);
  }
}

export class JobsResource {
  readonly #client: ImagegenBridgeClient;

  constructor(client: ImagegenBridgeClient) {
    this.#client = client;
  }

  create(request: ImageRequest, options: RequestOptions = {}): Promise<ImageJob> {
    return this.#client.createJob(request, options);
  }

  get(id: string, options: RequestOptions = {}): Promise<ImageJob> {
    return this.#client.getJob(id, options);
  }

  list(options: JobListOptions = {}): Promise<ImageJobPage> {
    return this.#client.listJobs(options);
  }

  cancel(id: string, options: RequestOptions = {}): Promise<ImageJob> {
    return this.#client.cancelJob(id, options);
  }

  partial(id: string, options: RequestOptions = {}): Promise<Uint8Array> {
    return this.#client.jobPartial(id, options);
  }

  update(id: string, update: ImageJobUpdate, options: RequestOptions = {}): Promise<ImageJob> {
    return this.#client.updateJob(id, update, options);
  }
}

export class ImagegenBridgeClient {
  readonly images: ImagesResource;
  readonly jobs: JobsResource;
  readonly #baseUrl: URL;
  readonly #fetch: typeof globalThis.fetch;
  readonly #headers: Record<string, string>;
  readonly #timeoutMs: number;
  readonly #maxSseEventBytes: number;

  constructor(options: ImagegenBridgeClientOptions) {
    this.#baseUrl = new URL(
      options.baseUrl.endsWith("/") ? options.baseUrl : `${options.baseUrl}/`,
    );
    this.#fetch = options.fetch ?? globalThis.fetch;
    if (!this.#fetch) throw new TypeError("a Fetch API implementation is required");
    this.#timeoutMs = positiveInteger(options.timeoutMs ?? 60_000, "timeoutMs");
    this.#maxSseEventBytes = positiveInteger(
      options.maxSseEventBytes ?? 4 * 1024 * 1024,
      "maxSseEventBytes",
    );
    this.#headers = {
      accept: "application/json",
      "user-agent": "imagegen-bridge-typescript/0.1.0",
      ...options.headers,
      ...(options.bearerToken ? { authorization: `Bearer ${options.bearerToken}` } : {}),
    };
    this.images = new ImagesResource(this);
    this.jobs = new JobsResource(this);
  }

  async executeImage(request: ImageRequest, options: RequestOptions): Promise<ImageResponse> {
    const idempotencyKey = options.idempotencyKey ?? request.idempotency_key ?? undefined;
    const value = await this.#json("POST", "v1/images", {
      body: request,
      options,
      ...(idempotencyKey ? { idempotencyKey } : {}),
    });
    return imageResponse(value);
  }

  async *streamImage(request: ImageRequest, options: RequestOptions): AsyncIterable<StreamEvent> {
    const idempotencyKey = options.idempotencyKey ?? request.idempotency_key ?? undefined;
    const opened = await this.#open("POST", "v1/images/stream", {
      body: request,
      options,
      ...(idempotencyKey ? { idempotencyKey } : {}),
      accept: "text/event-stream",
    });
    try {
      await throwForStatus(opened.response);
      if (!opened.response.body) throw new BridgeProtocolError("bridge returned an empty SSE body");
      const reader = opened.response.body.getReader();
      const decoder = new SseDecoder(this.#maxSseEventBytes);
      try {
        while (true) {
          const item = await reader.read();
          if (item.done) break;
          for (const event of decoder.push(item.value)) yield event;
        }
        for (const event of decoder.finish()) yield event;
      } finally {
        reader.releaseLock();
      }
    } catch (error) {
      if (error instanceof BridgeAPIError || error instanceof BridgeProtocolError) throw error;
      throw new BridgeTransportError("bridge streaming request failed", { cause: error });
    } finally {
      opened.cleanup();
    }
  }

  async createJob(request: ImageRequest, options: RequestOptions): Promise<ImageJob> {
    return imageJob(await this.#json("POST", "v1/jobs", { body: request, options }));
  }

  async getJob(id: string, options: RequestOptions): Promise<ImageJob> {
    return imageJob(await this.#json("GET", `v1/jobs/${encodeURIComponent(id)}`, { options }));
  }

  async listJobs(options: JobListOptions): Promise<ImageJobPage> {
    if (options.includeDeleted && options.visibility)
      throw new TypeError("includeDeleted cannot be combined with visibility");
    const value = await this.#json("GET", "v1/jobs", {
      query: {
        limit: options.limit ?? 20,
        cursor: options.cursor,
        status: options.status,
        visibility: options.visibility,
        favorite: options.favorite === undefined ? undefined : String(options.favorite),
        search: options.search,
        include_deleted: options.includeDeleted ? "true" : undefined,
      },
      options,
    });
    const page = record(value);
    if (!page || !Array.isArray(page.items))
      throw new BridgeProtocolError("bridge returned an invalid durable job page");
    return value as ImageJobPage;
  }

  async cancelJob(id: string, options: RequestOptions): Promise<ImageJob> {
    return imageJob(await this.#json("DELETE", `v1/jobs/${encodeURIComponent(id)}`, { options }));
  }

  async jobPartial(id: string, options: RequestOptions): Promise<Uint8Array> {
    const opened = await this.#open("GET", `v1/jobs/${encodeURIComponent(id)}/partial`, {
      options,
      accept: "image/png, image/jpeg, image/webp",
    });
    try {
      await throwForStatus(opened.response);
      const contentType = (opened.response.headers.get("content-type") ?? "")
        .split(";", 1)[0]
        ?.trim()
        .toLowerCase();
      if (!contentType || !IMAGE_CONTENT_TYPES.has(contentType))
        throw new BridgeProtocolError("bridge returned an invalid partial image preview");
      const bytes = new Uint8Array(await opened.response.arrayBuffer());
      if (bytes.byteLength === 0)
        throw new BridgeProtocolError("bridge returned an empty partial image preview");
      if (bytes.byteLength > MAX_PARTIAL_PREVIEW_BYTES)
        throw new BridgeProtocolError("bridge partial image preview exceeds the SDK limit");
      return bytes;
    } catch (error) {
      if (error instanceof BridgeAPIError || error instanceof BridgeProtocolError) throw error;
      throw new BridgeTransportError("bridge request failed", { cause: error });
    } finally {
      opened.cleanup();
    }
  }

  async updateJob(id: string, update: ImageJobUpdate, options: RequestOptions): Promise<ImageJob> {
    if (update.favorite === undefined && update.deleted === undefined)
      throw new TypeError("job update requires favorite or deleted");
    return imageJob(
      await this.#json("PATCH", `v1/jobs/${encodeURIComponent(id)}`, {
        body: update as unknown as JsonValue,
        options,
      }),
    );
  }

  async providers(
    options: { limit?: number; cursor?: string; signal?: AbortSignal } = {},
  ): Promise<ProviderPage> {
    const value = await this.#json("GET", "v1/providers", {
      query: { limit: options.limit ?? 20, cursor: options.cursor },
      options: signalOptions(options.signal),
    });
    const page = record(value);
    if (!page || !Array.isArray(page.items)) throw new BridgeProtocolError("invalid provider page");
    return value as ProviderPage;
  }

  async capabilities(
    provider: string,
    options: { model?: string; signal?: AbortSignal } = {},
  ): Promise<ProviderCapabilities> {
    const value = await this.#json(
      "GET",
      `v1/providers/${encodeURIComponent(provider)}/capabilities`,
      {
        query: { model: options.model },
        options: signalOptions(options.signal),
      },
    );
    const capabilities = record(value);
    if (!capabilities || capabilities.provider !== provider) {
      throw new BridgeProtocolError("invalid provider capabilities");
    }
    return value as ProviderCapabilities;
  }

  async diagnostics(options: { signal?: AbortSignal } = {}): Promise<OperatorDiagnostics> {
    const value = await this.#json("GET", "v1/diagnostics", {
      options: signalOptions(options.signal),
    });
    const diagnostics = record(value);
    if (
      !diagnostics ||
      typeof diagnostics.bridge_version !== "string" ||
      !record(diagnostics.configuration) ||
      !record(diagnostics.runtime) ||
      !Array.isArray(diagnostics.providers)
    ) {
      throw new BridgeProtocolError("invalid operator diagnostics");
    }
    return value as OperatorDiagnostics;
  }

  async session(
    key: string,
    options: { provider?: string; signal?: AbortSignal } = {},
  ): Promise<SessionMetadata> {
    const value = await this.#json("GET", `v1/sessions/${encodeURIComponent(key)}`, {
      query: { provider: options.provider },
      options: signalOptions(options.signal),
    });
    const session = record(value);
    if (!session || typeof session.reused !== "boolean")
      throw new BridgeProtocolError("invalid session metadata");
    return value as SessionMetadata;
  }

  async deleteSession(
    key: string,
    options: { provider?: string; signal?: AbortSignal } = {},
  ): Promise<void> {
    await this.#json("DELETE", `v1/sessions/${encodeURIComponent(key)}`, {
      query: { provider: options.provider },
      options: signalOptions(options.signal),
      allowEmpty: true,
    });
  }

  health(
    options: { ready?: boolean; signal?: AbortSignal } = {},
  ): Promise<Record<string, JsonValue>> {
    return this.#json("GET", options.ready ? "health/ready" : "health/live", {
      options: signalOptions(options.signal),
    }) as Promise<Record<string, JsonValue>>;
  }

  async #json(
    method: string,
    path: string,
    settings: {
      body?: JsonValue | ImageRequest;
      query?: Record<string, string | number | undefined>;
      options?: RequestOptions;
      idempotencyKey?: string;
      allowEmpty?: boolean;
    } = {},
  ): Promise<unknown> {
    const opened = await this.#open(method, path, settings);
    try {
      await throwForStatus(opened.response);
      if (settings.allowEmpty && opened.response.status === 204) return null;
      const value: unknown = await opened.response.json();
      if (!record(value))
        throw new BridgeProtocolError("bridge returned a non-object JSON response");
      return value;
    } catch (error) {
      if (error instanceof BridgeAPIError || error instanceof BridgeProtocolError) throw error;
      throw new BridgeTransportError("bridge request failed", { cause: error });
    } finally {
      opened.cleanup();
    }
  }

  async #open(
    method: string,
    path: string,
    settings: {
      body?: JsonValue | ImageRequest;
      query?: Record<string, string | number | undefined>;
      options?: RequestOptions;
      idempotencyKey?: string;
      accept?: string;
    },
  ): Promise<OpenRequest> {
    const url = new URL(path, this.#baseUrl);
    for (const [key, value] of Object.entries(settings.query ?? {})) {
      if (value !== undefined) url.searchParams.set(key, String(value));
    }
    const timeout = positiveInteger(settings.options?.timeoutMs ?? this.#timeoutMs, "timeoutMs");
    const combined = combinedSignal(settings.options?.signal, timeout);
    try {
      const response = await this.#fetch(url, {
        method,
        headers: {
          ...this.#headers,
          ...(settings.accept ? { accept: settings.accept } : {}),
          ...(settings.body ? { "content-type": "application/json" } : {}),
          ...(settings.idempotencyKey ? { "idempotency-key": settings.idempotencyKey } : {}),
        },
        ...(settings.body ? { body: JSON.stringify(settings.body) } : {}),
        signal: combined.signal,
      });
      return { response, cleanup: combined.cleanup };
    } catch (error) {
      combined.cleanup();
      throw new BridgeTransportError("bridge request failed", { cause: error });
    }
  }
}

async function throwForStatus(response: Response): Promise<void> {
  if (response.ok) return;
  let payload: unknown = null;
  try {
    payload = await response.json();
  } catch {
    /* invalid envelopes are normalized below */
  }
  throw BridgeAPIError.fromPayload(response.status, payload);
}

function imageResponse(value: unknown): ImageResponse {
  const response = record(value);
  if (
    !response ||
    typeof response.id !== "string" ||
    typeof response.provider !== "string" ||
    typeof response.model !== "string" ||
    !Array.isArray(response.data) ||
    !record(response.timings)
  )
    throw new BridgeProtocolError("bridge returned an invalid image response");
  return value as ImageResponse;
}

function imageJob(value: unknown): ImageJob {
  const job = record(value);
  if (
    !job ||
    typeof job.id !== "string" ||
    typeof job.status !== "string" ||
    typeof job.created !== "number" ||
    typeof job.updated !== "number" ||
    typeof job.favorite !== "boolean" ||
    typeof job.cancel_requested !== "boolean" ||
    !record(job.request)
  )
    throw new BridgeProtocolError("bridge returned an invalid durable job");
  return value as ImageJob;
}

function positiveInteger(value: number, name: string): number {
  if (!Number.isSafeInteger(value) || value <= 0)
    throw new RangeError(`${name} must be a positive safe integer`);
  return value;
}

function combinedSignal(
  signal: AbortSignal | undefined,
  timeoutMs: number,
): { signal: AbortSignal; cleanup: () => void } {
  const controller = new AbortController();
  const abort = () => controller.abort(signal?.reason);
  if (signal?.aborted) abort();
  else signal?.addEventListener("abort", abort, { once: true });
  const timeout = setTimeout(
    () => controller.abort(new DOMException("request timed out", "TimeoutError")),
    timeoutMs,
  );
  return {
    signal: controller.signal,
    cleanup: () => {
      clearTimeout(timeout);
      signal?.removeEventListener("abort", abort);
    },
  };
}

function signalOptions(signal: AbortSignal | undefined): RequestOptions {
  return signal ? { signal } : {};
}
