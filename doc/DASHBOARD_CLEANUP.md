# Dashboard cleanup catalog

**Status:** Inventory of UI debt accumulated across the 2026-04-* shipping arcs. Audio-path work is done; the dashboard now has 6+ months of bring-up cards still rendering as if the system were under active development. This file is the to-do list when we sit down to make the operator-facing UI presentable.

Every section below names a specific element + DOM id where applicable, then proposes a fix. None of these are blocking — they're the "I want this to look professional" backlog.

## High-impact (operator-facing)

### 1. Idle-state staleness — already noted, recap here

Field readings persist after the chain returns to Idle, so the operator can't tell at a glance whether the panel is showing **now** or **last call**. Worst offenders:

| Panel | Element | What it shows now (idle) | What it should show |
|---|---|---|---|
| `IMBE + Vocoder` | `voc_status` | `ACTIVE (645,120 samples)` | `IDLE` |
| `IMBE + Vocoder` → CURRENT CALL | `cc_tg_dur` | `TG ? · 160.7s` | `(idle)` |
| `IMBE + Vocoder` → CURRENT CALL | `cc_duids1`/`cc_duids2`/`cc_imbe`/`cc_voc` | last call's totals | zeros or `--` |
| `Traffic Channel` | `tune_batch_tg` | `TG 300` (last batch's TG) | `(idle)` |
| `Traffic Channel` | `tune_trf_duid` | `TDU_LC (0xF)` | `(idle)` |
| `Grant Follower` | `trf_duid` | `TDU_LC (0xF)` | `(idle)` |
| Audio status bar | `underruns` counter | sticks at last value, no reset boundary | reset on Stop/Start, or per-call |

Root cause for the IMBE/Vocoder ones: see `project_post_pacer_next_steps.md` item 1 — vocoder thread's `current_call` never closes on chain-Idle. **Fixing that in `vocoder_task.rs` clears most of the row above in one shot.** The renderer-side cleanup is then ~10 lines to null-out per-call fields when payload's `current_call.tg == null`.

### 2. Decoder Comparison Matrix is mostly dead columns

The `cmp_t` table in the Debug tab has two columns (`PS LSM framer` / `PL HDL LSM`) where most rows have one side filled and the other reads `(PS only)`, `(HDL: hit-only)`, `--`, or `(PS framer)`. After the 2026-04-16 PS C4FM retire it's a 2-column matrix where each metric naturally lives in one column. Cleanest fix:

- Split into two single-column cards stacked vertically: **HDL LSM (hardware)** and **PS framer (software)**. No more dead cells.
- Move pipeline-specific rows (`TSBK CRC OK / via plain / via xored`, `TSBK trellis failures`) into a third sub-section labeled `PS framer pipeline`.
- Drop the explanatory paragraph at the bottom — it exists only to explain the dead-cell convention that the split fixes.

### 3. Mixed PS/PL labelling in the LSM Dibit Stream card

Outer `<h2>`: "LSM Dibit Stream Diagnostics" with subtitle "PL HDL LSM chain (lsm_dibit_dma)".
Inner card `<h2>`: "PS LSM Dibit Stream".
Footer paragraph: "Live metrics from the PL HDL LSM dibit stream".

Pick one. The data is from the HDL LSM chain via `lsm_dibit_dma`, presented by the PS framer. Suggest:

- Outer header: `LSM Dibit Stream` (no `Diagnostics` suffix)
- Inner card title: drop `PS` — say `Histogram + Sync`
- Footer: keep, fix wording: "Live metrics tapped from the HDL LSM dibit DMA. TSDU bucket <90% means NID payload bits are being corrupted upstream."

Also: the `grid2` wrapper around it has only one card now (the second card was retired in 2026-04-16). Drop the grid wrapper and let the single card span full width, or add a second metric-relevant card.

### 4. Phase numbers leaking into operator UI

`Phase 7C`, `Phase 6F.2`, `Phase 7D`, `Phase 7E`, `Phase 9`, `Phase 10`, `Phase 10.7`, `Phase 10.8`, `Phase 2h (2026-04-25)`, `Phase 2b` — all visible in card titles, footers, and HTML comments rendered as data. Examples in current DOM:

- `<span id="trf_phase">Phase 7C</span>` — top-right of Traffic Channel header.
- Comment-marker phase tags appear in element titles via `style` / `title` attrs.

These were dev waypoints; the operator doesn't need them. Sweep:

- Remove `trf_phase` element entirely (or repurpose as a build-tag short hash if useful).
- HTML comments with phase markers stay in source (history is fine in code) but anything user-facing strips them.

### 5. Build tag rendered twice

Top header: `build: 2026-04-30-audio-pacer` (`#build_tag`).
Board Info first row: `Build` cell `#bi_build` showing the same string.

Same string, two places, a few hundred px apart. Pick one:

- Drop `#bi_build` from the Board Info table; it's already in the header.
- OR drop the `#build_tag` span and let the Board Info row be the canonical place.

I'd remove the `#bi_build` row — header is more skimmable.

### 6. Modulation row reads like a paragraph

Current value of `tune_mod`:

> `C4FM + LSM (parallel chains, LSM is the active one for HDU/TDU/LDU dispatch + IMBE extraction)`

That's a footnote, not a value. Suggest:

- Value: `LSM` (with `C4FM idle` dim subtext if both chains are running)
- Move the "parallel chains" detail into a `title=` tooltip or the card footer.

The Board Info `Modulation` selector row also has its own status string `LSM (c4fm NIDs=0 / lsm NIDs=20,500)` which is informative — keep that one.

### 7. Auto-PPM tracker dual-state confusion

Current display:

```text
PPM cal: -0.403 ppm   shift 346 Hz   cal: 21m ago   [Recalibrate]
         new ppm -0.403 (A: 409 Hz, B: -63.0 Hz) 7509ms

Auto PPM: [✓] enabled   Anchor ± 50 Hz
          est: 217.2 Hz (-0.253 ppm, Δ-128.8 Hz)
          → blocked: Δ-129 Hz outside anchor
```

Two separate ppm values (`-0.403` from last forced cal, `-0.253` from tracker estimate) plus an anchor-blocked status. Unclear at a glance which one is *applied*. Suggest:

- Single banner row at the top: `Applied PPM: -0.403 (forced cal, 21m ago)` in green if in-anchor, orange if blocked.
- Below: `Tracker estimate: -0.253` — secondary text, less prominent.
- Keep the per-stage A/B numbers in a `title=` hover.

### 8. Tune widget — three-button cluster

Current Radio Freq row has 7 controls in a row: input + 4 step buttons + Tune. Center row has input + Set center + auto/lock radio + status. Total: a lot of horizontal real estate for what is mostly "type a freq, click Tune."

- Collapse `-12.5k / -6.25k / +6.25k / +12.5k` into a single step-size dropdown + ± buttons: `Step: 12.5k ▾   [-]  [+]   [Tune]`. Saves 2 buttons.
- Move `Set center` + `auto/lock` into an expandable details disclosure — most operators don't touch the LO directly.

## Medium-impact (debug noise / accumulated cruft)

### 9. Hidden DOM placeholders

```html
<span id="dibits" style="display:none">0</span>
<span id="overflow" style="display:none">No</span>
```

Comment says they're kept "for historical reasons" so refresh() doesn't throw. Drop them once the corresponding refresh() assignments are removed. ~5-line cleanup. Already noted in the source comment as a "future cleanup."

### 10. Inline color hex codes

PS Cores grid uses `background:#4a8` for the busy bar, hardcoded. Should use `var(--green)` (or a new `--accent` if we want the bar to be slightly different from text-green).

### 11. Inline styles vs CSS classes

A lot of inline `style="..."` attributes in card layouts (gap, flex-wrap, font-size) where a few utility classes would be cleaner. Not blocking but if we ever want to do a theme variant or print stylesheet, this fights us.

### 12. PS Cores table noise

Seven threads listed, five of them `tokio-rt-worker`. Each pool worker is interchangeable so showing TID-871 vs TID-878 doesn't help diagnose anything.

- Group `tokio-rt-worker` rows into a single line: `tokio-rt-worker × 5 · max 4.0% · sum 8.0%`.
- Keep `p25-httpd`, `p25-vocoder`, and the audio-pacer thread (when it shows up) as their own rows.

### 13. Empty / placeholder values cluttering output

When idle, panels show `--` in many cells. Mixed conventions: some show `--`, some show `(idle)`, some show `0`, some show actual values from the last call. Pick one convention and apply across:

- `--` for "no data ever observed"
- `(idle)` for "no data right now, was active before"
- `0` for "data observed, count is zero"

### 14. HDL LSM Chain detail — too many "Last 1s window:" rows

The card mixes cumulative counters (top half) with windowed counters (bottom half). A divider row + a `Window` subhead would help skim. Currently 9 rows total.

## Low-impact (polish)

### 15. Card subtitles are inconsistent

Some cards have a subtitle in the `<h2>` itself:

```html
<h2>Pipeline Status <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">full-chain snapshot from /api/pipeline; opt-in</span></h2>
```

Others put it in the body as a `<p>` after the table. Pick one place + style.

### 16. Repeated explanatory paragraphs

Several cards have a `<p>` footer with `font-size:0.75em;color:var(--text-dim)` explaining the data source / interpretation. They're useful, but the styling is duplicated 8+ times. Make a `.card-footer-note` class.

### 17. `tab-badge` for Logs shows raw count

`(3248)` next to the Logs tab name. After 1 hour at a busy site this is `(50000+)` and meaningless. Two options:

- Convert to a "new since last view" delta: `(+12)` (resets on tab click).
- Drop the count entirely; the tab content shows it.

### 18. API tab table styling

Filter input + table with `Try` column linking out to `/api/...` pages. Functional but the table cells inherit `font-size:0.85em` which makes the `Try` button look tiny. Standardise on a button utility class.

### 19. Aliases popup trigger uses ⚙ glyph

`<span class="alias-btn" onclick="showAliases()">⚙ Aliases</span>` — should be a `<button>` for accessibility. Currently a span with onclick.

### 20. Radio tab subgrids drift

`grid2` and `grid3` classes used inconsistently. Some areas wrap manually with inline `display:flex`. Pick a layout primitive and apply.

## Pickup order suggestion

If we do a single dashboard-cleanup session:

1. Fix idle-state staleness (server-side `vocoder_task.rs` change unblocks most of #1)
2. Renderer-side null-out for idle (pairs with #1)
3. Decoder Comparison Matrix split into 2 cards (#2)
4. Strip phase numbers from operator UI (#4)
5. Drop duplicate build tag (#5)
6. Modulation value → short label + tooltip (#6)
7. Hidden DOM placeholders + inline colors (#9, #10)

Items 8-20 are polish that can ride along incrementally.

## Out-of-scope here (own memo)

- Any back-end behavior change beyond the vocoder idle-close noted in #1.
- API endpoint changes (different cleanup pass).
- Theme / print stylesheet (not asked for).
