function value(form, name) {
	return form.elements.namedItem(name)?.value?.trim() || "";
}

function integer(form, name, fallback) {
	const parsed = Number.parseInt(value(form, name), 10);
	return Number.isFinite(parsed) ? parsed : fallback;
}

function optionalInteger(form, name) {
	const raw = value(form, name);
	return raw === "" ? undefined : Number.parseInt(raw, 10);
}

function checked(form, name) {
	return Boolean(form.elements.namedItem(name)?.checked);
}

function fallbackRoutes(form) {
	const raw = value(form, "fallback-routes");
	if (!raw) return [];
	return raw.split(",").map((entry) => {
		const route = entry.trim();
		const separator = route.indexOf(":");
		const provider = separator < 0 ? route : route.slice(0, separator).trim();
		const model = separator < 0 ? "" : route.slice(separator + 1).trim();
		if (!provider || (separator >= 0 && !model)) {
			throw new Error(
				"Fallback routes must use provider or provider:model syntax.",
			);
		}
		return { provider, model: model || undefined };
	});
}

async function fileInput(input) {
	return Promise.all([...input.files].map(fileToImageInput));
}

function fileToImageInput(file) {
	return new Promise((resolve, reject) => {
		const reader = new FileReader();
		reader.addEventListener("load", () => {
			resolve({
				type: "data_url",
				data_url: reader.result,
				media_type: file.type || undefined,
				filename: file.name,
			});
		});
		reader.addEventListener("error", () =>
			reject(new Error(`Could not read ${file.name}`)),
		);
		reader.readAsDataURL(file);
	});
}

export async function buildRequest(form, { requireEditImages = true } = {}) {
	const operation = value(form, "operation");
	const references = await fileInput(
		form.elements.namedItem("reference-images"),
	);
	let operationFields;
	if (operation === "edit") {
		const images = await fileInput(form.elements.namedItem("edit-images"));
		if (requireEditImages && images.length === 0)
			throw new Error("Choose at least one image to edit.");
		const masks = await fileInput(form.elements.namedItem("mask"));
		operationFields = {
			operation: "edit",
			images,
			mask: masks[0] || undefined,
			reference_images: references,
		};
	} else {
		operationFields = { operation: "generate", reference_images: references };
	}

	const parameters = {
		n: integer(form, "count", 1),
		size: value(form, "size") || "auto",
		aspect_ratio: value(form, "aspect-ratio") || undefined,
		resolution: value(form, "resolution") || undefined,
		quality: value(form, "quality") || "auto",
		output_format: value(form, "format") || "png",
		output_compression: optionalInteger(form, "compression"),
		background: value(form, "background") || "auto",
		moderation: value(form, "moderation") || "auto",
		partial_images: integer(form, "partial-images", 0),
		failure_policy: value(form, "failure-policy") || "fail_fast",
		input_fidelity: value(form, "input-fidelity") || undefined,
		action: value(form, "action") || "auto",
	};

	const filename = value(form, "filename");
	if (filename && parameters.n !== 1) {
		throw new Error("An exact filename requires exactly one output image.");
	}

	const sessionMode = value(form, "session-mode") || "isolated";
	const fallbacks = fallbackRoutes(form);
	if (fallbacks.length > 0 && sessionMode !== "isolated") {
		throw new Error("Provider fallback requires an isolated session.");
	}
	const transparentThreshold = integer(
		form,
		"chroma-transparent-threshold",
		12,
	);
	const opaqueThreshold = integer(form, "chroma-opaque-threshold", 96);
	if (transparentThreshold >= opaqueThreshold) {
		throw new Error("Transparent threshold must be lower than opaque threshold.");
	}

	const request = {
		prompt: value(form, "prompt"),
		negative_prompt: value(form, "negative-prompt") || undefined,
		...operationFields,
		parameters,
		routing: {
			provider: value(form, "provider") || undefined,
			model: value(form, "model") || undefined,
			fallbacks,
			fallback_policy: value(form, "fallback-policy") || "on_unavailable",
		},
		session: {
			mode: sessionMode,
			key:
				sessionMode === "persistent"
					? value(form, "session-key") || undefined
					: undefined,
			thread_id:
				sessionMode === "thread"
					? value(form, "thread-id") || undefined
					: undefined,
		},
		output: {
			response_format: "artifact",
			filename_prefix: value(form, "filename-prefix") || undefined,
			directory: value(form, "directory") || undefined,
			filename: filename || undefined,
			collision: value(form, "collision") || "error",
			metadata: value(form, "metadata") || "sidecar",
			transparency: {
				mode: value(form, "transparency") || "auto",
				key_color: value(form, "chroma-key") || undefined,
				transparent_threshold: transparentThreshold,
				opaque_threshold: opaqueThreshold,
				despill: checked(form, "despill"),
			},
		},
		policies: {
			compatibility: value(form, "compatibility") || "best_effort",
			negative_prompt: value(form, "negative-policy") || "auto",
			revised_prompt: value(form, "revised-prompt") || "include",
			batch_execution: value(form, "batch-execution") || "auto",
		},
		idempotency_key: value(form, "idempotency-key") || undefined,
		timeout_ms: optionalInteger(form, "timeout-ms"),
		user: value(form, "user") || undefined,
	};
	return removeUndefined(request);
}

export function presetTemplateFromRequest(request) {
	return removeUndefined({
		prompt: request.prompt || undefined,
		negative_prompt: request.negative_prompt,
		operation: request.operation,
		parameters: request.parameters,
		routing: request.routing,
		session: request.session,
		output: request.output,
		policies: request.policies,
		timeout_ms: request.timeout_ms,
		user: request.user,
	});
}

export function applyPresetTemplate(form, template) {
	setValue(form, "prompt", template.prompt || "");
	setValue(form, "negative-prompt", template.negative_prompt || "");
	setValue(form, "operation", template.operation || "generate");
	const parameters = template.parameters || {};
	for (const [name, field, fallback] of [
		["count", "n", 1],
		["size", "size", "auto"],
		["aspect-ratio", "aspect_ratio", ""],
		["resolution", "resolution", ""],
		["quality", "quality", "auto"],
		["format", "output_format", "png"],
		["compression", "output_compression", ""],
		["background", "background", "auto"],
		["moderation", "moderation", "auto"],
		["partial-images", "partial_images", 0],
		["failure-policy", "failure_policy", "fail_fast"],
		["input-fidelity", "input_fidelity", ""],
		["action", "action", "auto"],
	]) {
		setValue(form, name, parameters[field] ?? fallback);
	}
	const routing = template.routing || {};
	setValue(form, "provider", routing.provider || "");
	setValue(form, "model", routing.model || "");
	setValue(
		form,
		"fallback-routes",
		(routing.fallbacks || [])
			.map((route) =>
				route.model ? `${route.provider}:${route.model}` : route.provider,
			)
			.join(", "),
	);
	setValue(form, "fallback-policy", routing.fallback_policy || "on_unavailable");
	const session = template.session || {};
	setValue(form, "session-mode", session.mode || "isolated");
	setValue(form, "session-key", session.key || "");
	setValue(form, "thread-id", session.thread_id || "");
	const output = template.output || {};
	setValue(form, "filename-prefix", output.filename_prefix || "");
	setValue(form, "directory", output.directory || "");
	setValue(form, "filename", output.filename || "");
	setValue(form, "collision", output.collision || "error");
	setValue(form, "metadata", output.metadata || "sidecar");
	const transparency = output.transparency || {};
	setValue(form, "transparency", transparency.mode || "auto");
	setValue(form, "chroma-key", transparency.key_color || "");
	setValue(
		form,
		"chroma-transparent-threshold",
		transparency.transparent_threshold ?? 12,
	);
	setValue(
		form,
		"chroma-opaque-threshold",
		transparency.opaque_threshold ?? 96,
	);
	form.elements.namedItem("despill").checked = transparency.despill ?? true;
	const policies = template.policies || {};
	setValue(form, "compatibility", policies.compatibility || "best_effort");
	setValue(form, "negative-policy", policies.negative_prompt || "auto");
	setValue(form, "revised-prompt", policies.revised_prompt || "include");
	setValue(form, "batch-execution", policies.batch_execution || "auto");
	setValue(form, "timeout-ms", template.timeout_ms ?? "");
	setValue(form, "user", template.user || "");
	setValue(form, "idempotency-key", "");
	updateOperationFields(template.operation || "generate");
	updateSessionFields(session.mode || "isolated");
	updateFormatFields(form);
	updateTransparencyFields(form);
}

function setValue(form, name, value) {
	form.elements.namedItem(name).value = String(value);
}

function removeUndefined(value) {
	if (Array.isArray(value)) return value.map(removeUndefined);
	if (value && typeof value === "object") {
		return Object.fromEntries(
			Object.entries(value)
				.filter(([, entry]) => entry !== undefined)
				.map(([key, entry]) => [key, removeUndefined(entry)]),
		);
	}
	return value;
}

export function updateOperationFields(operation) {
	const fileGroup = document.querySelector("#image-inputs");
	const editImages = document.querySelector("#edit-images-field");
	const mask = document.querySelector("#mask-field");
	const isEdit = operation === "edit";
	fileGroup.hidden = false;
	editImages.hidden = !isEdit;
	mask.hidden = !isEdit;
	document.querySelector("#edit-images").required = isEdit;
}

export function updateSessionFields(mode) {
	const keyField = document.querySelector("#session-key-field");
	const threadField = document.querySelector("#thread-id-field");
	const key = document.querySelector("#session-key");
	const thread = document.querySelector("#thread-id");
	keyField.hidden = mode !== "persistent";
	threadField.hidden = mode !== "thread";
	key.required = mode === "persistent";
	thread.required = mode === "thread";
}

export function applyCapabilities(form, capabilities) {
	const count = form.elements.namedItem("count");
	count.min = String(capabilities?.count?.min ?? 1);
	count.max = String(capabilities?.count?.max ?? 16);
	if (Number(count.value) > Number(count.max)) count.value = count.max;
	const batching = capabilities?.batching;
	count.title =
		batching?.mode === "fan_out"
			? `The bridge runs bounded upstream requests, up to ${batching.max_parallel_outputs} at once.`
			: "The provider returns the requested outputs natively.";

	const partial = form.elements.namedItem("partial-images");
	partial.min = String(capabilities?.partial_images?.min ?? 0);
	partial.max = String(capabilities?.partial_images?.max ?? 3);
	if (Number(partial.value) > Number(partial.max)) partial.value = partial.max;

	constrainOptions(
		form.elements.namedItem("size"),
		allowedSizes(capabilities?.sizes),
		true,
	);
	constrainOptions(
		form.elements.namedItem("quality"),
		capabilities?.qualities,
		true,
	);
	constrainOptions(
		form.elements.namedItem("format"),
		capabilities?.output_formats,
		true,
	);
	const backgrounds = capabilities?.backgrounds
		? [...new Set([...capabilities.backgrounds, "transparent"])]
		: undefined;
	constrainOptions(form.elements.namedItem("background"), backgrounds);
	constrainOptions(
		form.elements.namedItem("moderation"),
		capabilities?.moderation,
		true,
	);
	constrainOptionalOptions(
		form.elements.namedItem("input-fidelity"),
		capabilities?.input_fidelities,
	);
	constrainOptions(form.elements.namedItem("action"), capabilities?.actions, true);
	const aspectRatio = form.elements.namedItem("aspect-ratio");
	aspectRatio.disabled =
		capabilities?.aspect_ratio === "unsupported";
	aspectRatio.title = aspectRatio.disabled
		? "The selected provider cannot receive an aspect-ratio hint."
		: "Aspect-ratio support reported by the selected provider.";
	const resolution = form.elements.namedItem("resolution");
	resolution.disabled =
		capabilities?.resolution === "unsupported";
	resolution.title = resolution.disabled
		? "The selected provider cannot receive a resolution hint."
		: "Resolution support reported by the selected provider.";
	partial.disabled = Number(partial.max) === 0;
	partial.title = partial.disabled
		? "The selected provider does not stream partial images."
		: "Number of transient progress previews requested from the provider.";
	updateFormatFields(form);
	updateTransparencyFields(form);

	const session = form.elements.namedItem("session-mode");
	session.querySelector('option[value="persistent"]').disabled = capabilities
		? !capabilities.persistent_sessions
		: false;
	session.querySelector('option[value="thread"]').disabled = capabilities
		? !capabilities.explicit_threads
		: false;
	if (session.selectedOptions[0]?.disabled) {
		session.value = "isolated";
		updateSessionFields("isolated");
	}
}

function allowedSizes(sizes) {
	if (!sizes || sizes.arbitrary) return undefined;
	const allowed = [...(sizes.allowed || [])];
	if (sizes.auto) allowed.unshift("auto");
	return [...new Set(allowed)];
}

function constrainOptions(select, allowed, disableWhenFixed = false) {
	for (const option of select.options) {
		option.disabled = Array.isArray(allowed) && !allowed.includes(option.value);
	}
	if (!Array.isArray(allowed) || allowed.length === 0) {
		select.disabled = false;
		select.removeAttribute("title");
		return;
	}
	if (select.selectedOptions[0]?.disabled) select.value = allowed[0];
	select.disabled = disableWhenFixed && allowed.length === 1;
	select.title = select.disabled
		? `The selected provider only supports ${select.selectedOptions[0]?.textContent || allowed[0]}.`
		: "Options are limited to capabilities reported by the selected provider.";
}

function constrainOptionalOptions(select, allowed) {
	for (const option of select.options) {
		option.disabled =
			option.value !== "" &&
			Array.isArray(allowed) &&
			!allowed.includes(option.value);
	}
	if (select.selectedOptions[0]?.disabled) select.value = "";
}

export function updateFormatFields(form) {
	const format = form.elements.namedItem("format");
	const compression = form.elements.namedItem("compression");
	const compressible = format.value === "jpeg" || format.value === "webp";
	compression.disabled = !compressible;
	compression.title = compressible
		? "JPEG/WebP encoding quality from 0 through 100."
		: "Compression is available only for JPEG and WebP output.";
}

export function updateTransparencyFields(form) {
	const transparent = form.elements.namedItem("background").value === "transparent";
	const strategy = form.elements.namedItem("transparency");
	strategy.disabled = !transparent;
	strategy.title = transparent
		? "Choose native alpha or bridge-emulated chroma removal."
		: "Transparency controls apply only when Background is Transparent.";
	const chroma = transparent && strategy.value !== "native";
	for (const name of [
		"chroma-key",
		"chroma-transparent-threshold",
		"chroma-opaque-threshold",
		"despill",
	]) {
		const control = form.elements.namedItem(name);
		control.disabled = !chroma;
		control.title = chroma
			? "Used by bridge-emulated chroma-key transparency."
			: "Chroma controls require transparent background without native-only alpha.";
	}
}
