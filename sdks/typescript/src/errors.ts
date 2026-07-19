import type { JsonValue } from "./types.js";

export class ImagegenBridgeError extends Error {
  override readonly name: string = "ImagegenBridgeError";
}

export interface BridgeAPIErrorOptions {
  statusCode: number;
  type?: string;
  code?: string | null;
  param?: string | null;
  bridgeCode?: string | null;
  retryable?: boolean;
  provider?: string | null;
  upstreamRequestId?: string | null;
  requestId?: string | null;
  details?: Record<string, JsonValue>;
  suggestions?: string[];
}

export class BridgeAPIError extends ImagegenBridgeError {
  override readonly name = "BridgeAPIError";
  readonly statusCode: number;
  readonly type: string;
  readonly code: string | null;
  readonly param: string | null;
  readonly bridgeCode: string | null;
  readonly retryable: boolean;
  readonly provider: string | null;
  readonly upstreamRequestId: string | null;
  readonly requestId: string | null;
  readonly details: Record<string, JsonValue>;
  readonly suggestions: string[];

  constructor(message: string, options: BridgeAPIErrorOptions) {
    super(message);
    this.statusCode = options.statusCode;
    this.type = options.type ?? "api_error";
    this.code = options.code ?? null;
    this.param = options.param ?? null;
    this.bridgeCode = options.bridgeCode ?? null;
    this.retryable = options.retryable ?? false;
    this.provider = options.provider ?? null;
    this.upstreamRequestId = options.upstreamRequestId ?? null;
    this.requestId = options.requestId ?? null;
    this.details = options.details ?? {};
    this.suggestions = options.suggestions ?? [];
  }

  static fromPayload(statusCode: number, payload: unknown): BridgeAPIError {
    const root = record(payload);
    const error = record(root?.error);
    const bridge = record(error?.imagegen_bridge);
    const details = record(bridge?.details) as Record<string, JsonValue> | null;
    const suggestions = Array.isArray(bridge?.suggestions)
      ? bridge.suggestions.filter((value): value is string => typeof value === "string")
      : [];
    return new BridgeAPIError(string(error?.message) ?? "bridge request failed", {
      statusCode,
      type: string(error?.type) ?? "api_error",
      code: string(error?.code),
      param: string(error?.param),
      bridgeCode: string(bridge?.code),
      retryable: bridge?.retryable === true,
      provider: string(bridge?.provider),
      upstreamRequestId: string(bridge?.upstream_request_id),
      requestId: string(root?.request_id),
      suggestions,
      ...(details ? { details } : {}),
    });
  }
}

export class BridgeProtocolError extends ImagegenBridgeError {
  override readonly name = "BridgeProtocolError";
}

export class BridgeTransportError extends ImagegenBridgeError {
  override readonly name = "BridgeTransportError";
}

export function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

export function string(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}
