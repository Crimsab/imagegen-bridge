import { BridgeAPIError, BridgeProtocolError, BridgeTransportError, record } from "./errors.js";
import { SseDecoder } from "./sse.js";
import type {
  EditImageRequest,
  GenerateImageRequest,
  ImageRequest,
  ImageResponse,
  JsonValue,
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

export class ImagegenBridgeClient {
  readonly images: ImagesResource;
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
