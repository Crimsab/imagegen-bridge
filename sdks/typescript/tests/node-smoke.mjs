import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createInterface } from "node:readline";

import { ImagegenBridgeClient } from "../dist/index.js";

const repositoryRoot = new URL("../../../", import.meta.url).pathname;
const binary =
  process.env.IMAGEGEN_BRIDGE_SDK_MOCK ??
  `${repositoryRoot}target/debug/imagegen-bridge-sdk-mock-server`;
const fixture = JSON.parse(
  await readFile(`${repositoryRoot}fixtures/sdk/generate-request.json`, "utf8"),
);
const server = spawn(binary, [], { stdio: ["ignore", "pipe", "pipe"] });
const lines = createInterface({ input: server.stdout });
const first = await new Promise((resolve, reject) => {
  lines.once("line", resolve);
  server.once("error", reject);
  server.once("exit", (code) => reject(new Error(`mock server exited early with ${code}`)));
});
const { base_url: baseUrl } = JSON.parse(first);

try {
  const client = new ImagegenBridgeClient({ baseUrl, bearerToken: "sdk-test-token" });
  const response = await client.images.generate(fixture);
  assert.equal(response.id, "img_fixture_01");
  assert.equal(response.data[0].width, 1);
} finally {
  lines.close();
  server.kill("SIGINT");
  const code = await new Promise((resolve) => server.once("exit", resolve));
  assert.equal(code, 0);
}
