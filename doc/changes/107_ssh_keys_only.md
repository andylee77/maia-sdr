# 107 — SSH by key only

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. The firmware's side is tezuka_fw's change 009.

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
