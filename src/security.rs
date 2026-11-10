//! Local-only request guard. The server binds to 127.0.0.1 only; this
//! middleware additionally rejects mismatched Host headers and cross-site
//! state-changing origins, which is the practical baseline for a
//! localhost service exposed to a browser.

use axum::extract::Request;
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub async fn local_guard(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    if !is_api {
        return next.run(req).await;
    }

    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(is_local_host)
        .unwrap_or(false);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }

    let state_changing = !matches!(
        req.method(),
        &Method::GET | &Method::HEAD | &Method::OPTIONS
    );
    if state_changing {
        if let Some(origin) = req.headers().get(header::ORIGIN) {
            let ok = origin
                .to_str()
                .ok()
                .and_then(is_local_origin)
                .unwrap_or(false);
            if !ok {
                return (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response();
            }
        }
    }

    next.run(req).await
}

/// Accept 127.0.0.1 / localhost / ::1 hosts, with or without the port.
fn is_local_host(value: &str) -> bool {
    let lower = value.to_lowercase();
    if lower.starts_with("[::1]") {
        return true;
    }
    let host = lower.rsplit_once(':').map(|(h, _)| h).unwrap_or(&lower);
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Origins: http(s) on the same local hosts only.
fn is_local_origin(value: &str) -> Option<bool> {
    let after_scheme = match value.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => value,
    };
    Some(is_local_host(
        after_scheme.split('/').next().unwrap_or("").trim(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_host_detection() {
        for good in [
            "127.0.0.1:8423",
            "localhost:8423",
            "localhost",
            "127.0.0.1",
            "[::1]:8423",
        ] {
            assert!(is_local_host(good), "{good} should be local");
        }
        for bad in [
            "evil.example:8423",
            "127.0.0.1.evil.com:8",
            "instagram.com:443",
        ] {
            assert!(!is_local_host(bad), "{bad} should not be local");
        }
    }

    #[test]
    fn local_origin_detection() {
        assert!(is_local_origin("http://127.0.0.1:8423").unwrap_or(false));
        assert!(is_local_origin("http://localhost:8423").unwrap_or(false));
        assert!(!is_local_origin("https://instagram.com").unwrap_or(true));
        assert!(!is_local_origin("http://127.0.0.1.evil.com:8").unwrap_or(true));
    }
}
