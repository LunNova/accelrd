// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0
//
// Topology renderer: lays out racks → nodes → accelerator chips as
// inline SVG. No external lib. Color encodes the most recent
// rack-probe verdict per node; size encodes accelerator count.

(() => {
	const NS = "http://www.w3.org/2000/svg";
	const COL_W = 240;
	const NODE_H = 108;
	const NODE_GAP = 12;
	const COL_GAP = 36;
	const PAD = 24;
	const HEADER_H = 36;

	const VERDICT_COLOR = {
		pass: "#99c794",
		ok: "#99c794",
		fail: "#ec5f67",
		error: "#ec5f67",
	};

	// Visual treatment for nodes without RDMA. They live in a rack only
	// for management Ethernet / power-PDU reasons; nothing gets sent
	// across the RoCE fabric to them. Dim border + neutral stripe to set
	// them apart from RDMA peers without implying degradation.
	const NO_RDMA_BORDER = "#3a3f5e";
	const NO_RDMA_STRIPE = "#5e6480";

	function el(tag, attrs, children) {
		const e = document.createElementNS(NS, tag);
		if (attrs) for (const k of Object.keys(attrs)) e.setAttribute(k, attrs[k]);
		if (children) for (const c of children) {
			e.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
		}
		return e;
	}

	function nodeColor(node) {
		// Non-RDMA nodes never participate in pair/loopback probes, so
		// their "verdict" is the preflight rollup. Dim them visually so
		// the RDMA peers are easier to scan.
		if (!node.rdma_capable) {
			const v = node.last_probe && node.last_probe.verdict;
			if (v === "ok" || v === "pass") return "#7a8db4";
			if (v === "fail" || v === "error") return "#ec5f67";
			return NO_RDMA_BORDER;
		}
		const v = node.last_probe && node.last_probe.verdict;
		if (v && VERDICT_COLOR[v]) return VERDICT_COLOR[v];
		return "#fac863"; // untested
	}

	function stripeColor(node) {
		if (!node.rdma_capable) return NO_RDMA_STRIPE;
		return nodeColor(node);
	}

	function verdictLabel(node) {
		const p = node.last_probe;
		if (!p) return "untested";
		const v = p.verdict || "untested";
		// Mirror the cluster-table treatment: preflight-only nodes
		// (no RDMA hardware, no rxe) read as "no rdma" because the
		// preflight wasn't a fabric test. Pair / loopback get tagged
		// `· rdma` so the eye can pick out where a real verbs probe ran.
		if (p.source === "preflight") {
			return (v === "ok" || v === "pass") ? "no rdma" : `${v} · no rdma`;
		}
		return `${v} · rdma`;
	}

	function chip(x, y, label, count) {
		const g = el("g", { transform: `translate(${x},${y})` });
		g.appendChild(el("rect", {
			width: 64, height: 18, rx: 3,
			fill: "#1a2456", stroke: "#5e46c1", "stroke-width": 1,
		}));
		const t = el("text", {
			x: 32, y: 13, "text-anchor": "middle",
			"font-family": "SF Mono, Cascadia Code, monospace",
			"font-size": 10, fill: "#c0c5ce",
		}, [`${label} ${count}`]);
		g.appendChild(t);
		return g;
	}

	function fmtBytes(n) {
		if (n == null || !isFinite(n)) return null;
		const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
		let i = 0; let v = Math.abs(n);
		while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
		return `${v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 1)} ${units[i]}`;
	}

	function tempColor(c) {
		if (c == null) return "#65737e";
		if (c >= 90) return "#ec5f67";
		if (c >= 80) return "#fac863";
		return "#99c794";
	}

	function renderNode(node, live, x, y) {
		const g = el("g", { transform: `translate(${x},${y})` });
		const borderColor = nodeColor(node);
		const stripeFill = stripeColor(node);
		const dimmed = !node.rdma_capable;
		const m = live || {};

		g.appendChild(el("rect", {
			width: COL_W, height: NODE_H, rx: 4,
			fill: dimmed ? "#0c1438" : "#0f1b4c",
			stroke: borderColor,
			"stroke-width": 1.5,
			"stroke-dasharray": dimmed ? "4 3" : "",
		}));
		// Status stripe on left edge.
		g.appendChild(el("rect", {
			width: 4, height: NODE_H, rx: 2,
			fill: stripeFill,
		}));

		g.appendChild(el("text", {
			x: 12, y: 18,
			"font-family": "SF Mono, Cascadia Code, monospace",
			"font-size": 12, "font-weight": 700,
			fill: dimmed ? "#8d93a8" : "#c0c5ce",
		}, [node.name]));

		// "no-rdma" badge — small, top-right, only for non-RDMA nodes.
		if (dimmed) {
			const badge = el("g", { transform: `translate(${COL_W - 64}, 6)` });
			badge.appendChild(el("rect", {
				width: 56, height: 14, rx: 7,
				fill: "#1a2456", stroke: NO_RDMA_STRIPE,
			}));
			badge.appendChild(el("text", {
				x: 28, y: 10, "text-anchor": "middle",
				"font-family": "SF Mono, Cascadia Code, monospace",
				"font-size": 9, fill: "#a7adba",
			}, ["no-rdma"]));
			g.appendChild(badge);
		}

		const total = node.total_accelerators || 0;
		g.appendChild(el("text", {
			x: 12, y: 34,
			"font-family": "SF Pro Text, sans-serif",
			"font-size": 11, fill: "#a7adba",
		}, [`${total} accel · ${node.fabric_domains} fab · ${verdictLabel(node)}`]));

		// Vendor chips.
		const vendors = Object.keys(node.vendor_counts || {}).sort();
		let cx = 12;
		const cy = 48;
		for (const v of vendors) {
			g.appendChild(chip(cx, cy, v, node.vendor_counts[v]));
			cx += 70;
			if (cx + 64 > COL_W - 12) break;
		}

		// Live metrics row (temp / power / utilization).
		const live_y = 82;
		g.appendChild(el("text", {
			x: 12, y: live_y,
			"font-family": "SF Mono, Cascadia Code, monospace",
			"font-size": 10, fill: tempColor(m.temp_c),
		}, [m.temp_c == null ? "— °C" : `${m.temp_c.toFixed(0)}°C`]));
		g.appendChild(el("text", {
			x: 60, y: live_y,
			"font-family": "SF Mono, Cascadia Code, monospace",
			"font-size": 10, fill: "#a7adba",
		}, [m.power_watts == null ? "— W" : (m.power_watts >= 1000 ? `${(m.power_watts/1000).toFixed(1)}kW` : `${m.power_watts.toFixed(0)}W`)]));
		g.appendChild(el("text", {
			x: 110, y: live_y,
			"font-family": "SF Mono, Cascadia Code, monospace",
			"font-size": 10, fill: "#a7adba",
		}, [m.utilization == null ? "— %" : `${m.utilization.toFixed(0)}%`]));

		// Memory bar at bottom.
		const bar_y = 92;
		const bar_w = COL_W - 24;
		g.appendChild(el("rect", {
			x: 12, y: bar_y, width: bar_w, height: 8, rx: 2,
			fill: "#1a2456", stroke: "#2d1b5e",
		}));
		// Pick the "primary" memory bar: VRAM if present, else UMA. RAM
		// stays out of this view — the topology cards visualize accelerator
		// memory pressure, not host RAM.
		const memTotal = (m.vram_total_bytes || 0) + (m.uma_total_bytes || 0);
		const memUsed = (m.vram_used_bytes || 0) + (m.uma_used_bytes || 0);
		if (memTotal > 0) {
			const frac = Math.max(0, Math.min(1, memUsed / memTotal));
			g.appendChild(el("rect", {
				x: 12, y: bar_y, width: bar_w * frac, height: 8, rx: 2,
				fill: frac >= 0.9 ? "#ec5f67" : frac >= 0.75 ? "#fac863" : "#3bb1bc",
			}));
			const label = `${fmtBytes(memUsed)} / ${fmtBytes(memTotal)}`;
			g.appendChild(el("text", {
				x: COL_W - 12, y: bar_y - 1,
				"text-anchor": "end",
				"font-family": "SF Mono, Cascadia Code, monospace",
				"font-size": 9, fill: "#65737e",
			}, [label]));
		}

		return g;
	}

	function renderRackHeader(label, x, y, w) {
		const g = el("g", { transform: `translate(${x},${y})` });
		g.appendChild(el("rect", {
			width: w, height: HEADER_H - 8, rx: 3,
			fill: "#1a2456", stroke: "#6b5cb7",
		}));
		g.appendChild(el("text", {
			x: w / 2, y: 18, "text-anchor": "middle",
			"font-family": "SF Mono, Cascadia Code, monospace",
			"font-size": 12, "font-weight": 700, fill: "#7ee8e3",
		}, [label]));
		return g;
	}

	function groupByRack(nodes) {
		const buckets = new Map();
		const unassigned = [];
		for (const n of nodes) {
			if (n.rack) {
				if (!buckets.has(n.rack)) buckets.set(n.rack, []);
				buckets.get(n.rack).push(n);
			} else {
				unassigned.push(n);
			}
		}
		const cols = [...buckets.entries()].sort(([a], [b]) => a.localeCompare(b));
		if (unassigned.length > 0) cols.push(["(no rack)", unassigned]);
		// Sort nodes within rack lexicographically for deterministic layout.
		for (const [, ns] of cols) ns.sort((a, b) => a.name.localeCompare(b.name));
		return cols;
	}

	function render(data, fleet) {
		const nodes = (data && data.nodes) || [];
		const byNode = (fleet && fleet.by_node) || {};
		if (nodes.length === 0) {
			const svg = el("svg", { width: 320, height: 80 });
			svg.appendChild(el("text", {
				x: 160, y: 44, "text-anchor": "middle",
				"font-family": "SF Pro Text, sans-serif", "font-size": 13, fill: "#65737e",
			}, ["No nodes to render"]));
			return svg;
		}

		const cols = groupByRack(nodes);
		const tallest = cols.reduce((m, [, ns]) => Math.max(m, ns.length), 0);
		const w = PAD * 2 + cols.length * COL_W + (cols.length - 1) * COL_GAP;
		const h = PAD * 2 + HEADER_H + tallest * (NODE_H + NODE_GAP);

		const svg = el("svg", {
			width: w, height: h, viewBox: `0 0 ${w} ${h}`,
		});

		let x = PAD;
		for (const [rack, rackNodes] of cols) {
			svg.appendChild(renderRackHeader(rack, x, PAD, COL_W));
			let y = PAD + HEADER_H;
			for (const n of rackNodes) {
				svg.appendChild(renderNode(n, byNode[n.name], x, y));
				y += NODE_H + NODE_GAP;
			}
			x += COL_W + COL_GAP;
		}

		return svg;
	}

	window.AccelrdTopology = { render };
})();
