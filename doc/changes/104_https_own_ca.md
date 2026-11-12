# 104 — HTTPS as the one way in: the unit's own CA and certificate

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image since `2026-11-11-access2` (106), with the
firmware's side, tezuka_fw's change 007. Unit A's CA and certificate are on its flash: they
were made by this change's test build and the image went on using them.

## Why

The roadmap's Q9, the second part of step 6. 102 closed the unit to whoever has no password,
and left the password itself crossing the network in the clear: "HTTPS as the one way in and its
certificate" was not in it. 103 read the firmware and found why "as recommended" was not enough
to build it: the firmware's first-boot script made a CA and a certificate and deleted the CA's
key, so no certificate could be made again from that CA; and its certificate named
`192.168.x.1` only, not unit A's Ethernet address, so a browser warned twice. Asked "Keep a new
CA's key on the unit, so its certificate can be remade when the address changes?", Andy answered
"Yes, keep it".

## Changes

### The unit's CA and its certificate

`services/tls.rs`, kept in `<flash>/scanner/tls/` (a directory and files of root's alone).

- **The CA** is made once, at the first start that finds none: an ECDSA P-256 key that stays on
  the flash (`ca.key`), and a certificate named `Fishball scanner <host name> <8 hex digits>`
  that holds from 2020 to 2100, so it does not depend on the board's clock. A PC installs it
  once and goes on trusting the unit through every certificate after.
- **What it may sign for** is written into it (name constraints, critical; no CA below it):
  private addresses (loopback, link-local, 10/8, 172.16/12, 192.168/16, the shared range a VPN
  gives, and IPv6's loopback, unique-local and link-local) and local names (`localhost`,
  `.local`, `.lan`, `.home.arpa`, `.internal`, and the unit's own host name). A PC that trusts
  it trusts it for those and nothing else, so the key on a board is worth little to whoever
  takes the board: it cannot stand in for a site on the Internet.
- **The certificate** names what the unit is reached by now: each interface's address,
  `localhost`, its host name bare and under `.local`, and the names its administrator added
  (the Access card's names, from 102). It lasts 820 days (Apple's browsers refuse more than
  825). It is made again when those names change (looked at every 30 s, and at once when the
  names are edited), when it has 30 days left, and once the clock is set if it was made before
  (the board starts in 1970). The server takes the new one without a restart, and the radio's
  log says what it names.
- **What the CA may not sign for is left out and told** (`left_out`, and a line on the card): a
  public address, a name that is not local, and a name that is not a host name as a
  certificate writes one (an underscore: see "Found by the checks").
- A factory reset leaves the CA and the certificate, as it leaves the passwords; an export does
  not carry them. A new CA is made only when its files are gone.
- The certificates are made with `rcgen` 0.13 on aws-lc, the library the unit's TLS and the
  passwords already use (0.14 would have moved aws-lc's version; `Cargo.lock` keeps it). The
  interfaces' addresses come from `getifaddrs` (`nix`'s `net` feature).

### HTTPS as the one way in

- **Once a password is set, the plain HTTP port answers every request with a redirect (307) to
  the same path on HTTPS**, but for the CA's certificate. A password, a session's cookie and a
  token then cross the network only inside TLS. An open unit serves both ports as before, so
  nothing changes for a unit until its password is set.
- **`GET /ca.crt`** (anyone, and it stays on the plain port after a password is set: a PC needs
  it before it can trust the other port) is the CA's certificate, to install.
- **`GET /api/v1/certificate`** (anyone): the HTTPS port, the CA's name and SHA-256, what the
  certificate names and leaves out, and when it ends.
- **The scanner's arguments** `--ssl-cert`, `--ssl-key` and `--ca-cert` are gone;
  `--listen-https` stays. The firmware's `S60scanner` passes it always (tezuka_fw 007), and that
  script and this scanner go together: this scanner exits on the old arguments.
- A unit that cannot make its certificate (a flash that cannot be written) serves plain HTTP as
  before and says so in its log and on the card: it stays reachable.

### The Access card

Every reader of the card (an open radio's, the administrator, a listener) sees the radio's
HTTPS under it: the port, with "Open this page over HTTPS" when the page is on the plain one;
what the certificate names; the CA to download, with its SHA-256 in groups of four to hold
against what the PC shows when it is installed; and what was left out, if anything. On an open
radio reached over plain HTTP the card says, above the first password's fields, that a
password typed there crosses the network in the clear and to open the page over HTTPS first.

### Tools, the bench and the checks

- **A tool reaches a unit that has a password over HTTPS** (`tools/scanner_token.py`): with
  `SCANNER_TOKEN` set, a host given without a scheme is `https://<host>:8443`, and
  `SCANNER_CA`, the file of the unit's CA certificate, is what makes it trusted. Without
  `SCANNER_CA` the tool is refused by its own TLS and says what to do. An open unit is reached
  as always, on port 8080; with `SCANNER_CA` set, over HTTPS too.
- **A token goes over HTTPS only.** A tool pointed at the plain port sends its request without
  the token, is sent on to HTTPS, and sends the token there. The same holds for the bench's
  client (`bench/fbench/transport.py`: HTTPS on 8443 when `SCANNER_TOKEN` is set, trusted by
  `SCANNER_CA`) and the page checks (`runs/ui_check/unit.mjs`).
- **`tools/atsc_live_check.py`** gives VLC the stream's address with the token in it (a player
  sends no header; 102's `?token=`).
- **The page checks** take the unit's HTTPS address and the CA through `NODE_EXTRA_CA_CERTS`.
  A door to a unit (`--reads`) speaks TLS to it. They are run with Node itself: the Node inside
  Electron (VS Code's, which ran them until now) refuses the CA ("unsupported name constraint
  type"), because its TLS library does not judge constraints on addresses.
- **`runs/ui_check/https_api.py`** (new, the PC daemon only: it sets a password) checks the
  whole of it; `access_api.py`, `access_page.mjs` and `locked_run.py` close a daemon and so
  speak HTTPS to it.

### Found by the checks, and fixed

- **A host name with an underscore** (this PC's, `nucbox_k6`) in the certificate made a strict
  verifier (pyca/cryptography's, which reads names as the browsers' rules do) refuse the whole
  certificate, for every name in it. A name that is not a host name as a certificate writes one
  (letters, digits and hyphens in labels of 1 to 63) is now left out of the certificate and of
  the CA's own list, and told. Unit A's name, `pluto`, is one.
- **The count of wrong passwords was not started over** when the passwords were removed and set
  again (102): five wrong ones before left the next wrong one, after, to wait 30 s at once. The
  administrator's password given right to change or remove it now starts the count over, as a
  sign-in does. Found by running `access_api.py` and `access_page.mjs` back to back.
- **A new certificate is told a moment before it is served** (the store, then the server): a
  few milliseconds in which the old one still answers. Left as it is; the check waits.

### The firmware (tezuka_fw `fishball-dev`, change 007)

- `package/scanner/S60scanner` starts the scanner with `--listen-https 0.0.0.0:8443` and no
  certificate files.
- `S50p25-httpd-certificates` is `S50flash`: what is left of it is formatting a new board's
  blank flash partition once and mounting it, which the scripts after it rely on. Its
  extension file is gone, and `post-build-p25.sh` removes both from the target directory an
  earlier build left.
- The scanner's package no longer selects OpenSSL and its binary, which only that script used.

The old files on a unit's flash (`p25-httpd.crt`, `p25-httpd.key`, `p25-sdr-ca.crt`) are not
read by anything. A PC that had installed the old CA takes the new one.

## To trust a unit from a PC

Open `http://<unit>:8080`, the radio's Settings, Access: download the CA's certificate, hold its
SHA-256 against the card's, and install it as a trusted root (in Windows, the certificate's
"Install Certificate", into "Trusted Root Certification Authorities"). The browser then opens
`https://<unit>:8443` without a warning, by any address or name the certificate lists. For the
tools, point `SCANNER_CA` at the same file.

## Not in this change

- **`iiod`** (Q10), which waits for Andy's word on what 103 found; **SSH**, which is his to say.
- **A password on unit A**, which is his to set. Until it is set the unit is open on both ports
  as before.
- **The SD image**: unit A runs this from RAM; a reboot returns it to the image's pages, and the
  image's scanner does not read the new CA (it stays on the flash for the next image).

## Checks

See the status log for the counts.

- **Host tests:** what the CA may sign for; the unit's names; a certificate made once, kept, and
  made again when a name changes or it nears its end; the CA and certificate kept across a
  start, and a new CA when its key is gone; a certificate made before the clock was set made
  again after; a host name no certificate can carry left out; a request's host without its
  port; the count of wrong passwords. (The keys readable by their owner alone is a test for
  the unit's system, compiled by the ARM check and not run on the PC; on unit A the files are
  root's alone.)
- **`https_api.py`** against the PC daemon: the CA given over plain HTTP and its SHA-256 the
  one the daemon tells; HTTPS verified against it by address and by name, and refused without
  it; a name added in a new certificate within seconds, one the CA may not sign for left out,
  the log telling of it; closed, each request on the plain port sent to HTTPS but for the CA's
  certificate; the session's cookie for HTTPS alone; a token over HTTPS.
- **Other verifiers** than Python's OpenSSL, against the PC daemon and unit A:
  pyca/cryptography's (Rust), Windows' own (curl on Schannel), Node's.
- **The closed PC daemon, with a listener's token and the CA:** tools by the HTTPS address, by
  the plain port (sent on) and by the bare host on the default ports; the bench's client; a
  tool without the CA refused; the Files page; the tool pages through a door.
- **The pages,** as before, and over HTTPS.

## Status log

- 2026-11-11: built. Host tests 578 pass (7 new); the DMR reference's tests pass (102); the ARM
  check and the ARM release build are clean; the bench's host tests pass (269). BUILD_TAG
  `2026-11-11-https1`.
- 2026-11-11: on the PC daemon (started with `--listen-https 127.0.0.1:8453`), each run leaving
  it open:
  - `https_api.py` 16 steps; `access_api.py` 32, over HTTPS; `access_page.mjs` 15 at two sizes,
    over HTTPS, back to back with `access_api.py`;
  - the daemon's certificate verified by address and by name by Python's OpenSSL (its strict
    default), pyca/cryptography's verifier, curl on Windows' Schannel and Node 24's OpenSSL;
    without the CA, Python and Node refuse it;
  - closed, with a listener's token and the CA: `scanner_files.py` by the HTTPS address, by the
    plain port (sent on to HTTPS) and, with the daemon on ports 8080 and 8443, by the bare
    host; the bench's client reading `/api/v1/status` over `https://127.0.0.1:8443`; a tool
    without the CA refused; `files_page.mjs` 9 steps with its browser signed in by the token;
    `tool_pages.mjs` through a door speaking TLS, 26 steps with its sockets;
  - open, over HTTPS: `files_page.mjs` 15 (an archive downloaded through the page),
    `tool_pages.mjs` 31, `now_rail.mjs` 29;
  - open, over HTTP as before: `files_page.mjs` 15, `playlist_page.mjs` 16,
    `aliases_page.mjs` 34, `now_rail.mjs` 29 and 15, `scanner_pages.mjs` 40, `tool_pages.mjs`
    31 at two sizes, `tv_watch.mjs` 12, `modules_check.mjs` 47 modules;
  - the reset marker (`scanner-reset-access` on the card) opened the daemon after a failed run
    had left it closed: the log says so and the marker is gone.
- 2026-11-11: `2026-11-11-https1` on unit A (the binary and the firmware's `S60scanner`, in RAM
  over the SD image `2026-11-10-pages2`: a reboot of A returns to that image):
  - started with both ports, live on Clay County, the control channel at 100 %, recording as
    before (the card's count rose from 101,779 to 101,799 while the checks ran), nothing in its
    log; open as it was, both ports answering; no password was set on unit A, which is Andy's
    to do, so nothing there redirects;
  - it made its CA (`Fishball scanner pluto 3431a941`) and a certificate naming `127.0.0.1`,
    `192.168.120.50`, `192.168.2.1`, `localhost`, `pluto` and `pluto.local`, nothing left out,
    its files root's alone in `/mnt/jffs2/scanner/tls/`;
  - `https://192.168.120.50:8443` verified against the CA from `/ca.crt` by Python's OpenSSL,
    pyca/cryptography (by both addresses and both names; an address and a name it does not
    list refused), curl on Schannel and Node; without the CA, Python refuses it;
  - `scanner_files.py` over HTTPS with `SCANNER_CA`, and refused without it;
  - with A serving the page, reads only, over HTTP: `files_page.mjs --no-downloads` 9 steps,
    `tool_pages.mjs` 27 at two sizes, `now_rail.mjs` 15, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13, `scanner_pages.mjs` 40; over HTTPS:
    `files_page.mjs` 9, `tool_pages.mjs` 27, `now_rail.mjs` 15;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **The Files check on a busy unit:** it compared the page with a read of its own made seconds
  later, and Clay saved a recording between them in most runs that evening. The page shows what
  was on the card when it read it (it does not follow each recording saved; from 101). The
  check now has the page read anew before it counts a difference; four runs on A then passed.
- **Not checked:**
  - **a browser trusting the CA once it is installed.** The checks' Chrome ignores certificate
    errors, and installing a root on this PC is Andy's to do. Four other verifiers accept the
    certificate; no browser's own was tried. So the first thing to try is installing unit A's
    CA and opening `https://192.168.120.50:8443`. Known to refuse it: the Node inside
    Electron, which does not judge a CA's constraints on addresses. A browser that refused it
    for the same reason would mean a CA without them, which is a new CA;
  - a password on unit A, and so the redirect, the sign-in page and a token there (no check
    sets a password on a unit; all of it was checked on the PC daemon);
  - the certificate made again on a unit when its address changes (on the PC daemon a name
    added did it within seconds; an address changing is the same path, 30 s at most later),
    near its end, or after a start in 1970 (host tests);
  - the bench's own runs against a closed unit (its client was; `iiod` is not behind any of
    this);
  - VLC with a token in a stream's address over HTTPS.
- 2026-11-11: in the SD image `2026-11-11-access2` on unit A (106): the image's `S60scanner`
  started the scanner with both ports, the old certificate script is gone from it, and HTTPS
  verified against the CA saved before the image went on. Not checked there: an image built from
  nothing, which has no `openssl` binary (this one's build kept the one it had).
