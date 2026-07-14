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

export async function buildRequest(form) {
	const operation = value(form, "operation");
	const references = await fileInput(
		form.elements.namedItem("reference-images"),
	);
	let operationFields;
	if (operation === "edit") {
		const images = await fileInput(form.elements.namedItem("edit-images"));
		if (images.length === 0)
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

	const request = {
		prompt: value(form, "prompt"),
		negative_prompt: value(form, "negative-prompt") || undefined,
		...operationFields,
		parameters,
		routing: {
			provider: value(form, "provider") || undefined,
			model: value(form, "model") || undefined,
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
		},
		policies: {
			compatibility: value(form, "compatibility") || "best_effort",
			negative_prompt: value(form, "negative-policy") || "auto",
			revised_prompt: value(form, "revised-prompt") || "include",
		},
		idempotency_key: value(form, "idempotency-key") || undefined,
		timeout_ms: optionalInteger(form, "timeout-ms"),
		user: value(form, "user") || undefined,
	};
	return removeUndefined(request);
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

	constrainOptions(form.elements.namedItem("quality"), capabilities?.qualities);
	constrainOptions(
		form.elements.namedItem("format"),
		capabilities?.output_formats,
	);
	constrainOptions(
		form.elements.namedItem("background"),
		capabilities?.backgrounds,
	);
	constrainOptions(
		form.elements.namedItem("moderation"),
		capabilities?.moderation,
	);
	constrainOptionalOptions(
		form.elements.namedItem("input-fidelity"),
		capabilities?.input_fidelities,
	);
	constrainOptions(form.elements.namedItem("action"), capabilities?.actions);
	form.elements.namedItem("aspect-ratio").disabled =
		capabilities?.aspect_ratio === "unsupported";
	form.elements.namedItem("resolution").disabled =
		capabilities?.resolution === "unsupported";

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

function constrainOptions(select, allowed) {
	for (const option of select.options) {
		option.disabled = Array.isArray(allowed) && !allowed.includes(option.value);
	}
	if (!Array.isArray(allowed) || allowed.length === 0) return;
	if (select.selectedOptions[0]?.disabled) select.value = allowed[0];
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
