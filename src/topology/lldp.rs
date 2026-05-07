// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! LLDP neighbour discovery via raw AF_PACKET socket.
//!
//! IEEE 802.1AB-2009. We open one packet socket bound to ETH_P_LLDP
//! (0x88cc), wait for inbound LLDP frames on any interface, and parse
//! the TLV stream to extract Chassis ID, Port ID, and System Name.
//!
//! Why raw sockets and not lldpd: lldpd needs a `_lldpd` system user
//! for privilege separation, which our (rootless, sysfs-only) container
//! image doesn't have. A raw AF_PACKET socket gives us the same data
//! with one runtime capability (`CAP_NET_RAW`) and ~150 LOC of TLV
//! parsing — no external daemon, no userspace deps.
//!
//! Required capabilities:
//!  - `CAP_NET_RAW` (raw socket creation).
//!  - `CAP_NET_ADMIN` is NOT needed — we don't bind to a specific
//!    interface or set promiscuous mode.
//!  - The pod must run with `hostNetwork: true` so it shares the
//!    kernel's link layer with the host (LLDP frames are link-local
//!    multicast and don't traverse the netns boundary).

use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::unix::AsyncFd;

const ETH_P_LLDP: u16 = 0x88cc;

#[derive(Debug, Clone, Serialize)]
pub struct LldpNeighbor {
	/// The host-side interface that received the LLDP frame.
	pub interface: String,
	/// Switch chassis identifier as a printable string (MAC address in
	/// `aa:bb:cc:dd:ee:ff` form, hostname, or hex-encoded other).
	pub chassis_id: String,
	pub chassis_id_kind: ChassisIdKind,
	/// Switch-side port identifier — string ("TE8", "Ethernet1/3"), MAC,
	/// or hex-encoded depending on subtype.
	pub port_id: String,
	/// System Name TLV. Many switches don't advertise it.
	pub system_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChassisIdKind {
	Mac,
	Name,
	Other,
}

impl LldpNeighbor {
	/// A label-safe slug for use as `accel-topo.lunnova.dev/rack`.
	/// MAC chassis IDs become a 12-char lowercase hex string; other
	/// kinds get squashed to alphanumerics + dashes.
	pub fn rack_slug(&self) -> String {
		match self.chassis_id_kind {
			ChassisIdKind::Mac => self.chassis_id.replace(':', ""),
			_ => slugify(&self.chassis_id),
		}
	}
}

/// Open one socket, wait up to `timeout` for at least one LLDP frame
/// per interface, return all neighbours observed.
///
/// Returns an empty vec on permission/socket error (logged).
pub async fn discover(timeout: Duration) -> Vec<LldpNeighbor> {
	if timeout.is_zero() {
		return Vec::new();
	}
	let socket = match open_socket() {
		Ok(s) => s,
		Err(e) => {
			tracing::warn!(
				error = %e,
				"LLDP socket open failed (likely missing CAP_NET_RAW); skipping LLDP discovery",
			);
			return Vec::new();
		}
	};
	let async_fd = match AsyncFd::new(socket) {
		Ok(f) => f,
		Err(e) => {
			tracing::warn!(error = %e, "AsyncFd wrap failed for LLDP socket");
			return Vec::new();
		}
	};

	let deadline = Instant::now() + timeout;
	let mut by_iface: HashMap<String, LldpNeighbor> = HashMap::new();

	loop {
		let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
			break;
		};
		let Ok(Ok(mut guard)) = tokio::time::timeout(remaining, async_fd.readable()).await else {
			break;
		};

		let mut buf = [0u8; 1500];
		let mut sa: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
		let mut sa_len = std::mem::size_of_val(&sa) as libc::socklen_t;
		let recv = guard.try_io(|fd| {
			let n = unsafe {
				libc::recvfrom(
					fd.as_raw_fd(),
					buf.as_mut_ptr().cast(),
					buf.len(),
					0,
					(&mut sa as *mut libc::sockaddr_ll).cast(),
					&mut sa_len,
				)
			};
			if n < 0 {
				Err(io::Error::last_os_error())
			} else {
				Ok(n as usize)
			}
		});
		match recv {
			Ok(Ok(n)) => {
				let iface = ifindex_to_name(sa.sll_ifindex as u32).unwrap_or_else(|| format!("if{}", sa.sll_ifindex));
				if let Some(neighbor) = parse_lldpdu(&buf[..n], iface.clone()) {
					tracing::debug!(
						iface = %iface,
						chassis = %neighbor.chassis_id,
						port = %neighbor.port_id,
						"LLDP neighbour observed",
					);
					by_iface.insert(iface, neighbor);
				}
			}
			Ok(Err(e)) => tracing::debug!(error = %e, "LLDP recv error"),
			// would-block — try_io returns Err; loop back to readable().
			Err(_) => continue,
		}
	}

	by_iface.into_values().collect()
}

fn open_socket() -> io::Result<OwnedFd> {
	// Protocol passed to socket(2) for AF_PACKET is the L2 ethertype in
	// network byte order, i32-widened.
	let proto_be = (ETH_P_LLDP.to_be()) as i32;
	let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK, proto_be) };
	if fd < 0 {
		return Err(io::Error::last_os_error());
	}
	let owned = unsafe { OwnedFd::from_raw_fd(fd) };

	// sll_ifindex = 0 means "any interface". Binding still required so
	// the kernel filters by sll_protocol.
	let mut sa: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
	sa.sll_family = libc::AF_PACKET as libc::sa_family_t;
	sa.sll_protocol = ETH_P_LLDP.to_be();
	let rc = unsafe {
		libc::bind(
			owned.as_raw_fd(),
			(&sa as *const libc::sockaddr_ll).cast(),
			std::mem::size_of_val(&sa) as libc::socklen_t,
		)
	};
	if rc < 0 {
		return Err(io::Error::last_os_error());
	}

	// LLDP frames go to the IEEE-reserved multicast 01:80:c2:00:00:0e,
	// which NICs filter from default delivery. Subscribe each Ethernet
	// interface explicitly so the kernel forwards matching frames to
	// our socket. Errors here are non-fatal — at worst, an interface
	// doesn't have a switch on the other side.
	for ifindex in ethernet_ifindices() {
		subscribe_lldp_multicast(owned.as_raw_fd(), ifindex);
	}
	Ok(owned)
}

const LLDP_MULTICAST: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e];

fn subscribe_lldp_multicast(fd: std::os::fd::RawFd, ifindex: i32) {
	let mut mreq: libc::packet_mreq = unsafe { std::mem::zeroed() };
	mreq.mr_ifindex = ifindex;
	mreq.mr_type = libc::PACKET_MR_MULTICAST as u16;
	mreq.mr_alen = 6;
	mreq.mr_address[..6].copy_from_slice(&LLDP_MULTICAST);
	let rc = unsafe {
		libc::setsockopt(
			fd,
			libc::SOL_PACKET,
			libc::PACKET_ADD_MEMBERSHIP,
			(&mreq as *const libc::packet_mreq).cast(),
			std::mem::size_of_val(&mreq) as libc::socklen_t,
		)
	};
	if rc < 0 {
		tracing::debug!(
			ifindex,
			error = %io::Error::last_os_error(),
			"PACKET_ADD_MEMBERSHIP failed for LLDP multicast (non-fatal)",
		);
	} else {
		tracing::debug!(ifindex, "subscribed to LLDP multicast on interface");
	}
}

/// Enumerate ifindex of Ethernet interfaces that are up and not loopback
/// or obvious virtuals. We rely on /sys/class/net/<iface>/{ifindex,flags}.
fn ethernet_ifindices() -> Vec<i32> {
	let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
		return Vec::new();
	};
	let mut out = Vec::new();
	for entry in entries.flatten() {
		let Some(name) = entry.file_name().to_str().map(str::to_string) else {
			continue;
		};
		if name == "lo" {
			continue;
		}
		// Skip obvious virtual / overlay interfaces. We could be more
		// precise via SIOCGIFFLAGS, but startswith covers >99% of
		// real-world deployments.
		if name.starts_with("docker") || name.starts_with("veth") || name.starts_with("br-") {
			continue;
		}
		let ifindex_path = entry.path().join("ifindex");
		let Some(idx) = std::fs::read_to_string(&ifindex_path)
			.ok()
			.and_then(|s| s.trim().parse::<i32>().ok())
		else {
			continue;
		};
		// Only subscribe on interfaces that are administratively up.
		// /sys/class/net/<iface>/operstate reads "up", "down", "unknown", etc.
		let state = std::fs::read_to_string(entry.path().join("operstate"))
			.ok()
			.map(|s| s.trim().to_string())
			.unwrap_or_default();
		if state != "up" && state != "unknown" {
			continue;
		}
		out.push(idx);
	}
	out
}

fn ifindex_to_name(idx: u32) -> Option<String> {
	let mut buf = [0u8; libc::IF_NAMESIZE];
	let p = unsafe { libc::if_indextoname(idx, buf.as_mut_ptr().cast()) };
	if p.is_null() {
		return None;
	}
	let cstr = unsafe { CStr::from_ptr(p) };
	cstr.to_str().ok().map(str::to_string)
}

/// Parse a stream of LLDP TLVs. Returns Some when at least Chassis ID
/// and Port ID were extracted (the two mandatory TLVs).
fn parse_lldpdu(buf: &[u8], interface: String) -> Option<LldpNeighbor> {
	let mut chassis: Option<(String, ChassisIdKind)> = None;
	let mut port: Option<String> = None;
	let mut sysname: Option<String> = None;

	let mut p = 0;
	while p + 2 <= buf.len() {
		let hdr = u16::from_be_bytes([buf[p], buf[p + 1]]);
		let ty = (hdr >> 9) as u8;
		let len = (hdr & 0x01FF) as usize;
		p += 2;
		if p + len > buf.len() {
			break;
		}
		let val = &buf[p..p + len];
		p += len;
		match ty {
			0 => break, // End of LLDPDU
			1 => chassis = parse_chassis_id(val),
			2 => port = parse_port_id(val),
			5 => {
				sysname = std::str::from_utf8(val)
					.ok()
					.filter(|s| !s.is_empty())
					.map(String::from)
			}
			_ => {} // ignore other TLVs (port description, sys description, capabilities, mgmt addr, org-specific)
		}
	}

	let (chassis_id, chassis_id_kind) = chassis?;
	let port_id = port?;
	Some(LldpNeighbor {
		interface,
		chassis_id,
		chassis_id_kind,
		port_id,
		system_name: sysname,
	})
}

fn parse_chassis_id(val: &[u8]) -> Option<(String, ChassisIdKind)> {
	let (subtype, body) = val.split_first()?;
	match (*subtype, body) {
		(4, [a, b, c, d, e, f]) => Some((mac_to_string(&[*a, *b, *c, *d, *e, *f]), ChassisIdKind::Mac)),
		(6 | 7, body) => std::str::from_utf8(body)
			.ok()
			.filter(|s| !s.is_empty())
			.map(|s| (s.to_string(), ChassisIdKind::Name)),
		(_, body) => Some((hex_encode(body), ChassisIdKind::Other)),
	}
}

fn parse_port_id(val: &[u8]) -> Option<String> {
	let (subtype, body) = val.split_first()?;
	match (*subtype, body) {
		(3, [a, b, c, d, e, f]) => Some(mac_to_string(&[*a, *b, *c, *d, *e, *f])),
		(5 | 6 | 7, body) => std::str::from_utf8(body).ok().map(String::from),
		(_, body) => Some(hex_encode(body)),
	}
}

fn mac_to_string(b: &[u8; 6]) -> String {
	format!(
		"{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
		b[0], b[1], b[2], b[3], b[4], b[5]
	)
}

fn hex_encode(bytes: &[u8]) -> String {
	let mut s = String::with_capacity(bytes.len() * 2);
	for b in bytes {
		s.push_str(&format!("{b:02x}"));
	}
	s
}

fn slugify(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut last_dash = false;
	for c in s.chars() {
		let c = c.to_ascii_lowercase();
		if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
			out.push(c);
			last_dash = false;
		} else if !last_dash {
			out.push('-');
			last_dash = true;
		}
	}
	out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// One of the captured LLDP frames from the lab switch (Alcatel-Lucent
	/// chassis `1c:2a:a3:1e:c7:b4`, port `TE4`). Reconstructed from the
	/// tcpdump hex dumps.
	#[test]
	fn parse_lab_switch_frame() {
		let mut buf = Vec::new();
		// Chassis ID TLV: type=1 len=7  subtype=4 + 6-byte MAC
		buf.extend_from_slice(&[0x02, 0x07, 0x04, 0x1c, 0x2a, 0xa3, 0x1e, 0xc7, 0xb4]);
		// Port ID TLV: type=2 len=4  subtype=7 + "TE4"
		buf.extend_from_slice(&[0x04, 0x04, 0x07, b'T', b'E', b'4']);
		// TTL TLV: type=3 len=2 + 0x0078
		buf.extend_from_slice(&[0x06, 0x02, 0x00, 0x78]);
		// End TLV
		buf.extend_from_slice(&[0x00, 0x00]);

		let n = parse_lldpdu(&buf, "eno1np0".into()).expect("parse");
		assert_eq!(n.chassis_id, "1c:2a:a3:1e:c7:b4");
		assert_eq!(n.chassis_id_kind, ChassisIdKind::Mac);
		assert_eq!(n.port_id, "TE4");
		assert_eq!(n.system_name, None);
		assert_eq!(n.rack_slug(), "1c2aa31ec7b4");
	}

	#[test]
	fn parse_with_system_name() {
		let mut buf = Vec::new();
		buf.extend_from_slice(&[0x02, 0x07, 0x04, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
		buf.extend_from_slice(&[0x04, 0x05, 0x07, b'p', b'o', b'r', b't']);
		buf.extend_from_slice(&[0x06, 0x02, 0x00, 0x78]);
		// System Name TLV: type=5
		buf.extend_from_slice(&[0x0a, 0x07, b's', b'w', b'i', b't', b'c', b'h', b'1']);
		buf.extend_from_slice(&[0x00, 0x00]);

		let n = parse_lldpdu(&buf, "eth0".into()).expect("parse");
		assert_eq!(n.chassis_id, "00:11:22:33:44:55");
		assert_eq!(n.port_id, "port");
		assert_eq!(n.system_name.as_deref(), Some("switch1"));
	}

	#[test]
	fn rejects_truncated() {
		// length claims 100 but only 4 bytes follow
		let buf = [0x0c, 0x64, 0x01, 0x02, 0x03, 0x04];
		assert!(parse_lldpdu(&buf, "x".into()).is_none());
	}
}
