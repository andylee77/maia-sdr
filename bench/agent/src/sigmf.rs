//! SigMF metadata for ring captures (pure).

use serde_json::{json, Value};

pub struct CaptureInfo<'a> {
    pub datatype: &'a str,
    pub sample_rate: Option<f64>,
    pub frequency: Option<f64>,
    pub datetime: String,
    pub hw: String,
    pub description: String,
    pub agent_version: String,
    pub extra: Value,
}

/// Builds a SigMF 1.0 `.sigmf-meta` document. Agent-specific fields live in
/// the `fbench:` namespace.
pub fn meta(info: &CaptureInfo) -> Value {
    let mut global = serde_json::Map::new();
    global.insert("core:datatype".into(), json!(info.datatype));
    global.insert("core:version".into(), json!("1.0.0"));
    if let Some(sr) = info.sample_rate {
        global.insert("core:sample_rate".into(), json!(sr));
    }
    global.insert("core:hw".into(), json!(info.hw));
    global.insert("core:description".into(), json!(info.description));
    global.insert("core:recorder".into(), json!(format!("fbench-agent {}", info.agent_version)));
    global.insert("core:author".into(), json!("fbench-agent"));
    global.insert("core:extensions".into(), json!([{"name": "fbench", "version": "1.0.0", "optional": true}]));
    if let Value::Object(m) = &info.extra {
        for (k, v) in m {
            global.insert(format!("fbench:{k}"), v.clone());
        }
    }
    let mut cap = serde_json::Map::new();
    cap.insert("core:sample_start".into(), json!(0));
    cap.insert("core:datetime".into(), json!(info.datetime));
    if let Some(f) = info.frequency {
        cap.insert("core:frequency".into(), json!(f));
    }
    json!({
        "global": Value::Object(global),
        "captures": [Value::Object(cap)],
        "annotations": [],
    })
}

/// `<base>.sigmf-data` / `<base>.sigmf-meta` / `<base>.subbuf.jsonl` /
/// `<base>.anomalies.jsonl` from a user path (extension stripped).
pub fn base_path(p: &str) -> String {
    for ext in [".sigmf-data", ".sigmf-meta", ".sigmf", ".cs16", ".bin", ".raw"] {
        if let Some(b) = p.strip_suffix(ext) {
            return b.to_string();
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_shape() {
        let m = meta(&CaptureInfo {
            datatype: "ci16_le",
            sample_rate: Some(8e6),
            frequency: Some(858.1e6),
            datetime: "2026-09-26T12:00:00.000Z".into(),
            hw: "Fishball Z7020 serial X".into(),
            description: "test".into(),
            agent_version: "0.1.0".into(),
            extra: json!({"ring": "p25-wideband", "subbuf_bytes": 1048576}),
        });
        assert_eq!(m["global"]["core:datatype"], "ci16_le");
        assert_eq!(m["global"]["core:sample_rate"], 8e6);
        assert_eq!(m["global"]["fbench:subbuf_bytes"], 1048576);
        assert_eq!(m["captures"][0]["core:frequency"], 858.1e6);
        assert_eq!(base_path("/tmp/fbench/a.sigmf-data"), "/tmp/fbench/a");
        assert_eq!(base_path("/tmp/fbench/a"), "/tmp/fbench/a");
    }
}
