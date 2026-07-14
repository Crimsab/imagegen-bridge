import { describe, expect, test } from "bun:test";
import {
	applyCapabilities,
	buildRequest,
	updateFormatFields,
	updateTransparencyFields,
} from "./form.js";

function input(value = "", extra = {}) {
	return {
		value,
		disabled: false,
		title: "",
		removeAttribute(name) {
			if (name === "title") this.title = "";
		},
		...extra,
	};
}

function select(value, values) {
	const control = input(value);
	control.options = values.map(([optionValue, textContent]) => ({
		value: optionValue,
		textContent,
		disabled: false,
	}));
	Object.defineProperty(control, "selectedOptions", {
		get: () => control.options.filter((option) => option.value === control.value),
	});
	control.querySelector = (query) => {
		const optionValue = query.match(/value="([^"]+)"/)?.[1];
		return control.options.find((option) => option.value === optionValue) || null;
	};
	return control;
}

function fakeForm(overrides = {}) {
	const fields = {
		operation: select("generate", [["generate", "Generate"], ["edit", "Edit"]]),
		prompt: input("A precise test image"),
		"negative-prompt": input("no text"),
		provider: select("codex-app-server", [["", "Default"], ["codex-app-server", "Codex"]]),
		model: input("gpt-image-2"),
		count: input("2"),
		size: select("1024x1536", [["auto", "Auto"], ["1024x1536", "1024 x 1536"]]),
		"aspect-ratio": input("2:3"),
		resolution: select("2k", [["", "Automatic"], ["2k", "2K"]]),
		quality: select("high", [["auto", "Auto"], ["high", "High"]]),
		format: select("webp", [["png", "PNG"], ["jpeg", "JPEG"], ["webp", "WebP"]]),
		compression: input("82"),
		background: select("transparent", [["auto", "Auto"], ["opaque", "Opaque"], ["transparent", "Transparent"]]),
		transparency: select("chroma_key", [["auto", "Automatic"], ["native", "Native"], ["chroma_key", "Chroma key"]]),
		"chroma-key": input("#00ff00"),
		"chroma-transparent-threshold": input("10"),
		"chroma-opaque-threshold": input("90"),
		despill: input("", { checked: true }),
		moderation: select("low", [["auto", "Auto"], ["low", "Low"]]),
		"partial-images": input("2"),
		"input-fidelity": select("high", [["", "Automatic"], ["high", "High"]]),
		action: select("edit", [["auto", "Auto"], ["edit", "Edit"]]),
		"failure-policy": select("best_effort", [["fail_fast", "Fail fast"], ["best_effort", "Best effort"]]),
		"fallback-routes": input("codex-responses:gpt-image-1"),
		"fallback-policy": select("on_error", [["on_unavailable", "Unavailable"], ["on_error", "Known errors"]]),
		"batch-execution": select("parallel", [["auto", "Auto"], ["parallel", "Parallel"]]),
		"session-mode": select("isolated", [["isolated", "Isolated"], ["persistent", "Persistent"], ["thread", "Thread"]]),
		"session-key": input(""),
		"thread-id": input(""),
		"filename-prefix": input("portrait"),
		directory: input("tests/red"),
		filename: input(""),
		collision: select("suffix", [["error", "Error"], ["suffix", "Suffix"]]),
		metadata: select("embedded", [["none", "None"], ["sidecar", "Sidecar"], ["embedded", "Embedded"]]),
		compatibility: select("strict", [["best_effort", "Best effort"], ["strict", "Strict"]]),
		"negative-policy": select("merge", [["auto", "Auto"], ["merge", "Merge"]]),
		"revised-prompt": select("require", [["include", "Include"], ["require", "Require"]]),
		"idempotency-key": input("preset-test-1"),
		"timeout-ms": input("120000"),
		user: input("qa-user"),
		"reference-images": input("", { files: [] }),
		"edit-images": input("", { files: [] }),
		mask: input("", { files: [] }),
		...overrides,
	};
	return { elements: { namedItem: (name) => fields[name] }, fields };
}

describe("advanced request controls", () => {
	test("serializes every advanced generation setting", async () => {
		const { elements } = fakeForm();
		const request = await buildRequest({ elements });
		expect(request).toEqual({
			prompt: "A precise test image",
			negative_prompt: "no text",
			operation: "generate",
			reference_images: [],
			parameters: {
				n: 2,
				size: "1024x1536",
				aspect_ratio: "2:3",
				resolution: "2k",
				quality: "high",
				output_format: "webp",
				output_compression: 82,
				background: "transparent",
				moderation: "low",
				partial_images: 2,
				failure_policy: "best_effort",
				input_fidelity: "high",
				action: "edit",
			},
			routing: {
				provider: "codex-app-server",
				model: "gpt-image-2",
				fallbacks: [{ provider: "codex-responses", model: "gpt-image-1" }],
				fallback_policy: "on_error",
			},
			session: { mode: "isolated" },
			output: {
				response_format: "artifact",
				filename_prefix: "portrait",
				directory: "tests/red",
				collision: "suffix",
				metadata: "embedded",
				transparency: {
					mode: "chroma_key",
					key_color: "#00ff00",
					transparent_threshold: 10,
					opaque_threshold: 90,
					despill: true,
				},
			},
			policies: {
				compatibility: "strict",
				negative_prompt: "merge",
				revised_prompt: "require",
				batch_execution: "parallel",
			},
			idempotency_key: "preset-test-1",
			timeout_ms: 120000,
			user: "qa-user",
		});
	});

	test("enforces codex-app-server capabilities and dependent controls", () => {
		const { elements, fields } = fakeForm({
			size: select("1024x1536", [["auto", "Auto"], ["1024x1536", "1024 x 1536"]]),
			quality: select("high", [["auto", "Auto"], ["high", "High"]]),
			format: select("webp", [["png", "PNG"], ["webp", "WebP"]]),
			background: select("auto", [["auto", "Auto"], ["opaque", "Opaque"], ["transparent", "Transparent"]]),
			moderation: select("low", [["auto", "Auto"], ["low", "Low"]]),
			action: select("edit", [["auto", "Auto"], ["edit", "Edit"]]),
			"session-mode": select("isolated", [["isolated", "Isolated"], ["persistent", "Persistent"], ["thread", "Thread"]]),
		});
		applyCapabilities({ elements }, {
			count: { min: 1, max: 16 },
			batching: { mode: "fan_out", max_parallel_outputs: 2 },
			partial_images: { min: 0, max: 0 },
			sizes: { auto: true, arbitrary: false },
			qualities: ["auto"],
			output_formats: ["png"],
			backgrounds: ["auto"],
			moderation: ["auto"],
			input_fidelities: ["high"],
			actions: ["auto"],
			aspect_ratio: "unsupported",
			resolution: "unsupported",
			persistent_sessions: true,
			explicit_threads: true,
		});

		expect(fields.size.value).toBe("auto");
		expect(fields.size.disabled).toBe(true);
		expect(fields.quality.value).toBe("auto");
		expect(fields.quality.disabled).toBe(true);
		expect(fields.format.value).toBe("png");
		expect(fields.format.disabled).toBe(true);
		expect(fields.background.disabled).toBe(false);
		expect(fields.background.options.find((option) => option.value === "opaque").disabled).toBe(true);
		expect(fields.background.options.find((option) => option.value === "transparent").disabled).toBe(false);
		expect(fields["aspect-ratio"].disabled).toBe(true);
		expect(fields.resolution.disabled).toBe(true);
		expect(fields["partial-images"].disabled).toBe(true);
		expect(fields.compression.disabled).toBe(true);
		expect(fields.transparency.disabled).toBe(true);

		fields.background.value = "transparent";
		updateTransparencyFields({ elements });
		expect(fields.transparency.disabled).toBe(false);
		expect(fields["chroma-key"].disabled).toBe(false);
		fields.transparency.value = "native";
		updateTransparencyFields({ elements });
		expect(fields["chroma-key"].disabled).toBe(true);

		fields.format.value = "webp";
		updateFormatFields({ elements });
		expect(fields.compression.disabled).toBe(false);
	});
});
