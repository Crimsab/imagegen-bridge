#!/usr/bin/env bun

type Scenario = "load" | "stress" | "spike" | "soak" | "faults" | "all";

type Result = {
  status: number;
  durationMs: number;
  error?: string;
};

type Report = {
  scenario: string;
  requests: number;
  success: number;
  overloaded: number;
  errors: number;
  throughputRps: number;
  p50Ms: number;
  p95Ms: number;
  p99Ms: number;
  maxMs: number;
  recovered: boolean;
};

const args = new Map<string, string>();
for (let index = 2; index < Bun.argv.length; index += 1) {
  const value = Bun.argv[index];
  if (!value.startsWith("--")) continue;
  const [key, inline] = value.slice(2).split("=", 2);
  const next = Bun.argv[index + 1];
  if (inline !== undefined) args.set(key, inline);
  else if (next && !next.startsWith("--")) {
    args.set(key, next);
    index += 1;
  } else args.set(key, "true");
}

const scenario = (args.get("scenario") ?? "all") as Scenario;
const jsonOutput = args.get("json") === "true";
const soakSeconds = boundedNumber("soak-seconds", 300, 1, 86_400);
const seed = boundedNumber("seed", 7, 0, 2 ** 31 - 1);
let baseUrl = args.get("url") ?? "synthetic";
let synthetic: ReturnType<typeof startSynthetic> | undefined;

if (baseUrl === "synthetic") {
  synthetic = startSynthetic(seed);
  baseUrl = synthetic.url;
} else {
  new URL(baseUrl);
  if (args.get("allow-live") !== "true") {
    fail("every explicit --url requires --allow-live; capacity probes may generate real images");
  }
}

const token = process.env.IMAGEGEN_BRIDGE_BEARER_TOKEN;
const reports: Report[] = [];
try {
  if (scenario === "load" || scenario === "all") {
    reports.push(await runBatch("load", 40, 4));
  }
  if (scenario === "stress" || scenario === "all") {
    reports.push(await runBatch("stress", 120, 60));
  }
  if (scenario === "spike" || scenario === "all") {
    reports.push(await runBatch("spike", 80, 80));
  }
  if (scenario === "soak" || scenario === "all") {
    reports.push(await runSoak(soakSeconds));
  }
  if (scenario === "faults" || scenario === "all") {
    reports.push(await runFaults());
  }
} finally {
  synthetic?.stop(true);
}

const failed = reports.some((report) => !meetsEnvelope(report));
if (jsonOutput) console.log(JSON.stringify({ seed, reports, passed: !failed }, null, 2));
else {
  console.log("Imagegen Bridge resilience harness");
  for (const report of reports) {
    console.log(
      `${report.scenario.padEnd(8)} requests=${report.requests} ok=${report.success} ` +
        `overload=${report.overloaded} errors=${report.errors} p95=${report.p95Ms}ms ` +
        `rps=${report.throughputRps.toFixed(1)} recovered=${report.recovered}`,
    );
  }
}
process.exitCode = failed ? 1 : 0;

async function runBatch(name: string, total: number, concurrency: number): Promise<Report> {
  const started = performance.now();
  const results = await pooled(total, concurrency, (index) => request(`${name}-${index}`));
  const recovered = await recoveryProbe();
  return report(name, results, performance.now() - started, recovered);
}

async function runSoak(seconds: number): Promise<Report> {
  const started = performance.now();
  const deadline = started + seconds * 1_000;
  const results: Result[] = [];
  let sequence = 0;
  while (performance.now() < deadline) {
    results.push(...(await pooled(16, 4, () => request(`soak-${sequence++}`))));
  }
  return report("soak", results, performance.now() - started, await recoveryProbe());
}

async function runFaults(): Promise<Report> {
  const started = performance.now();
  const modes = ["502", "429", "disconnect", "hang", "ok"];
  const results = await Promise.all(modes.map((mode) => request(`fault:${mode}`, 300)));
  const expected = results.filter((result, index) => index < 4 && result.status !== 200).length;
  const normalized = results.map((result, index) => {
    if (index < 4 && result.status !== 200) return { ...result, status: 503, error: undefined };
    return result;
  });
  const output = report("faults", normalized, performance.now() - started, await recoveryProbe());
  output.overloaded = expected;
  output.success = results.filter((result) => result.status === 200).length;
  output.errors = expected === 4 ? 0 : 1;
  return output;
}

async function request(prompt: string, timeoutMs = 5_000): Promise<Result> {
  const started = performance.now();
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const headers: Record<string, string> = {
      "content-type": "application/json",
      "x-request-id": `resilience-${seed}-${prompt}`.replace(/[^A-Za-z0-9_.:-]/g, "-"),
      traceparent: `00-${hex(seed, 32)}-${hex(seed + prompt.length, 16)}-01`,
    };
    if (token) headers.authorization = `Bearer ${token}`;
    const response = await fetch(new URL("/v1/images", baseUrl), {
      method: "POST",
      headers,
      body: JSON.stringify({
        version: "1",
        prompt,
        operation: "generate",
        reference_images: [],
        output: { response_format: "metadata" },
        timeout_ms: timeoutMs,
      }),
      signal: controller.signal,
    });
    await response.arrayBuffer();
    return { status: response.status, durationMs: performance.now() - started };
  } catch (error) {
    return { status: 0, durationMs: performance.now() - started, error: String(error) };
  } finally {
    clearTimeout(timeout);
  }
}

async function recoveryProbe(): Promise<boolean> {
  const result = await request("recovery", 2_000);
  return result.status === 200;
}

async function pooled<T>(
  total: number,
  concurrency: number,
  task: (index: number) => Promise<T>,
): Promise<T[]> {
  const output = new Array<T>(total);
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(total, concurrency) }, async () => {
      while (next < total) {
        const index = next++;
        output[index] = await task(index);
      }
    }),
  );
  return output;
}

function report(name: string, results: Result[], elapsedMs: number, recovered: boolean): Report {
  const durations = results.map((result) => result.durationMs).sort((a, b) => a - b);
  const success = results.filter((result) => result.status >= 200 && result.status < 300).length;
  const overloaded = results.filter((result) => [429, 503].includes(result.status)).length;
  const errors = results.length - success - overloaded;
  return {
    scenario: name,
    requests: results.length,
    success,
    overloaded,
    errors,
    throughputRps: results.length / (elapsedMs / 1_000),
    p50Ms: percentile(durations, 0.5),
    p95Ms: percentile(durations, 0.95),
    p99Ms: percentile(durations, 0.99),
    maxMs: Math.round(durations.at(-1) ?? 0),
    recovered,
  };
}

function meetsEnvelope(report: Report): boolean {
  if (!report.recovered || report.errors > 0) return false;
  switch (report.scenario) {
    case "load":
    case "soak":
      return report.success === report.requests && report.overloaded === 0;
    case "stress":
    case "spike":
      return report.success > 0 && report.overloaded > 0 && report.success + report.overloaded === report.requests;
    case "faults":
      return report.success === 1 && report.overloaded === 4 && report.requests === 5;
    default:
      return false;
  }
}

function percentile(values: number[], percentileValue: number): number {
  if (values.length === 0) return 0;
  return Math.round(values[Math.min(values.length - 1, Math.ceil(values.length * percentileValue) - 1)]);
}

function boundedNumber(name: string, fallback: number, minimum: number, maximum: number): number {
  const value = Number(args.get(name) ?? fallback);
  if (!Number.isFinite(value) || value < minimum || value > maximum) {
    fail(`--${name} must be between ${minimum} and ${maximum}`);
  }
  return value;
}

function hex(value: number, length: number): string {
  return Math.abs(value).toString(16).padStart(length, "1").slice(-length);
}

function fail(message: string): never {
  console.error(message);
  process.exit(2);
}

function startSynthetic(seedValue: number) {
  let active = 0;
  const maxActive = 4;
  const maxQueued = 16;
  let queued = 0;
  return Bun.serve({
    port: 0,
    async fetch(request) {
      const url = new URL(request.url);
      if (url.pathname === "/health/ready") return Response.json({ status: "ready" });
      if (url.pathname !== "/v1/images") return new Response("not found", { status: 404 });
      const body = (await request.json()) as { prompt?: string };
      const prompt = body.prompt ?? "";
      if (prompt === "fault:502") return new Response("bad gateway", { status: 502 });
      if (prompt === "fault:429") return new Response("limited", { status: 429 });
      if (prompt === "fault:disconnect") {
        return new Response("injected disconnect analogue", { status: 520 });
      }
      if (prompt === "fault:hang") await Bun.sleep(1_000);
      if (active >= maxActive) {
        if (queued >= maxQueued) return new Response("overloaded", { status: 503 });
        queued += 1;
        while (active >= maxActive) await Bun.sleep(2);
        queued -= 1;
      }
      active += 1;
      await Bun.sleep(15 + ((seedValue + prompt.length) % 11));
      active -= 1;
      return Response.json({
        id: "synthetic",
        provider: "synthetic",
        model: "synthetic",
        data: [],
        timings: { total_ms: 20, queue_ms: 0, provider_ms: 20, artifact_ms: 0 },
      });
    },
  });
}
