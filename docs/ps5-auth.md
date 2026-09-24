# PS5 authentication

A PS5 only keeps a third-party peripheral if it can prove it is licensed. The console
sends a nonce, the peripheral signs it and returns the signature together with its
certificate. The bridge cannot sign; it relays the challenge to a device that can,
a **licensed PS4-mode controller** (the "signer"), in [`src/auth.rs`](../src/auth.rs).

Tested signers:

- **HORI Fighting Commander OCTA** (0f0d:0162) in PS4 mode, behind a USB hub on the
  bridge's host port, next to the wheel. Its auth reports are on HID interface 3.
- **DriveHub** (in the relay role), which relays to its own auth pad.

## Reports

Feature reports on IF0 of the c269, report IDs as in the DualShock 4 (and in
GP2040-CE / jfedor2's wheel adapter):

| report | direction | size (incl. ID) | content |
|---|---|---|---|
| F3 | GET | 8 | reset; answers `f3 00 38 38 00 00 00 00` |
| F0 | SET ×5 | 64 | `f0 <nonce id> <page 0-4> 00 <56 bytes>`; the last page ends with 4 check bytes |
| F2 | GET | 16 | `f2 <nonce id> <state> 00 ...`, state `0x10` signing, `0x00` ready |
| F1 | GET ×19 | 64 | `f1 <nonce id> <page 0-18> 00 <56 bytes> 00 00 00 00` |

The 19 F1 pages (1064 bytes) follow the PS4 DS4 layout: signature, serial, public
key, exponent (page 13 ends with `01 00 01`, 65537), certificate signature; data
ends in page 18. F1 and F2 carry no checksum.

## Flow

```text
console                         bridge                         signer
GET F3            ------------> (answers from cache) --------> GET F3 (reset)
SET F0 page 0..4  (1 s apart) > accepted at once ------------> SET F0 page 0..4
GET F2 (every 1 s) <----------- "signing" until cached
                                                   <---------- F2 ready (~270 ms)
                                                   <---------- GET F1 x19
GET F2            <------------ "ready"
GET F1 x19 (1 s apart) <------- cached pages
```

- The whole round takes ~27 s, paced by the console at one page per second.
- **The console authenticates again and again**: a new round (nonce id + 1) starts
  ~30 s after the previous round's last F1 page, about every minute.
- A round that never finishes (F2 stays "signing") is tolerated: the console starts
  the next round later without dropping the device. In a test where every round
  failed for 7 minutes the device was not dropped either.

embassy-usb control handlers are synchronous: a GET_REPORT from the console must be
answered on the spot. So the bridge accepts each F0 immediately and hands it to the
host side, answers F2 with "signing" until all 19 signature pages are cached, and
serves F1 from the cache. Only that busy F2 is made up; everything else is the
signer's own bytes (with the nonce id rewritten, below).

## HORI OCTA quirks

- **Own counter:** F2/F1 carry the OCTA's own counter (04, 08, 0f, 13, ...) instead
  of the console's nonce id. The bridge rewrites byte 1 to the console's id; F1/F2
  have no checksum.
- **Signing time:** ~250-270 ms after the last nonce page.
- **Page pointer:** it steps its F1 page on every GET it takes, including one whose
  reply the host never receives (a 500 ms timeout) or a SETUP resent after a garbled
  ACK. A lost page cannot be asked for again. The bridge checks the page number
  (byte 2) and, if a page is missing, signs the same nonce again: 300 ms pause, F3
  reset, the 5 nonce pages again (10 ms apart), at most 4 attempts.
- **Stale ready:** right after a nonce is sent again, F2 may still say "ready" from
  the previous signing. The bridge believes "ready" only after it has seen
  "signing" (or after 0.5 s).
- **Refusals:** it now and then STALLs or times out a SET F0 (at least some of the
  STALLs were the host's own doing, see [usb-host.md](usb-host.md#lost-handshakes)). The refused page is
  sent again with all the others once the last page is in.
- **Overload:** after many signings in a row (10 attempts per round) it once stayed
  "signing" for good, round after round, until it was unplugged. Hence the low
  attempt count and the pauses.

With these in place, 12 of 12 rounds succeeded in an 11-minute drive.
