//! Identifiers of systems, sites and profiles: slugs of `[a-z0-9_-]`.
//!
//! Site ids are the p25-httpd site names (`clay`, `cec_gcs`, ...), which the history rows and
//! recording file names already carry.

/// Longest id accepted.
pub const MAX_LEN: usize = 64;

pub fn is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_LEN
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// An id made from a label: lower case, runs of other characters become one `-`.
pub fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out: String = out.trim_end_matches('-').chars().take(MAX_LEN).collect();
    if out.is_empty() { "unnamed".into() } else { out }
}

/// `base`, or `base-2`, `base-3`, ... until `taken` says no.
pub fn unique(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).expect("an unused id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_valid_ids() {
        assert_eq!(slug("Clay Electric, Green Cove Springs (DMR)"), "clay-electric-green-cove-springs-dmr");
        assert_eq!(slug("System 00A"), "system-00a");
        assert_eq!(slug("  --  "), "unnamed");
        assert!(is_valid(&slug("Florida Power and Light (Clay)")));
        assert!(is_valid("system_00a_site_109_109") && is_valid("cec_gcs"));
        assert!(!is_valid("Clay") && !is_valid("") && !is_valid("a/b"));
    }

    #[test]
    fn unique_appends_a_number() {
        let taken = ["a", "a-2"];
        assert_eq!(unique("a", |id| taken.contains(&id)), "a-3");
        assert_eq!(unique("b", |id| taken.contains(&id)), "b");
    }
}
