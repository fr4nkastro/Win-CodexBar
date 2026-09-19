use super::{CLOUDFLARE_BODY_PREFIX_BYTES, classify_web_http_error};
use crate::core::ProviderError;
use reqwest::{StatusCode, header};

#[test]
fn tagged_forbidden_response_returns_oauth_and_network_guidance() {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        "cf-mitigated",
        header::HeaderValue::from_static("  ChAlLeNgE  "),
    );

    let error =
        classify_web_http_error("usage", StatusCode::FORBIDDEN, &headers, b"challenge page");

    assert_eq!(
        error.to_string(),
        crate::providers::claude::CLOUDFLARE_CHALLENGE_MESSAGE
    );
    assert!(error.to_string().contains("OAuth"));
    assert!(error.to_string().contains("different network"));
}

#[test]
fn just_a_moment_fixture_is_detected_only_in_the_bounded_prefix() {
    let error = classify_web_http_error(
        "organizations",
        StatusCode::FORBIDDEN,
        &header::HeaderMap::new(),
        br#"<html><title>Just a moment...</title></html>"#,
    );
    assert_eq!(
        error.to_string(),
        crate::providers::claude::CLOUDFLARE_CHALLENGE_MESSAGE
    );

    let mut late_marker = vec![b'x'; CLOUDFLARE_BODY_PREFIX_BYTES];
    late_marker.extend_from_slice(b"Just a moment");
    assert!(matches!(
        classify_web_http_error(
            "usage",
            StatusCode::FORBIDDEN,
            &header::HeaderMap::new(),
            &late_marker,
        ),
        ProviderError::AuthRequired
    ));
}

#[test]
fn ordinary_forbidden_and_all_unauthorized_responses_stay_auth_failures() {
    let mut challenge_headers = header::HeaderMap::new();
    challenge_headers.insert(
        "cf-mitigated",
        header::HeaderValue::from_static("challenge"),
    );

    assert!(matches!(
        classify_web_http_error(
            "usage",
            StatusCode::UNAUTHORIZED,
            &challenge_headers,
            b"Just a moment",
        ),
        ProviderError::AuthRequired
    ));
    assert!(matches!(
        classify_web_http_error(
            "usage",
            StatusCode::FORBIDDEN,
            &header::HeaderMap::new(),
            b"permission denied",
        ),
        ProviderError::AuthRequired
    ));
}
