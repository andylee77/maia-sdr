# UI brief: the final layout

For the session that designs the scanner's final web UI. Andy's direction, 2026-10-01:

- The current pages are base templates. Each shows the information its part of the API offers;
  the final layout is designed from them later.
- The backend is cleaned up first; the UI follows. A UI need the API can't serve is a backend
  task, not a UI workaround.
- The UI doesn't have to use every API call. The API is the full radio; the UI is a clean view
  of it.
- The Now page gets a session of its own.

`doc/API_INVENTORY.md` lists each route: what it carries, what it lacks, and who uses it.

## The Now page

Andy's requirements:

1. **System info at the top:** the live system, its site, the control channel, and how well it
   decodes.
2. **Call history in its own scroll container.** Scrolling the calls must not scroll the header
   or the system info off the screen. p25-httpd's list was much better than the current one:
   start from `p25-httpd/src/httpd/ui/js/components/calls_list.js`. That list had:
   - inline playback;
   - a site picker;
   - a filter;
   - the three times labelled for what they measure (voice, on air, grant).
3. **A radio selector** to choose what to listen to. To settle with Andy: does "radio" mean a
   lane (the call it carries), or a system/site?
4. **A clean left/right like before:** p25-httpd's speakers panel
   (`components/speakers_panel.js`):
   - talkgroup groups go to the left or right speaker;
   - talkgroups in no group go to both, left, right or off;
   - a higher-priority group can interrupt a lower one;
   - with two lanes, lane 1 plays left and lane 2 right at the same time.
5. **Lock onto a talkgroup.**
6. **Select a profile** on the page.
7. Only the live site's calls (the API serves this since `fea5236`).

### What the backend has for each

| Need | API | Gap |
|------|-----|-----|
| System info | `GET /api/v1/status` (live site, control, tuning, clock); `GET /api/v1/receivers` for detail | `live.tuning` isn't refreshed when the crystal tracker steps the LO |
| Call history | `GET /api/v1/calls` (open calls and the live site's newest 100); `GET /api/v1/activity/calls` (older calls, filters, CSV); `GET /api/v1/recordings/{id}` (the WAV, byte ranges) | No per-speaker times within a call |
| Listening | `/ws/audio?v=2`: every lane, each 20 ms frame tagged with its lane, text meta frames naming the talkgroup | — |
| Left/right | A profile's `speakers` (`left`, `right`, `other`, `preempt`) via `PUT /api/v1/profiles/{id}`; groups in the profile | — |
| Talkgroup lock | None | An endpoint to hold the live site on one talkgroup until released (the follower only follows it); the follower already locks a lane to a call's talkgroup for follow-on replies |
| Profile select | `GET /api/v1/profiles`, `PUT /api/v1/sites/{id}/profile` | — |
