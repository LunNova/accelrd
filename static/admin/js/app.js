// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0
//
// Single-file SPA glue. No framework — just direct fetch + DOM. The
// server only emits JSON, so this file owns all rendering. Each
// view's render fn is idempotent so we can re-render on tab switch
// without leaking handlers.

(() => {
	const state = {
		clusterCache: null,
		clusterFetchedAt: 0,
		fleetCache: null,
		fleetFetchedAt: 0,
		probesCache: null,
	};

	const $ = (sel) => document.querySelector(sel);
	const $$ = (sel) => Array.from(document.querySelectorAll(sel));

	function setStatusPill(id, ok, label) {
		const el = document.getElementById(id);
		if (!el) return;
		el.textContent = label;
		el.classList.toggle("ok", ok === true);
		el.classList.toggle("err", ok === false);
	}

	function showBanner(msg) {
		const b = document.getElementById("banner");
		if (!msg) { b.classList.add("hidden"); b.textContent = ""; return; }
		b.classList.remove("hidden");
		b.textContent = msg;
	}

	async function fetchJSON(url) {
		const r = await fetch(url, { headers: { "Accept": "application/json" } });
		if (!r.ok) {
			let body;
			try { body = await r.json(); } catch { body = { error: { message: r.statusText } }; }
			const msg = body && body.error ? body.error.message : r.statusText;
			throw new Error(`${url}: ${msg}`);
		}
		return r.json();
	}

	async function refreshHealth() {
		try {
			const h = await fetchJSON("/api/health");
			setStatusPill("status-k8s", h.k8s.ok, "k8s: " + (h.k8s.ok ? "ok" : "down"));
			setStatusPill("status-mutel", h.mutel.ok, "mutel: " + (h.mutel.ok ? "ok" : "down"));
			$("#footer-version").textContent = `accelrd-admin ${h.version}`;
			$("#footer-uptime").textContent = `uptime ${formatDuration(h.uptime_secs)}`;
			$("#footer-mutel").textContent = `mutel ${h.mutel_endpoint}`;

			const banners = [];
			if (!h.k8s.ok) banners.push(`k8s unavailable: ${h.k8s.error || "unknown"}`);
			if (!h.mutel.ok) banners.push(`mutel unavailable: ${h.mutel.error || "unknown"}`);
			showBanner(banners.join(" · ") || null);
		} catch (e) {
			setStatusPill("status-k8s", null, "k8s: ?");
			setStatusPill("status-mutel", null, "mutel: ?");
			showBanner(`admin self-check failed: ${e.message}`);
		}
	}

	function formatDuration(secs) {
		secs = Math.floor(secs);
		if (secs < 60) return `${secs}s`;
		const m = Math.floor(secs / 60);
		if (m < 60) return `${m}m`;
		const h = Math.floor(m / 60);
		const mr = m % 60;
		if (h < 24) return `${h}h${mr}m`;
		const d = Math.floor(h / 24);
		return `${d}d${h % 24}h`;
	}

	function fmtNum(n) {
		if (n === null || n === undefined) return "—";
		if (typeof n !== "number") return String(n);
		if (Math.abs(n) >= 1000) return n.toLocaleString();
		return n.toString();
	}

	function fmtRelative(iso) {
		if (!iso) return "—";
		const t = Date.parse(iso);
		if (isNaN(t)) return iso;
		const delta = (Date.now() - t) / 1000;
		if (delta < 0) return "soon";
		if (delta < 60) return `${Math.floor(delta)}s ago`;
		if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
		if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
		return `${Math.floor(delta / 86400)}d ago`;
	}

	function verdictPill(v) {
		if (!v) return `<span class="pill">untested</span>`;
		const cls = (v === "pass" || v === "ok") ? "ok"
			: (v === "fail" || v === "error") ? "err"
			: "warn";
		return `<span class="pill ${cls}">${escapeHTML(v)}</span>`;
	}

	function escapeHTML(s) {
		return String(s).replace(/[&<>"']/g, (c) => ({
			"&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;"
		}[c]));
	}

	function fmtVendors(counts) {
		const keys = Object.keys(counts || {});
		if (keys.length === 0) return "—";
		return keys.sort().map((k) => `${k}:${counts[k]}`).join(" ");
	}

	async function loadCluster(force) {
		const stale = Date.now() - state.clusterFetchedAt > 10000;
		if (!force && state.clusterCache && !stale) return state.clusterCache;
		const data = await fetchJSON("/api/cluster");
		state.clusterCache = data;
		state.clusterFetchedAt = Date.now();
		return data;
	}

	async function loadFleet(force) {
		const stale = Date.now() - state.fleetFetchedAt > 10000;
		if (!force && state.fleetCache && !stale) return state.fleetCache;
		try {
			const data = await fetchJSON("/api/fleet");
			state.fleetCache = data;
			state.fleetFetchedAt = Date.now();
			return data;
		} catch (e) {
			// Fleet failures are surfaced inline and don't poison the
			// cluster view — return an "unavailable" stub.
			return { available: false, error: e.message, by_node: {}, fleet: {} };
		}
	}

	async function renderCluster(force) {
		const tbody = $("#nodes-table tbody");
		const summaryRow = $("#cluster-summary");
		const fleetRow = $("#fleet-summary");
		try {
			const [data, fleet] = await Promise.all([loadCluster(force), loadFleet(force)]);
			const { nodes, summary } = data;

			summaryRow.innerHTML = [
				summaryCard("Nodes", `${summary.nodes_ready} / ${summary.nodes_total}`, "ready / total"),
				summaryCard("Racks", String(Object.keys(summary.racks).length), "with topology"),
				summaryCard("Accelerators", fmtVendorTotals(summary.vendor_totals), "by vendor"),
				summaryCard("Probe verdicts", fmtVerdictMix(summary.probe_verdicts), "last per node"),
			].join("");

			fleetRow.innerHTML = renderFleetCards(fleet);

			if (nodes.length === 0) {
				tbody.innerHTML = `<tr><td colspan="14" class="placeholder">No nodes (k8s disabled or empty cluster)</td></tr>`;
			} else {
				tbody.innerHTML = nodes.map((n) => nodeRow(n, fleet.by_node && fleet.by_node[n.name])).join("");
			}
		} catch (e) {
			tbody.innerHTML = `<tr><td colspan="14" class="placeholder">${escapeHTML(e.message)}</td></tr>`;
		}
	}

	function renderFleetCards(fleet) {
		if (!fleet || !fleet.available) {
			return summaryCard("Live fleet", "—",
				fleet && fleet.error ? `mutel: ${truncate(fleet.error, 60)}` : "mutel unavailable");
		}
		const f = fleet.fleet || {};
		const vramSub = f.vram_total_bytes > 0
			? `${fmtBytes(f.vram_used_bytes || 0)} / ${fmtBytes(f.vram_total_bytes)} discrete`
			: "no discrete cards";
		const umaSub = f.uma_total_bytes > 0
			? `${fmtBytes(f.uma_used_bytes || 0)} / ${fmtBytes(f.uma_total_bytes)} unified pool`
			: "no APUs / iGPUs";
		const ramSub = f.ram_total_bytes > 0
			? `${fmtBytes(f.ram_available_bytes || 0)} / ${fmtBytes(f.ram_total_bytes)} available`
			: "host RAM unknown";
		return [
			summaryCard("VRAM (dedicated)", fmtBytes(f.vram_total_bytes || 0), vramSub),
			summaryCard("UMA (unified)", fmtBytes(f.uma_total_bytes || 0), umaSub),
			summaryCard("RAM available", fmtBytes(f.ram_available_bytes || 0), ramSub),
			summaryCard("Disk free", fmtBytes(f.disk_free_bytes || 0), "sum of largest mounts"),
			summaryCard("Fleet power", fmtPower(f.power_watts || 0), `over ${f.nodes_with_data || 0} nodes`),
			summaryCard("Hottest accelerator", f.max_temp_c == null ? "—" : `${f.max_temp_c.toFixed(0)}°C`, "max across fleet"),
			summaryCard("Avg utilization", f.avg_utilization == null ? "—" : `${f.avg_utilization.toFixed(0)}%`, "mean per-node avg"),
		].join("");
	}

	function fmtBytes(n) {
		if (n == null || !isFinite(n) || n === 0) return "—";
		const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
		let i = 0;
		let v = Math.abs(n);
		while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
		const sign = n < 0 ? "-" : "";
		return `${sign}${v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2)} ${units[i]}`;
	}

	function fmtPower(w) {
		if (w == null || !isFinite(w) || w === 0) return "—";
		if (w >= 1000) return `${(w / 1000).toFixed(2)} kW`;
		return `${w.toFixed(0)} W`;
	}

	function fmtPercent(p) {
		if (p == null || !isFinite(p)) return "—";
		return `${p.toFixed(0)}%`;
	}

	function truncate(s, n) {
		s = String(s || "");
		return s.length <= n ? s : s.slice(0, n - 1) + "…";
	}

	function summaryCard(label, value, sub) {
		return `<div class="summary-card">
			<div class="label">${escapeHTML(label)}</div>
			<div class="value">${value}</div>
			<div class="sub">${escapeHTML(sub || "")}</div>
		</div>`;
	}

	function fmtVendorTotals(totals) {
		const keys = Object.keys(totals || {}).sort();
		if (keys.length === 0) return "—";
		return keys.map((k) => `${totals[k]} ${k}`).join(" · ");
	}

	function fmtVerdictMix(verdicts) {
		const keys = Object.keys(verdicts || {});
		if (keys.length === 0) return "—";
		return keys.sort().map((k) => `${verdicts[k]} ${k}`).join(" · ");
	}

	function nodeRow(n, live) {
		const status = n.ready
			? `<span class="pill ok">ready</span>`
			: `<span class="pill err">not-ready</span>`;
		const sched = n.schedulable ? "" : ` <span class="pill warn">cordoned</span>`;
		const probe = n.last_probe || {};
		const m = live || {};
		const tempClass = m.temp_c == null ? ""
			: m.temp_c >= 90 ? "verdict-fail"
			: m.temp_c >= 80 ? "verdict-untested" /* warning hue */
			: "verdict-pass";
		return `<tr>
			<td class="mono">${escapeHTML(n.name)}</td>
			<td>${escapeHTML(n.rack || "—")}</td>
			<td>${fmtNum(n.total_accelerators)}</td>
			<td class="mono">${escapeHTML(fmtVendors(n.vendor_counts))}</td>
			<td class="mono">${memCell(m.vram_used_bytes, m.vram_total_bytes)}</td>
			<td class="mono">${memCell(m.uma_used_bytes, m.uma_total_bytes)}</td>
			<td class="mono">${memCell(m.ram_available_bytes, m.ram_total_bytes, "/")}</td>
			<td class="mono">${memCell(m.disk_free_bytes, m.disk_total_bytes, "/")}</td>
			<td class="mono">${fmtPower(m.power_watts)}</td>
			<td class="mono ${tempClass}">${m.temp_c == null ? "—" : `${m.temp_c.toFixed(0)}°C`}</td>
			<td class="mono">${fmtPercent(m.utilization)}</td>
			<td>${escapeHTML(fmtRelative(probe.at))}</td>
			<td>${verdictPill(probe.verdict)}</td>
			<td>${status}${sched}</td>
		</tr>`;
	}

	/// "primary / total" cell. Returns "—" when both are missing; the
	/// total goes in the dim styling so the primary number reads cleanly.
	function memCell(primary, total) {
		if (primary == null && (total == null || !total)) return "—";
		if (total == null || !total) return fmtBytes(primary);
		return `${fmtBytes(primary || 0)} <span class="dim">/ ${fmtBytes(total)}</span>`;
	}

	async function renderProbes(force) {
		const tbody = $("#probes-table tbody");
		try {
			const data = await fetchJSON("/api/probes");
			if (!data.probes || data.probes.length === 0) {
				tbody.innerHTML = `<tr><td colspan="6" class="placeholder">No probe results yet</td></tr>`;
				return;
			}
			tbody.innerHTML = data.probes.map((p) => `<tr>
				<td>${escapeHTML(fmtRelative(p.at))}</td>
				<td class="mono">${escapeHTML(p.node)}</td>
				<td class="mono">${escapeHTML(p.partner || "—")}</td>
				<td>${escapeHTML(p.rack || "—")}</td>
				<td class="mono">${p.bandwidth_gbps == null ? "—" : p.bandwidth_gbps.toFixed(2)}</td>
				<td>${verdictPill(p.verdict)}</td>
			</tr>`).join("");
		} catch (e) {
			tbody.innerHTML = `<tr><td colspan="6" class="placeholder">${escapeHTML(e.message)}</td></tr>`;
		}
	}

	async function renderTopology() {
		const canvas = $("#topology-canvas");
		try {
			const [data, fleet] = await Promise.all([loadCluster(false), loadFleet(false)]);
			const svg = window.AccelrdTopology.render(data, fleet);
			canvas.innerHTML = "";
			canvas.appendChild(svg);
		} catch (e) {
			canvas.innerHTML = `<p class="placeholder">${escapeHTML(e.message)}</p>`;
		}
	}

	async function renderLogs() {
		const entries = $("#log-entries");
		const filter = $("#log-service").value.trim();
		const url = "/api/logs/recent?limit=200" + (filter ? `&service=${encodeURIComponent(filter)}` : "");
		try {
			const data = await fetchJSON(url);
			const logs = (data && data.logs) || [];
			if (logs.length === 0) {
				entries.innerHTML = `<p class="placeholder">No logs</p>`;
				return;
			}
			entries.innerHTML = logs.map((l) => {
				const ts = l.timestamp_ns ? new Date(Number(l.timestamp_ns) / 1e6).toISOString().replace("T", " ").replace("Z", "") : "—";
				const lvl = (l.level || "info").toLowerCase();
				const svc = l.service_name ? `[${l.service_name}] ` : "";
				return `<div class="log-entry">
					<span class="ts">${escapeHTML(ts)}</span>
					<span class="lvl ${lvl}">${escapeHTML(lvl.toUpperCase())}</span>
					<span class="body">${escapeHTML(svc + (l.body || ""))}</span>
				</div>`;
			}).join("");
		} catch (e) {
			entries.innerHTML = `<p class="placeholder">${escapeHTML(e.message)}</p>`;
		}
	}

	function activate(view) {
		$$(".view").forEach((v) => v.classList.toggle("active", v.id === `${view}-view`));
		$$(".nav-link").forEach((a) => a.classList.toggle("active", a.dataset.view === view));
		switch (view) {
			case "cluster": renderCluster(false); break;
			case "topology": renderTopology(); break;
			case "probes": renderProbes(false); break;
			case "logs": renderLogs(); break;
		}
	}

	function bind() {
		$$(".nav-link").forEach((a) => {
			a.addEventListener("click", (e) => {
				e.preventDefault();
				activate(a.dataset.view);
			});
		});
		$("#cluster-refresh").addEventListener("click", () => renderCluster(true));
		$("#probes-refresh").addEventListener("click", () => renderProbes(true));
		$("#logs-refresh").addEventListener("click", () => renderLogs());
		$("#log-service").addEventListener("keydown", (e) => {
			if (e.key === "Enter") renderLogs();
		});
	}

	bind();
	refreshHealth();
	activate("cluster");
	setInterval(refreshHealth, 5000);
})();
