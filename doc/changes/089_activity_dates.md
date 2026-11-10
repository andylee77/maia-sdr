# 089 — Activity: dates, more rows, and a playing recording left alone

**Date:** 2026-11-09. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed; the test build `2026-11-09-dates1` is not on a unit yet.

## Why

Andy could not review past alerts or past calls. Activity showed the last 24 hours, 7 days or
30 days, while the history keeps 365. Its "Newest" alerts were the newest 50 whatever the period,
which on Clay County is about 8 hours of alerts, and Recent calls were the newest 50 calls.

He also found the page reloading under a recording he was playing annoying. Every reload rebuilt
the alert list, which removed the player and stopped the recording.

Every `/api/v1/activity/*` query already took `from` and `to`. Only the page lacked them.

## Changes

All in `scanner/src/ui/static/js/views/activity.js`.

- **Dates:** a fourth chip next to 24 h, 7 days and 30 days, with a From and a To (a date and an
  hour each). They start as the period shown, ending with the hour now running.
  - The summary, the chart, the talkgroups, the radios, a picked radio's or talkgroup's details,
    the alerts, the recent calls and the CSV export all cover the picked dates.
  - The dates run on whole hours, because the history counts by the hour: the totals, the chart
    and the lists then cover the same span.
  - Up to 7 days are drawn per hour, longer dates per day. Days start at the midnight of the
    dates' first day.
  - Picked dates are not reloaded as calls close.
- **Show more:** under the newest alerts ("The newest 50 of 126 alerts") and under Recent calls.
  Each press asks for twice as many rows, up to 800.
- **An alert's row is kept** while its alert is listed, so the recording it plays goes on through
  a reload (Show more, or the automatic one).
- **The automatic reload waits while a recording plays,** so the list does not move under it. It
  looks again every 2 s.

## Measured

On unit A, over 30 days of Clay County (3,055 alerts, 259,000 calls):

| Rows asked for | Alerts | Calls |
|---|---|---|
| 50 | 0.30 s | 0.08 s |
| 400 | 1.11 s | 0.12 s |
| 800 | 2.17 s | 0.17 s |

An alert costs more than a call because each one's dispatch is looked up.

## Not in this change

A radio or talkgroup can only be picked from the 25 busiest of the period. Clay's console 1015
(49 calls in a month) cannot be reached from the page; the API reaches it with `unit=1015`.

## Checks

- **`node --check`** on `views/activity.js`. Every `import { ... }` in it names an export.
- **Headless Chrome** (`runs/ui_check/activity_dates.mjs`): the page from the PC daemon (build
  `2026-11-09-dates1`), its history from unit A. No exceptions or console errors, and:
  - 24 h as before: 50 alerts of 126, 50 calls.
  - Show more: 100 alerts, with the row whose recording was started still in the page with its
    player; 100 calls.
  - Dates: the two inputs appear filled with the last 24 hours on whole hours, the lists go back
    to 50 rows, and every query carries `from` and `to` in place of `hours`.
  - 2026-10-16 14:00 to 17:00: 37 alerts, all inside the three hours; the calls end at 16:59; the
    chart has three hourly bars, with `tz=-240` (daylight time on that day).
  - The summary's 2,294 calls for those hours equal the calls the API lists for them.
  - A radio picked in those hours: its details, calls and alerts keep the dates.
  - Nine days are drawn per day. Dates that end before they start are refused with a message,
    and nothing is asked.
  - 7 days again: the dates hide and the queries go back to `hours=168`.
- **The other pages** (Now, Systems, Diagnostics, Settings) load on the PC daemon without
  exceptions.
- **Host tests:** 515 pass.

## Status log

- 2026-11-09: committed.
