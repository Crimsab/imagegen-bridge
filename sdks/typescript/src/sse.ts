import { BridgeAPIError, BridgeProtocolError, record, string } from "./errors.js";
import type { ImageResponse, StreamEvent } from "./types.js";

export class SseDecoder {
  readonly #decoder = new TextDecoder("utf-8", { fatal: true });
  readonly #maximumLineBytes: number;
  readonly #maximumEventBytes: number;
  #lineParts: Uint8Array[] = [];
  #lineBytes = 0;
  #pendingCr = false;
  #eventName = "message";
  #data: string[] = [];
  #size = 0;

  constructor(maximumLineBytes: number, maximumEventBytes: number) {
    for (const [value, name] of [
      [maximumLineBytes, "maximumLineBytes"],
      [maximumEventBytes, "maximumEventBytes"],
    ] as const) {
      if (!Number.isSafeInteger(value) || value <= 0) {
        throw new RangeError(`${name} must be a positive safe integer`);
      }
    }
    this.#maximumLineBytes = maximumLineBytes;
    this.#maximumEventBytes = maximumEventBytes;
  }

  push(chunk: Uint8Array): StreamEvent[] {
    const events: StreamEvent[] = [];
    let start = 0;
    for (let index = 0; index < chunk.byteLength; index += 1) {
      const byte = chunk[index];
      if (this.#pendingCr) {
        const event = this.#processLine();
        if (event) events.push(event);
        this.#pendingCr = false;
        if (byte === 0x0a) {
          start = index + 1;
          continue;
        }
        start = index;
      }
      if (byte === 0x0d || byte === 0x0a) {
        this.#appendLineSegment(chunk.subarray(start, index));
        if (byte === 0x0d) this.#pendingCr = true;
        else {
          const event = this.#processLine();
          if (event) events.push(event);
        }
        start = index + 1;
      }
    }
    this.#appendLineSegment(chunk.subarray(start));
    return events;
  }

  finish(): StreamEvent[] {
    const events: StreamEvent[] = [];
    if (this.#pendingCr || this.#lineBytes > 0) {
      const event = this.#processLine();
      if (event) events.push(event);
      this.#pendingCr = false;
    }
    if (this.#data.length > 0) events.push(this.#dispatch());
    return events;
  }

  #appendLineSegment(segment: Uint8Array): void {
    if (segment.byteLength > this.#maximumLineBytes - this.#lineBytes) {
      throw new BridgeProtocolError("bridge SSE line exceeded the configured byte limit");
    }
    if (segment.byteLength > 0) this.#lineParts.push(segment.slice());
    this.#lineBytes += segment.byteLength;
  }

  #processLine(): StreamEvent | null {
    const bytes = new Uint8Array(this.#lineBytes);
    let offset = 0;
    for (const part of this.#lineParts) {
      bytes.set(part, offset);
      offset += part.byteLength;
    }
    const lineBytes = this.#lineBytes;
    this.#lineParts = [];
    this.#lineBytes = 0;
    let line: string;
    try {
      line = this.#decoder.decode(bytes);
    } catch (error) {
      throw new BridgeProtocolError("bridge returned invalid UTF-8 in the SSE stream", {
        cause: error,
      });
    }
    if (line === "") return this.#data.length > 0 ? this.#dispatch() : null;
    if (line.startsWith(":")) return null;
    this.#size += lineBytes;
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
