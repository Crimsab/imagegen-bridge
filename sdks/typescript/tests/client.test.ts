import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  BridgeAPIError,
  type EditImageRequest,
  type GenerateImageRequest,
  ImagegenBridgeClient,
} from "../src/index.js";

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
    const capabilities = await client.capabilities("codex-app-server");
    expect(capabilities.persistent_sessions).toBeTrue();
    expect(capabilities.input_fidelities).toEqual(["high"]);
    expect(capabilities.actions).toEqual(["auto"]);
    expect((await client.session("sdk-fixture")).thread_id).toBe("thread_fixture_01");
    await client.deleteSession("sdk-fixture");
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
    }
    await expect(async () => {
      for await (const _event of client.images.stream(request)) {
        // The fixture emits only a terminal error.
      }
    }).toThrow(BridgeAPIError);
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
