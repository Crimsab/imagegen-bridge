import { BridgeApi, BridgeApiError } from "/dashboard/api.js";
import {
	applyCapabilities,
	buildRequest,
	updateOperationFields,
	updateSessionFields,
} from "/dashboard/form.js";

const api = new BridgeApi();
const state = {
	cursor: "",
	details: new Map(),
	detailFailures: new Set(),
	summaries: [],
	assetUrls: new Map(),
	pollTimer: null,
	searchTimer: null,
	loadSequence: 0,
	authenticationPrompted: false,
	providers: [],
};

const PARAMETER_LABELS = {
	n: "Outputs",
	size: "Size",
	aspect_ratio: "Aspect ratio",
	resolution: "Resolution",
	quality: "Quality",
	output_format: "Format",
	output_compression: "Compression",
	background: "Background",
	moderation: "Moderation",
	partial_images: "Partial images",
	failure_policy: "Failure policy",
	input_fidelity: "Input fidelity",
	action: "Action",
};

const elements = {
	form: document.querySelector("#generation-form"),
	formMessage: document.querySelector("#form-message"),
	submit: document.querySelector("#submit-button"),
	operation: document.querySelector("#operation"),
	sessionMode: document.querySelector("#session-mode"),
	provider: document.querySelector("#provider"),
	model: document.querySelector("#model"),
	knownModels: document.querySelector("#known-models"),
	providerNote: document.querySelector("#provider-note"),
	grid: document.querySelector("#job-grid"),
	libraryMessage: document.querySelector("#library-message"),
	statusFilter: document.querySelector("#status-filter"),
	searchFilter: document.querySelector("#search-filter"),
	favoritesFilter: document.querySelector("#favorites-filter"),
	deletedFilter: document.querySelector("#deleted-filter"),
	loadMore: document.querySelector("#load-more-button"),
	refresh: document.querySelector("#refresh-button"),
	connectionStatus: document.querySelector("#connection-status"),
	connectionButton: document.querySelector("#connection-button"),
	operatorButton: document.querySelector("#operator-button"),
	operatorDialog: document.querySelector("#operator-dialog"),
	operatorMessage: document.querySelector("#operator-message"),
	operatorSummary: document.querySelector("#operator-summary"),
	capabilityTableBody: document.querySelector("#capability-table-body"),
	eventSummary: document.querySelector("#event-summary"),
	eventTableBody: document.querySelector("#event-table-body"),
	provenanceList: document.querySelector("#provenance-list"),
	refreshOperator: document.querySelector("#refresh-operator-button"),
	closeOperator: document.querySelector("#close-operator-button"),
	sessionForm: document.querySelector("#session-lookup-form"),
	operatorSessionKey: document.querySelector("#operator-session-key"),
	operatorSessionProvider: document.querySelector("#operator-session-provider"),
	sessionResult: document.querySelector("#session-result"),
	confirmDialog: document.querySelector("#confirm-dialog"),
	confirmTitle: document.querySelector("#confirm-title"),
	confirmMessage: document.querySelector("#confirm-message"),
	confirmAction: document.querySelector("#confirm-action-button"),
	cancelConfirmation: document.querySelector("#cancel-confirmation-button"),
	connectionDialog: document.querySelector("#connection-dialog"),
	token: document.querySelector("#bearer-token"),
	saveToken: document.querySelector("#save-token-button"),
	clearToken: document.querySelector("#clear-token-button"),
	jobDialog: document.querySelector("#job-dialog"),
	jobDetail: document.querySelector("#job-detail"),
	detailTitle: document.querySelector("#detail-title"),
	detailKicker: document.querySelector("#detail-kicker"),
	detailMessage: document.querySelector("#detail-message"),
	closeDetail: document.querySelector("#close-detail-button"),
};

function create(tag, className = "", text = "") {
	const node = document.createElement(tag);
	if (className) node.className = className;
	if (text) node.textContent = text;
	return node;
}

function makeButton(label, className, action) {
	const button = create("button", `button ${className}`, label);
	button.type = "button";
	button.addEventListener("click", action);
	return button;
}

function setConnection(label, status) {
	elements.connectionStatus.textContent = label;
	elements.connectionStatus.dataset.state = status;
}

function setMessage(element, message = "", status = "") {
	element.textContent = message;
	if (status) element.dataset.state = status;
	else delete element.dataset.state;
}

function showAuthentication() {
	elements.token.value = api.token;
	if (!elements.connectionDialog.open) elements.connectionDialog.showModal();
	state.authenticationPrompted = true;
}

function handleError(error, target = elements.libraryMessage) {
	const message = error instanceof Error ? error.message : String(error);
	setMessage(target, message, "error");
	if (error instanceof BridgeApiError && error.authenticationRequired) {
		setConnection("Authentication required", "error");
		if (!state.authenticationPrompted) showAuthentication();
	} else {
		setConnection("Connection error", "error");
	}
}

async function loadProviders() {
	const current = elements.provider.value;
	const page = await api.providers();
	const options = [new Option("Default", "")];
	for (const provider of page.items || []) {
		const suffix = provider.experimental ? " (experimental)" : "";
		options.push(
			new Option(`${provider.display_name}${suffix}`, provider.name),
		);
	}
	state.providers = page.items || [];
	elements.provider.replaceChildren(...options);
	elements.operatorSessionProvider.replaceChildren(
		...options.map((option) => option.cloneNode(true)),
	);
	elements.provider.value = current;
	setConnection("Connected", "ready");
	state.authenticationPrompted = false;
	await loadCapabilities();
}

async function loadCapabilities() {
	const provider = elements.provider.value;
	updateKnownModels(provider);
	if (!provider) {
		applyCapabilities(elements.form, null);
		elements.providerNote.textContent = "Provider default";
		return;
	}
	try {
		const capabilities = await api.capabilities(
			provider,
			elements.model.value.trim(),
		);
		applyCapabilities(elements.form, capabilities);
		const model = capabilities.model ? ` · ${capabilities.model}` : "";
		elements.providerNote.textContent = `${capabilities.provider}${model}`;
	} catch (error) {
		handleError(error, elements.formMessage);
	}
}

function updateKnownModels(providerName) {
	const providers = providerName
		? state.providers.filter((provider) => provider.name === providerName)
		: state.providers;
	const models = [
		...new Set(providers.flatMap((provider) => provider.models || [])),
	];
	elements.knownModels.replaceChildren(
		...models.map((model) => new Option("", model)),
	);
}

async function loadOperator() {
	setMessage(elements.operatorMessage, "Loading operator diagnostics");
	elements.refreshOperator.disabled = true;
	try {
		const [diagnostics, providerPage] = await Promise.all([
			api.diagnostics(),
			api.providers(),
		]);
		state.providers = providerPage.items || [];
		const targets = state.providers.flatMap((provider) => {
			const models = provider.models?.length ? provider.models : [""];
			return models.map((model) => ({ provider, model }));
		});
		const capabilityRows = new Array(targets.length);
		const indexedTargets = targets.map((target, index) => ({ index, target }));
		await mapLimit(indexedTargets, 4, async ({ index, target }) => {
			try {
				capabilityRows[index] = {
					...target,
					capabilities: await api.capabilities(
						target.provider.name,
						target.model,
					),
				};
			} catch (error) {
				capabilityRows[index] = { ...target, error };
			}
		});
		renderOperatorSummary(diagnostics);
		renderCapabilityMatrix(capabilityRows, diagnostics.providers || []);
		renderOperatorEvents(diagnostics.events);
		renderProvenance(diagnostics.configuration?.provenance || []);
		setMessage(elements.operatorMessage, "Diagnostics are current", "ready");
		setConnection("Connected", "ready");
	} catch (error) {
		handleError(error, elements.operatorMessage);
	} finally {
		elements.refreshOperator.disabled = false;
	}
}

function renderOperatorSummary(diagnostics) {
	const config = diagnostics.configuration || {};
	const jobs = diagnostics.jobs;
	const ready = (diagnostics.providers || []).filter(
		(provider) => provider.status === "ready",
	).length;
	const totalProviders = (diagnostics.providers || []).length;
	const items = [
		["Bridge", diagnostics.bridge_version || "unknown"],
		["Providers", `${ready}/${totalProviders} ready`],
		[
			"Listener",
			config.listener_port == null
				? config.listener_scope || "unknown"
				: `${config.listener_scope || "unknown"} · ${config.listener_port}`,
		],
		[
			"Bridge auth",
			config.authentication_required ? "required" : "not configured",
		],
		["Runtime queued", String(diagnostics.runtime?.global_queued ?? 0)],
		[
			"Active workers",
			jobs ? `${jobs.active_workers}/${jobs.max_running}` : "disabled",
		],
		["Retained jobs", jobs ? String(jobs.total) : "disabled"],
		["Job database", jobs ? formatBytes(jobs.database_bytes) : "disabled"],
		[
			"Artifact storage",
			diagnostics.artifact_storage_enabled ? "enabled" : "disabled",
		],
		["Metrics", config.metrics_enabled ? "enabled" : "disabled"],
		[
			"Redacted events",
			diagnostics.events
				? `${diagnostics.events.items?.length ?? 0}/${diagnostics.events.capacity}`
				: "unavailable",
		],
	];
	const nodes = items.map(([term, value]) => {
		const item = create("div");
		item.append(create("dt", "", term), create("dd", "", value));
		return item;
	});
	elements.operatorSummary.replaceChildren(...nodes);
}

function renderCapabilityMatrix(rows, readiness) {
	const health = new Map(readiness.map((item) => [item.provider, item]));
	const rendered = rows.map(({ provider, model, capabilities, error }) => {
		const row = document.createElement("tr");
		const providerHealth = health.get(provider.name);
		const status = providerHealth?.status || "unknown";
		const values = error
			? [
					provider.name,
					status,
					model || "provider default",
					"—",
					"—",
					"—",
					"—",
					"—",
					error instanceof Error ? error.message : "Capability error",
				]
			: [
					provider.name,
					status,
					capabilities.model || "provider default",
					capabilities.generation ? "Yes" : "No",
					capabilities.edits ? "Yes" : "No",
					`${capabilities.count?.min ?? "?"}–${capabilities.count?.max ?? "?"}`,
					joinValues(capabilities.output_formats),
					capabilities.persistent_sessions
						? capabilities.explicit_threads
							? "Keys + threads"
							: "Keys"
						: "No",
					capabilities.experimental ? "Experimental" : "—",
				];
		for (const [index, value] of values.entries()) {
			const cell = create("td", index === 1 ? "readiness" : "", String(value));
			if (index === 1) cell.dataset.state = status;
			row.append(cell);
		}
		return row;
	});
	if (rendered.length === 0) {
		const row = document.createElement("tr");
		const cell = create("td", "", "No providers are registered.");
		cell.colSpan = 9;
		row.append(cell);
		rendered.push(row);
	}
	elements.capabilityTableBody.replaceChildren(...rendered);
}

function renderOperatorEvents(history) {
	const items = history?.items || [];
	const capacity = history?.capacity ?? 0;
	const dropped = history?.dropped ?? 0;
	setMessage(
		elements.eventSummary,
		capacity > 0
			? `Newest first · ${items.length}/${capacity} retained · ${dropped} overwritten. No prompts, request/job/session IDs, headers, paths, or payloads are stored.`
			: "Redacted event history is unavailable from this server.",
	);
	const rows = items.map((event) => {
		const row = document.createElement("tr");
		const values = [
			formatEventTime(event.timestamp_ms),
			event.method,
			event.route,
			String(event.status),
			formatDuration(event.duration_ms),
		];
		for (const value of values) row.append(create("td", "", value));
		return row;
	});
	if (rows.length === 0) {
		const row = document.createElement("tr");
		const cell = create("td", "", "No API events have been recorded yet.");
		cell.colSpan = 5;
		row.append(cell);
		rows.push(row);
	}
	elements.eventTableBody.replaceChildren(...rows);
}

function renderProvenance(provenance) {
	const rows = provenance.map((origin) => {
		const row = create("div", "provenance-row");
		row.append(
			create("span", "", origin.field),
			create(
				"span",
				"provenance-source",
				origin.key === origin.field
					? origin.source
					: `${origin.source} · ${origin.key}`,
			),
		);
		return row;
	});
	elements.provenanceList.replaceChildren(
		...(rows.length > 0
			? rows
			: [
					create(
						"div",
						"provenance-row",
						"Provenance unavailable for this embedded host.",
					),
				]),
	);
}

function joinValues(values) {
	return Array.isArray(values) && values.length > 0
		? values.join(", ")
		: "None";
}

async function inspectSession(event) {
	event.preventDefault();
	const key = elements.operatorSessionKey.value.trim();
	const provider = elements.operatorSessionProvider.value;
	if (!key) return;
	setMessage(elements.sessionResult, "Looking up session");
	try {
		const session = await api.getSession(key, provider);
		renderSession(session, provider);
	} catch (error) {
		handleError(error, elements.sessionResult);
	}
}

function renderSession(session, provider) {
	const details = document.createElement("dl");
	for (const [term, value] of [
		["Key", session.key],
		["Thread", session.thread_id],
		["Reused", session.reused ? "yes" : "no"],
	]) {
		details.append(create("dt", "", term), create("dd", "", value || "—"));
	}
	const remove = makeButton("Delete binding", "danger", async () => {
		const confirmed = await confirmAction({
			title: "Delete persistent session?",
			message: `Delete the local binding “${session.key}”? The upstream Codex thread is not deleted.`,
			action: "Delete session",
		});
		if (!confirmed) return;
		remove.disabled = true;
		try {
			await api.deleteSession(session.key, provider);
			setMessage(elements.sessionResult, "Session binding deleted", "ready");
		} catch (error) {
			handleError(error, elements.sessionResult);
		} finally {
			remove.disabled = false;
		}
	});
	elements.sessionResult.replaceChildren(details, remove);
}

function confirmAction({ title, message, action }) {
	elements.confirmTitle.textContent = title;
	elements.confirmMessage.textContent = message;
	elements.confirmAction.textContent = action;
	elements.confirmDialog.showModal();
	elements.cancelConfirmation.focus();
	return new Promise((resolve) => {
		elements.confirmDialog.addEventListener(
			"close",
			() => resolve(elements.confirmDialog.returnValue === "confirm"),
			{ once: true },
		);
	});
}

function showSkeletons() {
	const skeletons = Array.from({ length: 6 }, () => create("div", "skeleton"));
	elements.grid.replaceChildren(...skeletons);
	elements.grid.setAttribute("aria-busy", "true");
}

async function loadJobs({ append = false, quiet = false } = {}) {
	const sequence = ++state.loadSequence;
	if (!append && !quiet) showSkeletons();
	if (!quiet)
		setMessage(
			elements.libraryMessage,
			append ? "Loading older jobs" : "Loading jobs",
		);
	elements.loadMore.disabled = true;
	try {
		const page = await api.listJobs({
			cursor: append ? state.cursor : "",
			status: elements.statusFilter.value,
			visibility: elements.deletedFilter.checked ? "hidden" : "active",
			favorite: elements.favoritesFilter.checked,
			search: elements.searchFilter.value,
		});
		if (sequence !== state.loadSequence) return;

		state.summaries = append ? [...state.summaries, ...page.items] : page.items;
		state.cursor = page.next_cursor || "";
		elements.loadMore.hidden = !state.cursor;
		renderJobs();

		await mapLimit(page.items, 6, async (summary) => {
			try {
				const detail = await api.getJob(summary.id);
				state.details.set(summary.id, detail);
				state.detailFailures.delete(summary.id);
			} catch (error) {
				if (error instanceof BridgeApiError && error.authenticationRequired)
					throw error;
				state.detailFailures.add(summary.id);
			}
		});
		if (sequence !== state.loadSequence) return;
		renderJobs();
		setMessage(
			elements.libraryMessage,
			`${visibleSummaries().length} ${visibleSummaries().length === 1 ? "job" : "jobs"} shown`,
		);
		setConnection("Connected", "ready");
		state.authenticationPrompted = false;
		schedulePoll();
	} catch (error) {
		if (sequence !== state.loadSequence) return;
		elements.grid.replaceChildren(
			create("div", "empty-state", "History could not be loaded."),
		);
		handleError(error);
	} finally {
		if (sequence === state.loadSequence) {
			elements.grid.setAttribute("aria-busy", "false");
			elements.loadMore.disabled = false;
		}
	}
}

async function mapLimit(items, limit, task) {
	const queue = [...items];
	const workers = Array.from(
		{ length: Math.min(limit, queue.length) },
		async () => {
			while (queue.length > 0) await task(queue.shift());
		},
	);
	await Promise.all(workers);
}

function visibleSummaries() {
	return state.summaries;
}

function renderJobs() {
	const summaries = visibleSummaries();
	if (summaries.length === 0) {
		const message =
			state.summaries.length > 0
				? "No visible jobs. Adjust the history filters to find hidden or favorite work."
				: elements.favoritesFilter.checked
					? "No favorites match this view."
					: elements.deletedFilter.checked
						? "No hidden jobs match this view."
						: "No jobs yet. Queue an operation to start the gallery.";
		elements.grid.replaceChildren(create("div", "empty-state", message));
		return;
	}
	elements.grid.replaceChildren(...summaries.map(renderCard));
}

function renderCard(summary) {
	const detail = state.details.get(summary.id);
	const card = create("article", "job-card");
	const visual = create("div", "job-visual");
	const image = firstArtifact(detail);
	if (image) {
		const thumbnail = create("img");
		thumbnail.alt = detail?.request?.prompt
			? `Generated result for ${truncate(detail.request.prompt, 90)}`
			: "Generated image";
		visual.append(thumbnail);
		loadImage(thumbnail, image.id, 640).catch(() => {
			visual.replaceChildren(
				create("span", "job-placeholder", "Preview unavailable"),
			);
		});
	} else if (
		summary.status === "running" &&
		Number(summary.progress?.partial_images) > 0
	) {
		const partial = create("img");
		partial.alt = `Latest partial preview for ${truncate(detail?.request?.prompt, 90)}`;
		visual.append(partial);
		loadPartialImage(
			partial,
			summary.id,
			Number(summary.progress.partial_images),
		).catch(() => {
			visual.replaceChildren(
				create("span", "job-placeholder", "Partial preview unavailable"),
			);
		});
	} else {
		visual.append(
			create("span", "job-placeholder", placeholderText(summary, detail)),
		);
	}

	const body = create("div", "job-card-body");
	const top = create("div", "job-topline");
	const statusGroup = create("div", "status-group");
	const badge = create("span", "status", summary.status);
	badge.dataset.status = summary.status;
	statusGroup.append(badge);
	if (summary.favorite)
		statusGroup.append(create("span", "favorite-mark", "Favorite"));
	top.append(
		statusGroup,
		create("time", "job-time", formatTime(summary.created)),
	);

	const prompt = create(
		"p",
		"job-prompt",
		detail?.request?.prompt ||
			(detail
				? "Prompt unavailable"
				: state.detailFailures.has(summary.id)
					? "Details unavailable"
					: "Loading prompt"),
	);
	const metadata = create("div", "meta-line");
	metadata.append(
		create(
			"span",
			"",
			detail?.result?.provider ||
				detail?.request?.routing?.provider ||
				"default",
		),
		create(
			"span",
			"",
			detail?.result?.model || detail?.request?.routing?.model || "auto",
		),
	);
	if (detail?.result?.data?.length) {
		metadata.append(
			create(
				"span",
				"",
				`${detail.result.data.length} output${detail.result.data.length === 1 ? "" : "s"}`,
			),
		);
	}
	if (summary.progress?.stage) {
		metadata.append(
			create("span", "", summary.progress.stage.replaceAll("_", " ")),
		);
	}

	const actions = create("div", "job-actions");
	actions.append(
		makeButton("Details", "secondary compact", () => showJob(summary.id)),
	);
	actions.append(
		makeButton(
			summary.favorite ? "Unfavorite" : "Favorite",
			"secondary compact",
			() => updateJob(summary.id, { favorite: !summary.favorite }),
		),
	);
	if (summary.status === "queued" || summary.status === "running") {
		actions.append(
			makeButton("Cancel", "danger compact", () => cancelJob(summary.id)),
		);
	} else {
		actions.append(
			makeButton(
				summary.deleted == null ? "Hide" : "Restore",
				"secondary compact",
				() => updateJob(summary.id, { deleted: summary.deleted == null }),
			),
		);
	}

	body.append(top, prompt, metadata, actions);
	card.append(visual, body);
	return card;
}

function placeholderText(summary, detail) {
	if (!detail) return "Loading details";
	if (summary.status === "queued") return "Waiting for worker";
	if (summary.status === "running") {
		const stage = summary.progress?.stage?.replaceAll("_", " ");
		return stage ? `Working · ${stage}` : "Generation in progress";
	}
	if (summary.status === "failed") return "Generation failed";
	if (summary.status === "cancelled") return "Generation cancelled";
	if (summary.status === "interrupted") return "Generation interrupted";
	return "No image output";
}

function firstArtifact(job) {
	return job?.result?.data?.find(
		(image) => image.type === "artifact" && image.id,
	);
}

async function loadImage(element, artifactId, edge) {
	const key = `thumbnail:${artifactId}:${edge}`;
	let url = state.assetUrls.get(key);
	if (!url) {
		const blob = await api.thumbnail(artifactId, edge);
		url = URL.createObjectURL(blob);
		state.assetUrls.set(key, url);
	}
	element.src = url;
}

async function loadPartialImage(element, jobId, partialCount) {
	const key = `partial:${jobId}:${partialCount}`;
	let url = state.assetUrls.get(key);
	if (!url) {
		const blob = await api.jobPartial(jobId);
		url = URL.createObjectURL(blob);
		state.assetUrls.set(key, url);
	}
	element.src = url;
}

async function updateJob(id, update) {
	try {
		const detail = await api.updateJob(id, update);
		state.details.set(id, detail);
		replaceSummary(detail);
		renderJobs();
		if (elements.jobDialog.open) renderDetail(detail);
	} catch (error) {
		handleError(error);
	}
}

async function cancelJob(id) {
	const confirmed = await confirmAction({
		title: "Cancel this operation?",
		message:
			"Provider cancellation is best-effort. Paid work may already have completed upstream.",
		action: "Cancel operation",
	});
	if (!confirmed) return;
	try {
		const detail = await api.cancelJob(id);
		state.details.set(id, detail);
		replaceSummary(detail);
		renderJobs();
		schedulePoll(500);
	} catch (error) {
		handleError(error);
	}
}

function replaceSummary(job) {
	const index = state.summaries.findIndex((item) => item.id === job.id);
	if (index >= 0) state.summaries[index] = summaryFromJob(job);
}

function summaryFromJob(job) {
	const {
		request,
		result,
		error,
		cancel_requested: cancelRequested,
		...summary
	} = job;
	void request;
	void result;
	void error;
	void cancelRequested;
	return summary;
}

async function showJob(id) {
	elements.detailTitle.textContent = "Loading operation";
	setMessage(elements.detailMessage);
	elements.jobDetail.replaceChildren(create("div", "skeleton"));
	elements.jobDialog.showModal();
	try {
		const detail = state.details.get(id) || (await api.getJob(id));
		state.details.set(id, detail);
		state.detailFailures.delete(id);
		renderDetail(detail);
	} catch (error) {
		elements.jobDetail.replaceChildren(
			create("div", "error-panel", error.message),
		);
	}
}

function renderDetail(job) {
	elements.detailTitle.textContent = truncate(job.request.prompt, 72);
	elements.detailKicker.textContent = `${job.status} · ${job.id.slice(0, 8)}`;
	const layout = create("div", "detail-layout");
	const primary = create("div", "detail-section");
	const images = create("div", "detail-images");
	if (job.status === "running" && Number(job.progress?.partial_images) > 0) {
		const figure = create("figure", "detail-image partial-preview");
		const image = create("img");
		image.alt = `Latest partial preview after ${job.progress.partial_images} partial image events`;
		figure.append(
			image,
			create(
				"figcaption",
				"",
				`Live partial preview · ${job.progress.partial_images} received · not retained`,
			),
		);
		images.append(figure);
		loadPartialImage(image, job.id, job.progress.partial_images).catch(() => {
			figure.replaceChildren(
				create("span", "job-placeholder", "Partial preview unavailable"),
			);
		});
	}
	const outputs = job.result?.data || [];
	for (const output of outputs) {
		if (output.type !== "artifact" || !output.id) continue;
		const figure = create("figure", "detail-image");
		const open = create("button");
		open.type = "button";
		open.title = "Open full image";
		const image = create("img");
		image.alt = `Generated output ${output.index + 1}`;
		open.append(image);
		open.addEventListener("click", () => openArtifact(output.id));
		const caption = create("figcaption");
		const imageActions = create("span", "detail-image-actions");
		imageActions.append(
			makeButton("Copy folder", "secondary compact", () =>
				copyArtifactFolder(output),
			),
			makeButton("Download", "secondary compact detail-download", () =>
				downloadArtifact(output),
			),
		);
		caption.append(
			create(
				"span",
				"",
				`${output.width} x ${output.height} · ${output.format.toUpperCase()} · ${formatBytes(output.bytes)}`,
			),
			imageActions,
		);
		figure.append(open, caption);
		images.append(figure);
		loadImage(image, output.id, 1024).catch(() => {
			open.replaceChildren(
				create("span", "job-placeholder", "Preview unavailable"),
			);
		});
	}
	if (images.childElementCount > 0) primary.append(images);
	primary.append(detailSection("Prompt", job.request.prompt));
	if (job.request.negative_prompt)
		primary.append(
			detailSection("Negative prompt", job.request.negative_prompt),
		);
	if (job.result?.revised_prompt)
		primary.append(detailSection("Revised prompt", job.result.revised_prompt));
	if (job.error) {
		const error = create(
			"div",
			"error-panel",
			job.error.message || "The operation failed.",
		);
		error.setAttribute("role", "alert");
		primary.append(error);
	}
	if (job.result) {
		primary.append(parameterComparison(job.result));
		if (job.result.normalizations?.length)
			primary.append(
				messageList(
					"Applied normalizations",
					job.result.normalizations.map(
						(item) =>
							`${item.field}: ${displayValue(item.requested)} → ${displayValue(item.effective)} (${item.reason})`,
					),
				),
			);
		if (job.result.warnings?.length)
			primary.append(messageList("Warnings", job.result.warnings, "warning"));
		if (job.result.failures?.length)
			primary.append(
				messageList(
					"Output failures",
					job.result.failures.map(
						(item) =>
							`Output ${Number(item.index) + 1}: ${item.error?.message || item.error?.code || "generation failed"} (${formatDuration(item.generation_ms)})`,
					),
					"error",
				),
			);
	}

	const secondary = create("div", "detail-section");
	secondary.append(metadataList(job));
	const actions = create("div", "job-actions");
	actions.append(
		makeButton(
			job.favorite ? "Unfavorite" : "Favorite",
			"secondary compact",
			() => updateJob(job.id, { favorite: !job.favorite }),
		),
	);
	if (job.status === "queued" || job.status === "running") {
		actions.append(
			makeButton("Cancel", "danger compact", () => cancelJob(job.id)),
		);
	} else {
		actions.append(
			makeButton(
				job.deleted == null ? "Hide" : "Restore",
				"secondary compact",
				() => updateJob(job.id, { deleted: job.deleted == null }),
			),
		);
	}
	secondary.append(actions);

	const raw = create("details", "raw-details");
	raw.append(create("summary", "", "Raw metadata"));
	raw.append(create("pre", "raw-json", JSON.stringify(job, null, 2)));
	secondary.append(raw);
	layout.append(primary, secondary);
	elements.jobDetail.replaceChildren(layout);
}

function detailSection(title, content) {
	const section = create("section", "detail-section");
	section.append(create("h3", "", title), create("p", "detail-copy", content));
	return section;
}

function messageList(title, items, kind = "") {
	const section = create("section", "detail-section");
	section.append(create("h3", "", title));
	const list = create("ul", `detail-messages${kind ? ` ${kind}` : ""}`);
	for (const item of items) list.append(create("li", "", String(item)));
	section.append(list);
	return section;
}

function parameterComparison(result) {
	const section = create("section", "detail-section");
	section.append(create("h3", "", "Requested and effective parameters"));
	const scroll = create("div", "table-scroll");
	scroll.tabIndex = 0;
	scroll.setAttribute("aria-label", "Scrollable requested and effective parameter comparison");
	const table = create("table", "parameter-table");
	const head = create("thead");
	const heading = create("tr");
	for (const label of ["Parameter", "Requested", "Effective"])
		heading.append(create("th", "", label));
	for (const cell of heading.children) cell.scope = "col";
	head.append(heading);
	const body = create("tbody");
	const requested = result.requested || {};
	const effective = result.effective || {};
	for (const [field, label] of Object.entries(PARAMETER_LABELS)) {
		const row = create("tr");
		const changed =
			JSON.stringify(requested[field]) !== JSON.stringify(effective[field]);
		if (changed) row.className = "changed";
		row.append(
			create("th", "", label),
			create("td", "", displayValue(requested[field])),
			create("td", "", displayValue(effective[field])),
		);
		row.firstElementChild.scope = "row";
		body.append(row);
	}
	table.append(head, body);
	scroll.append(table);
	section.append(scroll);
	return section;
}

function displayValue(value) {
	if (value == null || value === "") return "—";
	if (typeof value === "object") return JSON.stringify(value);
	return String(value);
}

function metadataList(job) {
	const list = create("dl", "detail-meta");
	const result = job.result;
	const output = result?.data?.[0];
	const entries = [
		["Status", job.status],
		["Created", formatDate(job.created)],
		[
			"Provider",
			result?.provider || job.request.routing?.provider || "default",
		],
		["Model", result?.model || job.request.routing?.model || "auto"],
		["Operation", job.request.operation],
		["Progress", job.progress?.stage?.replaceAll("_", " ") || "complete"],
		["Cancel requested", job.cancel_requested ? "yes" : "no"],
		["Outputs", String(result?.data?.length ?? job.request.parameters?.n ?? 1)],
		[
			"Size",
			output
				? `${output.width} x ${output.height}`
				: job.request.parameters?.size || "auto",
		],
		[
			"Quality",
			result?.effective?.quality || job.request.parameters?.quality || "auto",
		],
		[
			"Format",
			output?.format || job.request.parameters?.output_format || "auto",
		],
		[
			"Queue time",
			result ? formatDuration(result.timings?.queue_ms) : "pending",
		],
		[
			"Input time",
			result ? formatDuration(result.timings?.input_ms) : "pending",
		],
		[
			"Total time",
			result ? formatDuration(result.timings?.total_ms) : "pending",
		],
		[
			"Provider time",
			result ? formatDuration(result.timings?.provider_ms) : "pending",
		],
		[
			"Artifact time",
			result ? formatDuration(result.timings?.artifact_ms) : "pending",
		],
		["Partial images", String(job.progress?.partial_images ?? 0)],
	];
	for (const [term, description] of entries) {
		const group = create("div");
		group.append(create("dt", "", term), create("dd", "", description));
		list.append(group);
	}
	return list;
}

async function openArtifact(id) {
	const tab = window.open("about:blank", "_blank");
	if (tab) tab.opener = null;
	try {
		const key = `artifact:${id}`;
		let url = state.assetUrls.get(key);
		if (!url) {
			const blob = await api.artifact(id);
			url = URL.createObjectURL(blob);
			state.assetUrls.set(key, url);
		}
		if (tab) tab.location.replace(url);
		else window.location.assign(url);
	} catch (error) {
		if (tab) tab.close();
		handleError(error);
	}
}

async function downloadArtifact(output) {
	try {
		const blob = await api.artifact(output.id);
		const url = URL.createObjectURL(blob);
		const fallback = `image-${Number(output.index ?? 0) + 1}.${output.format || "png"}`;
		const filename =
			(output.name || fallback).split(/[\\/]/).at(-1) || fallback;
		const link = document.createElement("a");
		link.href = url;
		link.download = filename;
		link.hidden = true;
		document.body.append(link);
		link.click();
		link.remove();
		window.setTimeout(() => URL.revokeObjectURL(url), 1000);
	} catch (error) {
		handleError(error);
	}
}

async function copyArtifactFolder(output) {
	const name = typeof output.name === "string" ? output.name : "";
	const parts = name.split("/").filter(Boolean);
	const folder = parts.length > 1 ? parts.slice(0, -1).join("/") : ".";
	try {
		await copyText(folder);
		setMessage(
			elements.detailMessage,
			`Copied portable output folder: ${folder}`,
			"ready",
		);
	} catch {
		setMessage(
			elements.detailMessage,
			`Could not copy the portable output folder. Folder: ${folder}`,
			"error",
		);
	}
}

async function copyText(value) {
	if (navigator.clipboard?.writeText) {
		try {
			await navigator.clipboard.writeText(value);
			return;
		} catch {
			// Fall through for browsers that expose but deny the async clipboard API.
		}
	}
	const input = create("textarea");
	input.value = value;
	input.readOnly = true;
	input.setAttribute("aria-hidden", "true");
	input.style.position = "fixed";
	input.style.opacity = "0";
	document.body.append(input);
	input.select();
	const copied = document.execCommand("copy");
	input.remove();
	if (!copied) throw new Error("clipboard unavailable");
}

async function submitGeneration(event) {
	event.preventDefault();
	elements.submit.disabled = true;
	setMessage(elements.formMessage, "Preparing request");
	try {
		const request = await buildRequest(elements.form);
		const job = await api.createJob(request);
		state.details.set(job.id, job);
		state.summaries.unshift(summaryFromJob(job));
		renderJobs();
		setMessage(
			elements.formMessage,
			"Operation queued. The gallery will update automatically.",
		);
		schedulePoll(400);
	} catch (error) {
		handleError(error, elements.formMessage);
	} finally {
		elements.submit.disabled = false;
	}
}

function schedulePoll(delay) {
	window.clearTimeout(state.pollTimer);
	const active = state.summaries.some(
		(job) => job.status === "queued" || job.status === "running",
	);
	const interval = delay ?? (active ? 2500 : 15000);
	state.pollTimer = window.setTimeout(async () => {
		if (document.visibilityState === "visible") await loadJobs({ quiet: true });
		else schedulePoll(5000);
	}, interval);
}

function truncate(value, length) {
	if (!value || value.length <= length) return value || "Operation";
	return `${value.slice(0, length - 1).trimEnd()}…`;
}

function formatTime(timestamp) {
	return new Intl.DateTimeFormat(undefined, {
		hour: "2-digit",
		minute: "2-digit",
	}).format(new Date(timestamp * 1000));
}

function formatDate(timestamp) {
	return new Intl.DateTimeFormat(undefined, {
		dateStyle: "medium",
		timeStyle: "short",
	}).format(new Date(timestamp * 1000));
}

function formatEventTime(timestamp) {
	if (!Number.isFinite(timestamp)) return "unknown";
	return new Intl.DateTimeFormat(undefined, {
		dateStyle: "short",
		timeStyle: "medium",
	}).format(new Date(timestamp));
}

function formatDuration(milliseconds) {
	if (!Number.isFinite(milliseconds)) return "unknown";
	if (milliseconds < 1000) return `${milliseconds} ms`;
	return `${(milliseconds / 1000).toFixed(milliseconds < 10000 ? 1 : 0)} s`;
}

function formatBytes(bytes) {
	if (!Number.isFinite(bytes)) return "unknown";
	if (bytes < 1024) return `${bytes} B`;
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function bindEvents() {
	elements.form.addEventListener("submit", submitGeneration);
	elements.operation.addEventListener("change", () =>
		updateOperationFields(elements.operation.value),
	);
	elements.sessionMode.addEventListener("change", () =>
		updateSessionFields(elements.sessionMode.value),
	);
	elements.provider.addEventListener("change", loadCapabilities);
	elements.model.addEventListener("change", loadCapabilities);
	elements.refresh.addEventListener("click", () => loadJobs());
	elements.loadMore.addEventListener("click", () => loadJobs({ append: true }));
	elements.statusFilter.addEventListener("change", () => loadJobs());
	elements.deletedFilter.addEventListener("change", () => loadJobs());
	elements.favoritesFilter.addEventListener("change", () => loadJobs());
	elements.searchFilter.addEventListener("input", () => {
		window.clearTimeout(state.searchTimer);
		state.searchTimer = window.setTimeout(() => loadJobs(), 300);
	});
	elements.connectionButton.addEventListener("click", showAuthentication);
	elements.operatorButton.addEventListener("click", () => {
		elements.operatorDialog.showModal();
		elements.closeOperator.focus();
		loadOperator();
	});
	elements.refreshOperator.addEventListener("click", loadOperator);
	elements.closeOperator.addEventListener("click", () =>
		elements.operatorDialog.close(),
	);
	elements.operatorDialog.addEventListener("click", (event) => {
		if (event.target === elements.operatorDialog)
			elements.operatorDialog.close();
	});
	elements.sessionForm.addEventListener("submit", inspectSession);
	elements.clearToken.addEventListener("click", () => {
		api.setToken("");
		elements.token.value = "";
		state.authenticationPrompted = false;
		loadProviders()
			.then(() => loadJobs())
			.catch((error) => handleError(error));
	});
	elements.connectionDialog.addEventListener("submit", (event) => {
		if (event.submitter !== elements.saveToken) return;
		event.preventDefault();
		api.setToken(elements.token.value);
		elements.connectionDialog.close();
		state.authenticationPrompted = false;
		Promise.all([loadProviders(), loadJobs()]).catch((error) =>
			handleError(error),
		);
	});
	elements.closeDetail.addEventListener("click", () =>
		elements.jobDialog.close(),
	);
	elements.jobDialog.addEventListener("click", (event) => {
		if (event.target === elements.jobDialog) elements.jobDialog.close();
	});
	document.addEventListener("visibilitychange", () => {
		if (document.visibilityState === "visible") schedulePoll(250);
	});
	window.addEventListener("beforeunload", () => {
		for (const url of state.assetUrls.values()) URL.revokeObjectURL(url);
	});
}

async function initialize() {
	bindEvents();
	updateOperationFields(elements.operation.value);
	updateSessionFields(elements.sessionMode.value);
	showSkeletons();
	try {
		await Promise.all([loadProviders(), loadJobs({ quiet: true })]);
	} catch (error) {
		handleError(error);
	}
}

initialize();
