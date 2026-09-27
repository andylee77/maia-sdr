//! Eye-window math for delay sweeps (pure).

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub start: usize,
    pub end: usize,
    pub len: usize,
    pub centre: f64,
}

impl Window {
    pub fn to_json(self) -> Value {
        json!({"start": self.start, "end": self.end, "len": self.len, "centre": self.centre})
    }
}

/// Longest run of passing taps (first one on ties).
pub fn longest_run(pass: &[bool]) -> Option<Window> {
    let mut best: Option<Window> = None;
    let mut i = 0;
    while i < pass.len() {
        if pass[i] {
            let s = i;
            while i < pass.len() && pass[i] {
                i += 1;
            }
            let len = i - s;
            if best.map(|b| len > b.len).unwrap_or(true) {
                best = Some(Window {
                    start: s,
                    end: i - 1,
                    len,
                    centre: (s + i - 1) as f64 / 2.0,
                });
            }
        } else {
            i += 1;
        }
    }
    best
}

/// Passing taps on each side of `chosen` before the first failure (or the
/// end of the sweep). `None` if `chosen` itself fails.
pub fn margins(pass: &[bool], chosen: usize) -> Option<(usize, usize)> {
    if chosen >= pass.len() || !pass[chosen] {
        return None;
    }
    let lo = (0..chosen).rev().take_while(|&i| pass[i]).count();
    let hi = (chosen + 1..pass.len()).take_while(|&i| pass[i]).count();
    Some((lo, hi))
}

pub fn ascii(pass: &[bool]) -> String {
    pass.iter().map(|p| if *p { 'o' } else { '.' }).collect()
}

pub fn as_ints(pass: &[bool]) -> Vec<u8> {
    pass.iter().map(|p| *p as u8).collect()
}

/// Summary of a 1-D sweep.
pub fn summary_1d(pass: &[bool], chosen: Option<usize>) -> Value {
    let w = longest_run(pass);
    let m = chosen.and_then(|c| margins(pass, c));
    json!({
        "pass": as_ints(pass),
        "ascii": ascii(pass),
        "passing": pass.iter().filter(|p| **p).count(),
        "window": w.map(|w| w.to_json()),
        "centre": w.map(|w| w.centre),
        "chosen": chosen,
        "chosen_passes": chosen.map(|c| pass.get(c).copied().unwrap_or(false)),
        "margin_lo": m.map(|m| m.0),
        "margin_hi": m.map(|m| m.1),
        "margin_min": m.map(|m| m.0.min(m.1)),
    })
}

/// Summary of a 2-D grid (`grid[row][col]`), with the chosen point.
pub fn summary_2d(grid: &[Vec<bool>], chosen: Option<(usize, usize)>) -> Value {
    let rows: Vec<Value> = grid
        .iter()
        .map(|r| json!({"ascii": ascii(r), "window": longest_run(r).map(|w| w.to_json())}))
        .collect();
    let ncols = grid.iter().map(|r| r.len()).max().unwrap_or(0);
    let cols: Vec<Vec<bool>> = (0..ncols)
        .map(|c| grid.iter().map(|r| r.get(c).copied().unwrap_or(false)).collect())
        .collect();
    let mut chosen_v = Value::Null;
    if let Some((r, c)) = chosen {
        if r < grid.len() {
            let row_m = margins(&grid[r], c);
            let col_m = cols.get(c).and_then(|col| margins(col, r));
            chosen_v = json!({
                "row": r, "col": c,
                "passes": grid[r].get(c).copied().unwrap_or(false),
                "row_margin": row_m.map(|m| [m.0, m.1]),
                "col_margin": col_m.map(|m| [m.0, m.1]),
                "margin_min": match (row_m, col_m) {
                    (Some(a), Some(b)) => Some(a.0.min(a.1).min(b.0).min(b.1)),
                    _ => None,
                },
            });
        }
    }
    let max_row_window = grid
        .iter()
        .map(|r| longest_run(r).map(|w| w.len).unwrap_or(0))
        .max()
        .unwrap_or(0);
    json!({
        "grid": grid.iter().map(|r| as_ints(r)).collect::<Vec<_>>(),
        "ascii": grid.iter().map(|r| ascii(r)).collect::<Vec<_>>(),
        "passing": grid.iter().flatten().filter(|p| **p).count(),
        "rows": rows,
        "max_row_window": max_row_window,
        "chosen": chosen_v,
    })
}

/// Parses the AD9361 driver's `bist_timing_analysis` output: rows of the
/// form `<hex>: o o . ...` ('o' = pass, '.' = fail).
pub fn parse_timing_grid(text: &str) -> Vec<Vec<bool>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        let (head, rest) = match t.split_once(':') {
            Some(x) => x,
            None => continue,
        };
        if head.len() > 2 || head.is_empty() || !head.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let row: Vec<bool> = rest
            .chars()
            .filter(|c| *c == 'o' || *c == '.' || *c == 'x')
            .map(|c| c == 'o')
            .collect();
        if !row.is_empty() {
            out.push(row);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Vec<bool> {
        s.chars().map(|c| c == 'o').collect()
    }

    #[test]
    fn windows_and_margins() {
        let v = p("..oooooo...ooo..");
        let w = longest_run(&v).unwrap();
        assert_eq!((w.start, w.end, w.len), (2, 7, 6));
        assert_eq!(w.centre, 4.5);
        assert_eq!(margins(&v, 4), Some((2, 3)));
        assert_eq!(margins(&v, 0), None);
        assert_eq!(margins(&v, 12), Some((1, 1)));
        assert!(longest_run(&p("....")).is_none());
        let s = summary_1d(&v, Some(4));
        assert_eq!(s["margin_min"], 2);
        assert_eq!(s["passing"], 9);
    }

    #[test]
    fn grid_summary() {
        let g = vec![p("..oo"), p(".ooo"), p("oooo"), p("oo..")];
        let s = summary_2d(&g, Some((2, 1)));
        assert_eq!(s["chosen"]["row_margin"], json!([1, 2]));
        assert_eq!(s["chosen"]["col_margin"], json!([1, 1]));
        assert_eq!(s["chosen"]["margin_min"], 1);
        assert_eq!(s["passing"], 11);
    }

    #[test]
    fn timing_grid_parse() {
        let t = "CLK: 61440000 Hz 'o' = PASS\nDC0:1:2:3:\n0:o o . .\n1:. o o o \na:o o o o\n";
        let g = parse_timing_grid(t);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0], p("oo.."));
        assert_eq!(g[2], p("oooo"));
    }
}
