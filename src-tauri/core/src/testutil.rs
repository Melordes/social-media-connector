//! Helpers for tests.

use wiremock::{Match, Request};

/// Matches requests whose raw body contains `needle` (works for multipart
/// bodies with binary parts, unlike `body_string_contains`).
pub struct BytesContains(pub Vec<u8>);

impl Match for BytesContains {
    fn matches(&self, request: &Request) -> bool {
        request.body.windows(self.0.len()).any(|w| w == self.0.as_slice())
    }
}

pub fn bytes_contains(needle: &str) -> BytesContains {
    BytesContains(needle.as_bytes().to_vec())
}
