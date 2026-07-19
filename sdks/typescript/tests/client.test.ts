import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  BridgeAPIError,
  BridgeProtocolError,
  type EditImageRequest,
  type GenerateImageRequest,
  ImagegenBridgeClient,
} from "../src/index.js";
import { SseDecoder } from "../src/sse.js";

const repositoryRoot = new URL("../../../", import.meta.url).pathname;
const mockBinary =
  process.env.IMAGEGEN_BRIDGE_SDK_MOCK ??
  `${repositoryRoot}target/debug/imagegen-bridge-sdk-mock-server`;

let processHandle: ReturnType<typeof Bun.spawn>;
let bridgeUrl: string;
let generateFixture: GenerateImageRequest;
let editFixture: EditImageRequest;

beforeAll(async () => {
  generateFixture = (await Bun.file(
    `${repositoryRoot}fixtures/sdk/generate-request.json`,
  ).json()) as GenerateImageRequest;
  editFixture = (await Bun.file(
    `${repositoryRoot}fixtures/sdk/edit-request.json`,
  ).json()) as EditImageRequest;
  processHandle = Bun.spawn([mockBinary], { stdout: "pipe", stderr: "pipe" });
  if (!processHandle.stdout || typeof processHandle.stdout === "number") {
    throw new Error("mock server stdout is unavailable");
  }
  const reader = processHandle.stdout.getReader();
  const decoder = new TextDecoder();
  let line = "";
  while (!line.includes("\n")) {
    const item = await reader.read();
    if (item.done) throw new Error("mock server exited before announcing its address");
    line += decoder.decode(item.value, { stream: true });
  }
  reader.releaseLock();
  bridgeUrl = (JSON.parse(line.slice(0, line.indexOf("\n"))) as { base_url: string }).base_url;
});

afterAll(async () => {
  processHandle.kill("SIGINT");
  expect(await processHandle.exited).toBe(0);
});

describe("ImagegenBridgeClient", () => {
  test("rejects plaintext remote URLs before fetch sees credentials", async () => {
    let calls = 0;
    const fetch = (async () => {
      calls += 1;
      return new Response('{"status":"live"}', {
        headers: { "content-type": "application/json" },
      });
    }) as unknown as typeof globalThis.fetch;
    expect(
      () =>
        new ImagegenBridgeClient({
          baseUrl: "http://10.0.0.2:8787",
          bearerToken: "secret",
          fetch,
        }),
    ).toThrow("must use HTTPS");
    expect(calls).toBe(0);

    const allowed = new ImagegenBridgeClient({
      baseUrl: "http://10.0.0.2:8787",
      allowInsecureRemoteHttp: true,
      fetch,
    });
    expect((await allowed.health()).status).toBe("live");
    expect(calls).toBe(1);
  });

  test("bounds JSON, partial previews, and SSE lines before unbounded buffering", async () => {
    const oversizedJson = (async () =>
      new Response('{"status":"live"}', {
        headers: { "content-type": "application/json" },
      })) as unknown as typeof globalThis.fetch;
    const client = new ImagegenBridgeClient({
      baseUrl: "https://bridge.example",
      fetch: oversizedJson,
      maxJsonBodyBytes: 8,
    });
    await expect(client.health()).rejects.toThrow("JSON body exceeds");

    let partialReads = 0;
    const partial = (async () =>
      new Response(
        new ReadableStream({
          pull(controller) {
            partialReads += 1;
            controller.enqueue(new Uint8Array([1]));
          },
        }),
        {
          headers: {
            "content-type": "image/png",
            "content-length": String(16 * 1024 * 1024 + 1),
          },
        },
      )) as unknown as typeof globalThis.fetch;
    const partialClient = new ImagegenBridgeClient({
      baseUrl: "https://bridge.example",
      fetch: partial,
    });
    await expect(partialClient.jobs.partial("job-1")).rejects.toThrow("response body exceeds");
    expect(partialReads).toBeLessThanOrEqual(1);

    const decoder = new SseDecoder(8, 32);
    expect(() => decoder.push(new TextEncoder().encode(`:${"x".repeat(9)}`))).toThrow(
      BridgeProtocolError,
    );
  });

  test("rejects redirects instead of forwarding authorization", async () => {
    let redirectMode: RequestRedirect | undefined;
    const fetch = (async (_input: URL | RequestInfo, init?: RequestInit) => {
      redirectMode = init?.redirect;
      return new Response(null, { status: 302, headers: { location: "http://bridge.example" } });
    }) as unknown as typeof globalThis.fetch;
    const client = new ImagegenBridgeClient({
      baseUrl: "https://bridge.example",
      bearerToken: "secret",
      fetch,
    });
    await expect(client.health()).rejects.toThrow("redirects are not allowed");
    expect(redirectMode).toBe("manual");
  });

  test("matches the shared generation, edit, discovery, session, and health contract", async () => {
    const client = new ImagegenBridgeClient({ baseUrl: bridgeUrl, bearerToken: "sdk-test-token" });
    const generated = await client.images.generate(generateFixture);
    expect(generated.id).toBe("img_fixture_01");
    expect(generated.data[0]?.width).toBe(1);
    expect(generated.data[0]?.index).toBe(0);
    expect(generated.requested.failure_policy).toBe("fail_fast");
    expect(generated.session?.reused).toBeTrue();

    const edited = await client.images.edit(editFixture);
    expect(edited.data[0]?.type).toBe("b64_json");

    const providers = await client.providers({ limit: 2 });
    expect(providers.items.map((provider) => provider.name)).toEqual([
      "codex-app-server",
      "codex-responses",
    ]);
    expect(providers.items[1]?.models).toEqual([
      "gpt-image-2",
      "gpt-image-1.5",
      "gpt-image-1",
      "gpt-image-1-mini",
    ]);
    const capabilities = await client.capabilities("codex-app-server");
    expect(capabilities.persistent_sessions).toBeTrue();
    expect(capabilities.count.max).toBe(4);
    expect(capabilities.batching.mode).toBe("fan_out");
    expect(capabilities.transparent_background).toBe("emulated");
    expect(capabilities.batching.native_count.max).toBe(1);
    expect(capabilities.batching.max_parallel_outputs).toBe(2);
    expect(capabilities.input_fidelities).toEqual(["high"]);
    expect(capabilities.actions).toEqual(["auto"]);
    expect((await client.capabilities("codex-responses", { model: "gpt-image-1" })).model).toBe(
      "gpt-image-1",
    );
    const diagnostics = await client.diagnostics();
    expect(diagnostics.configuration.listener_scope).toBe("loopback");
    expect(diagnostics.jobs?.total).toBe(1);
    expect(diagnostics.providers[1]?.provider).toBe("codex-responses");
    expect(diagnostics.events?.capacity).toBe(256);
    expect(diagnostics.events?.items[0]?.route).toBe("/v1/jobs");
    expect((await client.session("sdk-fixture")).thread_id).toBe("thread_fixture_01");
    await client.deleteSession("sdk-fixture");
    const queued = await client.jobs.create(generateFixture);
    expect(queued.status).toBe("queued");
    expect(queued.request.output?.response_format).toBe("artifact");
    const completed = await client.jobs.get(queued.id);
    expect(completed.status).toBe("succeeded");
    expect(completed.result?.data[0]?.type).toBe("artifact");
    expect(Array.from((await client.jobs.partial(queued.id)).slice(0, 8))).toEqual([
      137, 80, 78, 71, 13, 10, 26, 10,
    ]);
    const jobs = await client.jobs.list({
      status: "succeeded",
      visibility: "active",
      favorite: true,
      search: "fixture",
    });
    expect(jobs.items[0]?.id).toBe(queued.id);
    expect(jobs.next_cursor).toBe("sdk-next");
    expect((await client.jobs.update(queued.id, { favorite: true, deleted: false })).favorite).toBe(
      true,
    );
    expect((await client.jobs.cancel(queued.id)).status).toBe("cancelled");
    const presets = await client.presets.list();
    expect(presets.items[0]?.name).toBe("portrait-high");
    const preset = await client.presets.create({
      name: "sdk-preset",
      description: "SDK preset",
      template: { operation: "generate", parameters: { quality: "high" } },
    });
    expect(preset.template.parameters?.quality).toBe("high");
    expect((await client.presets.get("sdk-preset")).name).toBe("sdk-preset");
    expect(
      (
        await client.presets.update("sdk-preset", {
          description: "Updated",
          template: { operation: "generate" },
        })
      ).description,
    ).toBe("Updated");
    await client.presets.delete("sdk-preset");
    expect((await client.health()).status).toBe("live");
    expect((await client.health({ ready: true })).status).toBe("ready");
  });

  test("decodes fragmented SSE and exposes every event variant", async () => {
    const client = new ImagegenBridgeClient({ baseUrl: bridgeUrl, bearerToken: "sdk-test-token" });
    const events = [];
    for await (const event of client.images.stream(generateFixture)) events.push(event);
    expect(events.map((event) => event.type)).toEqual([
      "started",
      "progress",
      "partial_image",
      "completed",
    ]);
  });

  test("returns structured HTTP and SSE errors", async () => {
    const client = new ImagegenBridgeClient({ baseUrl: bridgeUrl, bearerToken: "sdk-test-token" });
    const request: GenerateImageRequest = { operation: "generate", prompt: "trigger-error" };
    try {
      await client.images.generate(request);
      throw new Error("expected generation to fail");
    } catch (error) {
      expect(error).toBeInstanceOf(BridgeAPIError);
      const api = error as BridgeAPIError;
      expect(api.statusCode).toBe(429);
      expect(api.bridgeCode).toBe("rate_limited");
      expect(api.retryable).toBeTrue();
      expect(api.suggestions.length).toBeGreaterThan(0);
    }
    await expect(async () => {
      for await (const _event of client.images.stream(request)) {
        // The fixture emits only a terminal error.
      }
    }).toThrow(BridgeAPIError);
    await expect(client.jobs.list({ includeDeleted: true, visibility: "hidden" })).rejects.toThrow(
      "cannot be combined",
    );
  });

  test("supports abort signals and provider switching as request configuration", async () => {
    const client = new ImagegenBridgeClient({ baseUrl: bridgeUrl, bearerToken: "sdk-test-token" });
    const controller = new AbortController();
    controller.abort();
    await expect(
      client.images.generate(generateFixture, { signal: controller.signal }),
    ).rejects.toThrow();

    const switched: GenerateImageRequest = {
      ...generateFixture,
      routing: { ...generateFixture.routing, provider: "codex-responses" },
    };
    expect((await client.images.generate(switched)).provider).toBe("codex-responses");
  });

  test("requires the configured bridge bearer token", async () => {
    const client = new ImagegenBridgeClient({ baseUrl: bridgeUrl, bearerToken: "wrong" });
    await expect(client.providers()).rejects.toMatchObject({ statusCode: 401 });
  });
});
