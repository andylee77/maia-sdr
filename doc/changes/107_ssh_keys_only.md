# 107 — SSH by key only

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image, `2026-11-11-ssh1`, with the firmware's
side, tezuka_fw's change 009.

## Why

The last part of the roadmap's step 6, the half of Q10 that had no recommendation and was left
for Andy. The image sets a root password in its build configuration, which is in the firmware's
repository, and its SSH server took password logins: whoever knew that password had root over
the network, past every door 102 to 105 built (the passwords' file, the CA's key, libiio's
switch are all root's to read and change). Asked "Make SSH on the units keys-only, with no
password login?", Andy answered "Yes, keys only".

## Changes

- **The scanner image's SSH takes a key and no password** (tezuka_fw 009:
  `/etc/default/dropbear`, the server's `-s`). The image carries its key
  (`/root/.ssh/authorized_keys`, Andy's, the one the bench and the sessions use).
- **The root password still opens the serial consoles** (the DEBUG port, and the USB cable's
  serial function): the way back for whoever has the unit in hand and no key.
- **The bench's `setup keys --password-prompt`** cannot put a key on such an image, and need
  not: the image has the key. `bench/README.md` says so.
- **The roadmap:** with this, step 6 is built. What a client on the network can reach on a unit
  is the scanner's two ports (open until its administrator's password is set), SSH by key, and
  libiio's port while it is switched on.
- BUILD_TAG `2026-11-11-ssh1`, so that a unit tells which image it runs.

## Not in this change

- **The root password itself**, which stays as it is and stays in the repository: it no longer
  opens anything over the network.
- **Another key.** The image's file is what the unit uses (a copy on the flash is read only
  when the image has none), so a second PC's key goes into the image
  (`overlay_p25/root/.ssh/authorized_keys`), or is added by hand until the next restart. Keys
  are not managed from the Access card.
- The other images (`*maiasdr*`, `*mini*`), which keep password logins.

## Checks

See the status log.

## Status log

- 2026-11-11: built. Host tests 582 pass; the DMR reference's tests pass (102); the ARM check
  is clean. The scanner is 106's but for its tag, BUILD_TAG `2026-11-11-ssh1`.
- 2026-11-11: tried on unit A first, the firmware's file put on its RAM root with a timed way
  back armed (the old setting restored after three minutes unless a login by key had removed
  its marker): the server started as `dropbear -s -R` and offered `publickey` alone where it
  had offered `publickey,password`; a login and a copy by key worked; a login without a key was
  refused. No password was typed in any of it: the server was asked what it takes.
- 2026-11-11: the SD image `2026-11-11-ssh1` (fishball-p25 f7cefd0, tezuka_fw daadb44) on unit
  A, its boot files also in `/mnt/sd/boot_new_ssh1` (access2's in `/mnt/sd/boot_new_access2`).
  - **Checked before installing** (the ramdisk read on the PC): the scanner carries the tag;
    `/etc/default/dropbear` gives the server `-s` and the server's start script reads it; the
    image's key is this PC's, in a file of root's alone, and the same as access2's. Against
    access2's ramdisk nothing else differs but what differs at every build: the salt of the
    root password's hash (the same scheme, the same password as configured) and 16 bytes of
    the USB drive's image. `BOOT.bin` differs in 5 bytes, U-Boot's build time; the device tree
    is the same. The card's copies matched the build's sha256 before the reboot.
  - **After the reboot:** live on Clay County 103 s after it was asked, control decode 100 %;
    open as before, libiio off, HTTPS verified by the unit's CA. SSH offers `publickey` alone;
    a login by key works; a login without one is refused.
  - **The pages, A serving them,** reads only, over HTTP: `tool_pages.mjs` 27 steps,
    `files_page.mjs` 9, `now_rail.mjs` 15, `scanner_pages.mjs` 40; over HTTPS: `tool_pages.mjs`
    27, `files_page.mjs` 9, `now_rail.mjs` 15. `scanner/doc/API_FIELDS.md` regenerated from A.
  - The Files check's first run failed two steps: while it ran, another client downloaded the
    history's database and a TV recording from A (01:35 UTC in the radio's log, which does not
    name a client), and the page showed its links off, as it does while a download runs. Run
    again with none running, it passed.
- **Not checked:** the serial consoles, which is where the root password still works (that
  takes the unit in hand); unit B, which still has an older image and its password logins.
