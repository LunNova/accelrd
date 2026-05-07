// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Static UI assets baked into the binary at compile time. Keeps the
//! admin pod self-contained — no ConfigMap, no extra volumes, image is
//! the only artifact. The `static/admin/` directory is the source of
//! truth; on a release build, the contents become read-only `.rodata`.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};

static ADMIN_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static/admin");

pub fn index() -> Response {
	serve_path("index.html").unwrap_or_else(|| not_found())
}

pub fn serve(rest: &str) -> Response {
	// Strip a single leading `/` if axum's matcher passes one through.
	let path = rest.strip_prefix('/').unwrap_or(rest);
	if path.is_empty() {
		return index();
	}
	serve_path(path).unwrap_or_else(|| not_found())
}

fn serve_path(path: &str) -> Option<Response> {
	let file = ADMIN_ASSETS.get_file(path)?;
	let mime = mime_for(path);
	Some(
		Response::builder()
			.status(StatusCode::OK)
			.header(header::CONTENT_TYPE, mime)
			// Static assets are versioned implicitly with the binary; a
			// short max-age + must-revalidate keeps deploys honest.
			.header(header::CACHE_CONTROL, "public, max-age=60, must-revalidate")
			.body(Body::from(file.contents()))
			.expect("static body builds")
			.into_response(),
	)
}

fn not_found() -> Response {
	(StatusCode::NOT_FOUND, "Not found").into_response()
}

fn mime_for(path: &str) -> &'static str {
	match path.rsplit_once('.').map(|(_, ext)| ext) {
		Some("html") => "text/html; charset=utf-8",
		Some("css") => "text/css; charset=utf-8",
		Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
		Some("json") => "application/json; charset=utf-8",
		Some("svg") => "image/svg+xml",
		Some("png") => "image/png",
		Some("ico") => "image/x-icon",
		Some("woff2") => "font/woff2",
		_ => "application/octet-stream",
	}
}
