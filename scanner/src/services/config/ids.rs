//! Identifiers of systems and sites: slugs of `[a-z0-9_-]`, which the history rows and the
//! recording file names carry.

/// Longest id accepted.
pub const MAX_LEN: usize = 64;

pub fn is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_LEN
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// An id from a label.
pub fn slug(label: &str) -> String {
    let mut s: String = label.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s: String = s.trim_matches('_').chars().take(48).collect();
    if s.is_empty() { "site".into() } else { s }
}

/// `base`, or `base_2`, `base_3`... when it is taken.
pub fn unique(base: String, taken: impl Fn(&str) -> bool) -> String {
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}_{n}")).find(|c| !taken(c)).unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_lower_case_slugs() {
        assert!(is_valid("system_00a_site_109_109") && is_valid("cec_gcs") && is_valid("clay-county"));
        assert!(!is_valid("Clay") && !is_valid("") && !is_valid("a/b"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Florida Power & Light (Clay)"), "florida_power_light_clay");
        assert_eq!(slug("  "), "site");
        assert!(is_valid(&slug("Green Cove Springs")));
        assert_eq!(unique("clay".into(), |id| id == "clay" || id == "clay_2"), "clay_3");
    }
}
