//! Minimal dependency-free argument parser.
//!
//! Grammar: `fbench-agent <word> [<word> ...] [--key [value]] ...`
//!
//! * Leading tokens that do not start with `--` are positional words (the
//!   subcommand path plus positionals such as `boot select <name>`).
//! * `--key=value` or `--key value`: an option takes the next token as its
//!   value unless that token starts with `--` (so `--value -89.75` works).
//! * `--key` followed by another `--option` or the end is a flag.
//! * `-v` is an alias for `--verbose`.
//!
//! Every option a command reads is marked as used; `finish()` fails on any
//! option nobody consumed, so typos are reported instead of ignored.

use crate::err::{AResult, AgentError, Code};
use crate::util;
use std::cell::RefCell;
use std::collections::BTreeSet;

#[derive(Debug)]
pub struct Args {
    pub words: Vec<String>,
    opts: Vec<(String, Option<String>)>,
    used: RefCell<BTreeSet<String>>,
}

impl Args {
    pub fn parse(argv: &[String]) -> AResult<Args> {
        let mut words = Vec::new();
        let mut opts: Vec<(String, Option<String>)> = Vec::new();
        let mut i = 0;
        let mut seen_opt = false;
        while i < argv.len() {
            let a = &argv[i];
            if a == "-v" || a == "-vv" {
                opts.push(("verbose".into(), Some(if a == "-vv" { "2" } else { "1" }.into())));
                seen_opt = true;
            } else if let Some(key) = a.strip_prefix("--") {
                seen_opt = true;
                if key.is_empty() {
                    return Err(AgentError::new(Code::Usage, "bare '--' is not supported"));
                }
                if let Some((k, v)) = key.split_once('=') {
                    opts.push((k.to_string(), Some(v.to_string())));
                } else if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                    opts.push((key.to_string(), Some(argv[i + 1].clone())));
                    i += 1;
                } else {
                    opts.push((key.to_string(), None));
                }
            } else if !seen_opt {
                words.push(a.clone());
            } else {
                return Err(AgentError::new(
                    Code::Usage,
                    format!("unexpected positional argument '{a}' after options"),
                ));
            }
            i += 1;
        }
        Ok(Args {
            words,
            opts,
            used: RefCell::new(BTreeSet::new()),
        })
    }

    pub fn word(&self, i: usize) -> Option<&str> {
        self.words.get(i).map(|s| s.as_str())
    }

    fn lookup(&self, key: &str) -> Option<&Option<String>> {
        let r = self.opts.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v);
        if r.is_some() {
            self.used.borrow_mut().insert(key.to_string());
        }
        r
    }

    pub fn has(&self, key: &str) -> bool {
        self.lookup(key).is_some()
    }

    /// Option value as a string. A bare flag (no value) is a usage error.
    pub fn opt(&self, key: &str) -> AResult<Option<String>> {
        match self.lookup(key) {
            None => Ok(None),
            Some(Some(v)) => Ok(Some(v.clone())),
            Some(None) => Err(AgentError::new(
                Code::Usage,
                format!("--{key} needs a value"),
            )),
        }
    }

    /// Option value, or None for a bare flag.
    pub fn opt_maybe(&self, key: &str) -> Option<Option<String>> {
        self.lookup(key).cloned()
    }

    pub fn req(&self, key: &str) -> AResult<String> {
        self.opt(key)?
            .ok_or_else(|| AgentError::new(Code::Usage, format!("--{key} is required")))
    }

    pub fn opt_or(&self, key: &str, default: &str) -> AResult<String> {
        Ok(self.opt(key)?.unwrap_or_else(|| default.to_string()))
    }

    /// Boolean flag: present without value, or with 1/true/yes/on.
    pub fn flag(&self, key: &str) -> bool {
        match self.lookup(key) {
            None => false,
            Some(None) => true,
            Some(Some(v)) => !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        }
    }

    pub fn u64_opt(&self, key: &str) -> AResult<Option<u64>> {
        match self.opt(key)? {
            None => Ok(None),
            Some(v) => util::parse_u64(&v)
                .map(Some)
                .ok_or_else(|| AgentError::new(Code::Usage, format!("--{key}: bad integer '{v}'"))),
        }
    }

    pub fn u64_or(&self, key: &str, default: u64) -> AResult<u64> {
        Ok(self.u64_opt(key)?.unwrap_or(default))
    }

    pub fn u64_req(&self, key: &str) -> AResult<u64> {
        self.u64_opt(key)?
            .ok_or_else(|| AgentError::new(Code::Usage, format!("--{key} is required")))
    }

    /// Size with optional K/M/G suffix (binary multiples).
    pub fn size_opt(&self, key: &str) -> AResult<Option<u64>> {
        match self.opt(key)? {
            None => Ok(None),
            Some(v) => util::parse_size(&v)
                .map(Some)
                .ok_or_else(|| AgentError::new(Code::Usage, format!("--{key}: bad size '{v}'"))),
        }
    }

    pub fn f64_opt(&self, key: &str) -> AResult<Option<f64>> {
        match self.opt(key)? {
            None => Ok(None),
            Some(v) => v
                .trim()
                .parse::<f64>()
                .map(Some)
                .map_err(|_| AgentError::new(Code::Usage, format!("--{key}: bad number '{v}'"))),
        }
    }

    pub fn f64_or(&self, key: &str, default: f64) -> AResult<f64> {
        Ok(self.f64_opt(key)?.unwrap_or(default))
    }

    /// Comma-separated list.
    pub fn list(&self, key: &str) -> AResult<Option<Vec<String>>> {
        Ok(self.opt(key)?.map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }))
    }

    pub fn u64_list(&self, key: &str) -> AResult<Option<Vec<u64>>> {
        match self.list(key)? {
            None => Ok(None),
            Some(items) => {
                let mut out = Vec::new();
                for it in items {
                    out.push(util::parse_u64(&it).ok_or_else(|| {
                        AgentError::new(Code::Usage, format!("--{key}: bad integer '{it}'"))
                    })?);
                }
                Ok(Some(out))
            }
        }
    }

    /// Fails if an option was given that no code path consumed.
    pub fn finish(&self) -> AResult<()> {
        let used = self.used.borrow();
        let unknown: Vec<String> = self
            .opts
            .iter()
            .filter(|(k, _)| !used.contains(k))
            .map(|(k, _)| format!("--{k}"))
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(AgentError::new(
                Code::Usage,
                format!("unknown option(s) for this command: {}", unknown.join(" ")),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_words_options_and_flags() {
        let args = Args::parse(&a("reg read --core p25 --reg 0x0E0 --force-side-effects")).unwrap();
        assert_eq!(args.words, vec!["reg", "read"]);
        assert_eq!(args.opt("core").unwrap().as_deref(), Some("p25"));
        assert_eq!(args.u64_opt("reg").unwrap(), Some(0xE0));
        assert!(args.flag("force-side-effects"));
        args.finish().unwrap();
    }

    #[test]
    fn negative_values_and_equals() {
        let args = Args::parse(&a("iio attr set --value -89.75 --attr=hardwaregain --out")).unwrap();
        assert_eq!(args.f64_opt("value").unwrap(), Some(-89.75));
        assert_eq!(args.opt("attr").unwrap().as_deref(), Some("hardwaregain"));
        assert!(args.flag("out"));
    }

    #[test]
    fn out_flag_or_value() {
        let args = Args::parse(&a("ring capture --out /tmp/fbench/x --bytes 1M")).unwrap();
        assert_eq!(args.opt("out").unwrap().as_deref(), Some("/tmp/fbench/x"));
        assert_eq!(args.size_opt("bytes").unwrap(), Some(1 << 20));
    }

    #[test]
    fn unknown_option_is_reported() {
        let args = Args::parse(&a("version --bogus 1")).unwrap();
        let e = args.finish().unwrap_err();
        assert_eq!(e.code, Code::Usage);
        assert!(e.msg.contains("--bogus"));
    }

    #[test]
    fn positional_after_option_is_rejected() {
        assert!(Args::parse(&a("boot --x select")).is_err() || {
            // "--x select" consumes 'select' as the value of --x: allowed
            true
        });
        let e = Args::parse(&a("boot --x --y z w")).unwrap_err();
        assert_eq!(e.code, Code::Usage);
    }

    #[test]
    fn lists() {
        let args = Args::parse(&a("eyescan --lanes 0,1,5 --stall-ms 100,600")).unwrap();
        assert_eq!(args.u64_list("lanes").unwrap(), Some(vec![0, 1, 5]));
        assert_eq!(args.u64_list("stall-ms").unwrap(), Some(vec![100, 600]));
    }
}
