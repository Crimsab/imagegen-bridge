import { BridgeAPIError, BridgeProtocolError, record, string } from "./errors.js";
import type { ImageResponse, StreamEvent } from "./types.js";

export class SseDecoder {
  readonly #decoder = new TextDecoder();
  readonly #encoder = new TextEncoder();
  readonly #maximumEventBytes: number;
  #buffer = "";
  #eventName = "message";
  #data: string[] = [];
  #size = 0;

  constructor(maximumEventBytes: number) {
    if (!Number.isSafeInteger(maximumEventBytes) || maximumEventBytes <= 0) {
      throw new RangeError("maximumEventBytes must be a positive safe integer");
    }
    this.#maximumEventBytes = maximumEventBytes;
  }

  push(chunk: Uint8Array): StreamEvent[] {
    this.#buffer += this.#decoder.decode(chunk, { stream: true });
    return this.#drain(false);
  }

  finish(): StreamEvent[] {
    this.#buffer += this.#decoder.decode();
    return this.#drain(true);
  }

  #drain(final: boolean): StreamEvent[] {
    const events: StreamEvent[] = [];
    let newline = this.#buffer.indexOf("\n");
    while (newline >= 0) {
      const raw = this.#buffer.slice(0, newline);
      this.#buffer = this.#buffer.slice(newline + 1);
      const event = this.#line(raw.endsWith("\r") ? raw.slice(0, -1) : raw);
      if (event) events.push(event);
      newline = this.#buffer.indexOf("\n");
    }
    if (final && this.#buffer.length > 0) {
      const event = this.#line(
        this.#buffer.endsWith("\r") ? this.#buffer.slice(0, -1) : this.#buffer,
      );
      if (event) events.push(event);
      this.#buffer = "";
    }
    if (final && this.#data.length > 0) events.push(this.#dispatch());
    return events;
  }

  #line(line: string): StreamEvent | null {
    if (line === "") return this.#data.length > 0 ? this.#dispatch() : null;
    if (line.startsWith(":")) return null;
    this.#size += this.#encoder.encode(line).byteLength;
    if (this.#size > this.#maximumEventBytes) {
      throw new BridgeProtocolError("bridge SSE event exceeded the configured byte limit");
    }
    const separator = line.indexOf(":");
    const field = separator < 0 ? line : line.slice(0, separator);
    let value = separator < 0 ? "" : line.slice(separator + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") this.#eventName = value;
    if (field === "data") this.#data.push(value);
    return null;
  }

  #dispatch(): StreamEvent {
    const eventName = this.#eventName;
    const data = this.#data.join("\n");
    this.#eventName = "message";
    this.#data = [];
    this.#size = 0;
    let payload: unknown;
    try {
      payload = JSON.parse(data);
    } catch (error) {
      throw new BridgeProtocolError("bridge returned invalid JSON in an SSE event", {
        cause: error,
      });
    }
    if (eventName === "error") throw BridgeAPIError.fromPayload(200, payload);
    const value = record(payload);
    const type = string(value?.type);
    if (type === "started") return { type };
    if (type === "progress" && string(value?.stage))
      return { type, stage: string(value?.stage) ?? "" };
    if (
      type === "partial_image" &&
      typeof value?.index === "number" &&
      typeof value.partial_index === "number" &&
      typeof value.b64_json === "string"
    ) {
      return {
        type,
        index: value.index,
        partial_index: value.partial_index,
        b64_json: value.b64_json,
      };
    }
    const response = record(value?.response);
    if (type === "completed" && response) {
      return { type, response: response as unknown as ImageResponse };
    }
    throw new BridgeProtocolError(`bridge returned unsupported SSE event type ${String(type)}`);
  }
}
