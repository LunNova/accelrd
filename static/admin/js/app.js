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

	function setStatusTag(id, ok, label) {
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
			setStatusTag("status-k8s", h.k8s.ok, "k8s: " + (h.k8s.ok ? "ok" : "down"));
			setStatusTag("status-mutel", h.mutel.ok, "mutel: " + (h.mutel.ok ? "ok" : "down"));
			$("#footer-version").textContent = `accelrd-admin ${h.version}`;
			$("#footer-uptime").textContent = `uptime ${formatDuration(h.uptime_secs)}`;
			$("#footer-mutel").textContent = `mutel ${h.mutel_endpoint}`;

			const banners = [];
			if (!h.k8s.ok) banners.push(`k8s unavailable: ${h.k8s.error || "unknown"}`);
			if (!h.mutel.ok) banners.push(`mutel unavailable: ${h.mutel.error || "unknown"}`);
			showBanner(banners.join(" · ") || null);
		} catch (e) {
			setStatusTag("status-k8s", null, "k8s: ?");
			setStatusTag("status-mutel", null, "mutel: ?");
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

	function verdictTag(v, source) {
		if (!v) return `<span class="tag">untested</span>`;
		const cls = (v === "pass" || v === "ok") ? "ok"
			: (v === "fail" || v === "error") ? "err"
			: "warn";
		// Two cases that get rendered very differently:
		//  - preflight tier: the node has no RDMA hardware and no rxe,
		//    so no fabric test ever ran. We collapse "ok · preflight"
		//    to "no rdma" — the preflight outcome wasn't measuring
		//    RDMA, it was measuring "is this GPU node otherwise sane",
		//    so showing "ok" here would imply a fabric test. Preserve
		//    fail/warn distinctly so a degraded preflight still pops.
		//  - pair / loopback tier: a real ibverbs probe ran. Tag the
		//    pill with `· rdma` so it's visibly distinct from the
		//    preflight-only nodes at a glance.
		let text;
		if (source === "preflight") {
			text = (v === "ok" || v === "pass") ? "no rdma" : `${v} · no rdma`;
		} else {
			text = `${v} · rdma`;
		}
		return `<span class="tag ${cls}">${escapeHTML(text)}</span>`;
	}

	function isFailVerdict(v) {
		return v === "fail" || v === "error";
	}

	/// Build an eBay search query from the most populous accelerator on a
	/// node. Falls back through model → vendor → generic so the link never
	/// points at an empty search.
	function scavengeQuery(node) {
		const models = node.model_counts || {};
		const modelKeys = Object.keys(models);
		if (modelKeys.length > 0) {
			modelKeys.sort((a, b) => (models[b] || 0) - (models[a] || 0));
			return modelKeys[0];
		}
		const vendors = Object.keys(node.vendor_counts || {});
		if (vendors.length > 0) return vendors.sort().join(" ") + " gpu";
		return "datacenter accelerator";
	}

	function scavengeLink(node) {
		const q = scavengeQuery(node);
		// _sop=15 sorts by price + shipping ascending — scavenger's choice.
		const url = `https://www.ebay.com/sch/i.html?_nkw=${encodeURIComponent(q)}&_sop=15`;
		const tip = `node failed — search eBay for "${q}"`;
		return `<a class="scavenge-btn" href="${escapeHTML(url)}" target="_blank" rel="noopener noreferrer" title="${escapeHTML(tip)}">scavenge ↗</a>`;
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
		const strip = $("#cluster-strip");
		try {
			const [data, fleet] = await Promise.all([loadCluster(force), loadFleet(force)]);
			const { nodes, summary } = data;

			strip.innerHTML = renderStatusStrip(summary, fleet);

			if (nodes.length === 0) {
				tbody.innerHTML = `<tr><td colspan="13" class="placeholder">No nodes (k8s disabled or empty cluster)</td></tr>`;
			} else {
				tbody.innerHTML = nodes.map((n) => nodeRow(n, fleet.by_node && fleet.by_node[n.name])).join("");
			}
		} catch (e) {
			tbody.innerHTML = `<tr><td colspan="13" class="placeholder">${escapeHTML(e.message)}</td></tr>`;
		}
	}

	function renderStatusStrip(summary, fleet) {
		const cells = [
			stat("nodes", `${summary.nodes_ready}<span class="dim">/${summary.nodes_total}</span>`),
			stat("racks", String(Object.keys(summary.racks).length)),
			stat("accel", fmtVendorTotals(summary.vendor_totals)),
			stat("verdict", fmtVerdictMixCompact(summary.probe_verdicts)),
		];
		if (!fleet || !fleet.available) {
			cells.push(stat("fleet", `<span class="verdict-fail">${escapeHTML(fleet && fleet.error ? truncate(fleet.error, 28) : "unavailable")}</span>`));
			return cells.join("");
		}
		const f = fleet.fleet || {};
		cells.push(
			stat("memory", memPair(f.mem_used_bytes, f.mem_total_bytes)),
			stat("ram avl", fmtBytesShort(f.ram_available_bytes)),
			stat("disk free", fmtBytesShort(f.disk_free_bytes)),
			stat("power", fmtPower(f.power_watts || 0)),
			stat("temp max", tempCell(f.max_temp_c)),
			stat("util avg", fmtPercent(f.avg_utilization)),
		);
		return cells.join("");
	}

	function stat(label, value) {
		return `<div class="stat"><div class="stat-label">${escapeHTML(label)}</div><div class="stat-value">${value}</div></div>`;
	}

	function memPair(used, total) {
		if (!total) return "—";
		return `${fmtBytesShort(used || 0)}<span class="dim">/${fmtBytesShort(total)}</span>`;
	}

	function tempCell(c) {
		if (c == null) return "—";
		const cls = c >= 90 ? "verdict-fail" : c >= 80 ? "verdict-untested" : "verdict-pass";
		return `<span class="${cls}">${c.toFixed(0)}°C</span>`;
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

	/// Single-letter byte units for the status strip ("23G", "1.2T") —
	/// terminal-style density. fmtBytes still renders the spaced "23 GiB"
	/// form for the table cells where there's room.
	function fmtBytesShort(n) {
		if (n == null || !isFinite(n) || n === 0) return "—";
		const units = ["B", "K", "M", "G", "T", "P"];
		let i = 0;
		let v = Math.abs(n);
		while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
		const sign = n < 0 ? "-" : "";
		const fixed = v >= 100 ? 0 : v >= 10 ? 0 : 1;
		return `${sign}${v.toFixed(fixed)}${units[i]}`;
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

	function fmtVendorTotals(totals) {
		const keys = Object.keys(totals || {}).sort();
		if (keys.length === 0) return "—";
		return keys.map((k) => `${totals[k]} ${escapeHTML(k)}`).join(" ");
	}

	/// Color-coded compact verdict roll-up: "10P 1F 2U" with green/red/
	/// amber spans. Empty buckets are dropped so an all-pass cluster
	/// reads as just "12P".
	function fmtVerdictMixCompact(verdicts) {
		const groups = { P: 0, F: 0, U: 0 };
		const cls = { P: "verdict-pass", F: "verdict-fail", U: "verdict-untested" };
		for (const [k, v] of Object.entries(verdicts || {})) {
			const key = (k === "pass" || k === "ok") ? "P"
				: (k === "fail" || k === "error") ? "F" : "U";
			groups[key] += v;
		}
		const parts = [];
		for (const k of ["P", "F", "U"]) {
			if (groups[k] > 0) parts.push(`<span class="${cls[k]}">${groups[k]}${k}</span>`);
		}
		return parts.length ? parts.join(" ") : "—";
	}

	function nodeRow(n, live) {
		const status = n.ready
			? `<span class="tag ok">ready</span>`
			: `<span class="tag err">not-ready</span>`;
		const sched = n.schedulable ? "" : ` <span class="tag warn">cordoned</span>`;
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
			<td class="mono">${memCell(m.mem_used_bytes, m.mem_total_bytes)}</td>
			<td class="mono">${memCell(m.ram_available_bytes, m.ram_total_bytes, "/")}</td>
			<td class="mono">${memCell(m.disk_free_bytes, m.disk_total_bytes, "/")}</td>
			<td class="mono">${fmtPower(m.power_watts)}</td>
			<td class="mono ${tempClass}">${m.temp_c == null ? "—" : `${m.temp_c.toFixed(0)}°C`}</td>
			<td class="mono">${fmtPercent(m.utilization)}</td>
			<td>${escapeHTML(fmtRelative(probe.at))}</td>
			<td>${verdictTag(probe.verdict, probe.source)}${isFailVerdict(probe.verdict) ? scavengeLink(n) : ""}</td>
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
				<td>${verdictTag(p.verdict, p.source)}</td>
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

	let activeView = "cluster";

	function activate(view) {
		activeView = view;
		$$(".view").forEach((v) => v.classList.toggle("active", v.id === `${view}-view`));
		$$(".nav-link").forEach((a) => a.classList.toggle("active", a.dataset.view === view));
		// Show only the navbar command-slot bits relevant to this view.
		$$(".nav-cmd .cmd-logs, .nav-cmd .cmd-topology").forEach((el) => {
			el.classList.toggle("hidden", !el.classList.contains(`cmd-${view}`));
		});
		runActiveView(false);
	}

	function runActiveView(force) {
		switch (activeView) {
			case "cluster": renderCluster(force); break;
			case "topology": renderTopology(); break;
			case "probes": renderProbes(force); break;
			case "logs": renderLogs(); break;
		}
		stampUpdated();
	}

	function stampUpdated() {
		const el = $("#cmd-updated");
		if (!el) return;
		const t = new Date();
		const hh = String(t.getHours()).padStart(2, "0");
		const mm = String(t.getMinutes()).padStart(2, "0");
		const ss = String(t.getSeconds()).padStart(2, "0");
		el.textContent = `upd ${hh}:${mm}:${ss}`;
	}

	function bind() {
		$$(".nav-link").forEach((a) => {
			a.addEventListener("click", (e) => {
				e.preventDefault();
				activate(a.dataset.view);
			});
		});
		$("#global-refresh").addEventListener("click", () => runActiveView(true));
		$("#log-service").addEventListener("keydown", (e) => {
			if (e.key === "Enter") runActiveView(true);
		});
	}

	bind();
	refreshHealth();
	activate("cluster");
	setInterval(refreshHealth, 5000);
})();
