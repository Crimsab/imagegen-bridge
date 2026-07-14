const TOKEN_KEY = "imagegen-bridge.dashboard.bearer";

export class BridgeApiError extends Error {
	constructor(message, status, payload) {
		super(message);
		this.name = "BridgeApiError";
		this.status = status;
		this.payload = payload;
	}

	get authenticationRequired() {
		return this.status === 401;
	}
}

export class BridgeApi {
	constructor() {
		this.token = sessionStorage.getItem(TOKEN_KEY) || "";
	}

	setToken(token) {
		this.token = token.trim();
		if (this.token) {
			sessionStorage.setItem(TOKEN_KEY, this.token);
		} else {
			sessionStorage.removeItem(TOKEN_KEY);
		}
	}

	async providers() {
		return this.request("/v1/providers?limit=100");
	}

	async diagnostics() {
		return this.request("/v1/diagnostics");
	}

	async capabilities(provider, model = "") {
		const modelQuery = model ? `?model=${encodeURIComponent(model)}` : "";
		return this.request(
			`/v1/providers/${encodeURIComponent(provider)}/capabilities${modelQuery}`,
		);
	}

	async createJob(request) {
		return this.request("/v1/jobs", {
			method: "POST",
			body: JSON.stringify(request),
		});
	}

	async listJobs({
		cursor = "",
		status = "",
		visibility = "active",
		favorite = false,
		search = "",
		limit = 24,
	} = {}) {
		const query = new URLSearchParams({ limit: String(limit) });
		if (cursor) query.set("cursor", cursor);
		if (status) query.set("status", status);
		if (visibility) query.set("visibility", visibility);
		if (favorite) query.set("favorite", "true");
		if (search.trim()) query.set("search", search.trim());
		return this.request(`/v1/jobs?${query}`);
	}

	async getJob(id) {
		return this.request(`/v1/jobs/${encodeURIComponent(id)}`);
	}

	async getSession(key, provider = "") {
		const query = provider ? `?provider=${encodeURIComponent(provider)}` : "";
		return this.request(`/v1/sessions/${encodeURIComponent(key)}${query}`);
	}

	async deleteSession(key, provider = "") {
		const query = provider ? `?provider=${encodeURIComponent(provider)}` : "";
		return this.request(`/v1/sessions/${encodeURIComponent(key)}${query}`, {
			method: "DELETE",
		});
	}

	async updateJob(id, update) {
		return this.request(`/v1/jobs/${encodeURIComponent(id)}`, {
			method: "PATCH",
			body: JSON.stringify(update),
		});
	}

	async cancelJob(id) {
		return this.request(`/v1/jobs/${encodeURIComponent(id)}`, {
			method: "DELETE",
		});
	}

	async jobPartial(id) {
		return this.requestBlob(`/v1/jobs/${encodeURIComponent(id)}/partial`);
	}

	async thumbnail(id, edge = 640) {
		return this.requestBlob(
			`/v1/artifacts/${encodeURIComponent(id)}/thumbnail?edge=${encodeURIComponent(edge)}`,
		);
	}

	async artifact(id) {
		return this.requestBlob(`/v1/artifacts/${encodeURIComponent(id)}`);
	}

	async request(path, options = {}) {
		const response = await fetch(path, this.options(options));
		const payload = await this.readPayload(response);
		if (!response.ok) {
			throw this.error(response, payload);
		}
		return payload;
	}

	async requestBlob(path) {
		const response = await fetch(path, this.options());
		if (!response.ok) {
			const payload = await this.readPayload(response);
			throw this.error(response, payload);
		}
		return response.blob();
	}

	options(options = {}) {
		const headers = new Headers(options.headers || {});
		headers.set("Accept", "application/json");
		if (options.body) headers.set("Content-Type", "application/json");
		if (this.token) headers.set("Authorization", `Bearer ${this.token}`);
		return { ...options, headers };
	}

	async readPayload(response) {
		if (response.status === 204) return null;
		const type = response.headers.get("content-type") || "";
		if (type.includes("application/json")) {
			return response.json().catch(() => null);
		}
		return response.text().catch(() => "");
	}

	error(response, payload) {
		const message =
			payload?.error?.message ||
			payload?.message ||
			(typeof payload === "string" && payload) ||
			`Bridge request failed with HTTP ${response.status}`;
		return new BridgeApiError(message, response.status, payload);
	}
}
