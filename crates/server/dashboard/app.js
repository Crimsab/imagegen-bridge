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
	loadSequence: 0,
	authenticationPrompted: false,
};

const elements = {
	form: document.querySelector("#generation-form"),
	formMessage: document.querySelector("#form-message"),
	submit: document.querySelector("#submit-button"),
	operation: document.querySelector("#operation"),
	sessionMode: document.querySelector("#session-mode"),
	provider: document.querySelector("#provider"),
	model: document.querySelector("#model"),
	providerNote: document.querySelector("#provider-note"),
	grid: document.querySelector("#job-grid"),
	libraryMessage: document.querySelector("#library-message"),
	statusFilter: document.querySelector("#status-filter"),
	favoritesFilter: document.querySelector("#favorites-filter"),
	deletedFilter: document.querySelector("#deleted-filter"),
	loadMore: document.querySelector("#load-more-button"),
	refresh: document.querySelector("#refresh-button"),
	connectionStatus: document.querySelector("#connection-status"),
	connectionButton: document.querySelector("#connection-button"),
	connectionDialog: document.querySelector("#connection-dialog"),
	token: document.querySelector("#bearer-token"),
	saveToken: document.querySelector("#save-token-button"),
	clearToken: document.querySelector("#clear-token-button"),
	jobDialog: document.querySelector("#job-dialog"),
	jobDetail: document.querySelector("#job-detail"),
	detailTitle: document.querySelector("#detail-title"),
	detailKicker: document.querySelector("#detail-kicker"),
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
	elements.provider.replaceChildren(...options);
	elements.provider.value = current;
	setConnection("Connected", "ready");
	state.authenticationPrompted = false;
	await loadCapabilities();
}

async function loadCapabilities() {
	const provider = elements.provider.value;
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
			includeDeleted: elements.deletedFilter.checked,
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
	return state.summaries.filter((summary) => {
		if (elements.favoritesFilter.checked && !summary.favorite) return false;
		if (!elements.deletedFilter.checked && summary.deleted != null)
			return false;
		if (elements.deletedFilter.checked && summary.deleted == null) return false;
		return true;
	});
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
		const caption = create(
			"figcaption",
			"",
			`${output.width} x ${output.height} · ${output.format.toUpperCase()} · ${formatBytes(output.bytes)}`,
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
		primary.append(
			create(
				"div",
				"error-panel",
				job.error.message || "The operation failed.",
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
			"Total time",
			result ? formatDuration(result.timings?.total_ms) : "pending",
		],
		[
			"Provider time",
			result ? formatDuration(result.timings?.provider_ms) : "pending",
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
	elements.favoritesFilter.addEventListener("change", () => {
		renderJobs();
		setMessage(
			elements.libraryMessage,
			`${visibleSummaries().length} jobs shown`,
		);
	});
	elements.connectionButton.addEventListener("click", showAuthentication);
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
