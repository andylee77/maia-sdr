//! Identifiers of systems and sites: slugs of `[a-z0-9_-]`, which the history rows and the
//! recording file names carry.

/// Longest id accepted.
pub const MAX_LEN: usize = 64;

pub fn is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_LEN
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_lower_case_slugs() {
        assert!(is_valid("system_00a_site_109_109") && is_valid("cec_gcs") && is_valid("clay-county"));
        assert!(!is_valid("Clay") && !is_valid("") && !is_valid("a/b"));
    }
}
