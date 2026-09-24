# PS5 Logitech G Pro Research Links

This document collects the most relevant public references for the `rp-wheel-bridge` project.

Project goal:

- Input device: Logitech G Pro Xbox/PC wheel
- Target console: PlayStation 5
- Target game: Gran Turismo 7
- Desired result: PS5/GT7 recognizes the bridge as a native PlayStation Logitech G Pro wheel
- MCU: RP2350
- Firmware: Rust + Embassy
- USB host side: PIO USB
- USB device side: RP2350 native USB

---

## 0. Review notes (checked 2026-09-24)

Facts below were checked against the sources, the earlier `ps5-wheel-passthrough`
project and our own hardware logs. Items marked *unverified* still need a capture.

### PIDs (checked in SDL `initial_wheel_devices`)

```text
046d:c268  Logitech PRO Racing Wheel (PC mode)
046d:c269  Logitech PRO Racing Wheel (PS4/PS5 mode)
046d:c272  Logitech PRO Racing Wheel for Xbox (PC mode)
```

The earlier `ps5-wheel-passthrough` project presented **c268** to the PS5. Per SDL
that is the PS/PC wheel's *PC* mode; the console-mode ID is **c269**. Treat the old
project's descriptors (Report ID 1, feature 0x03 definition, output 0x05, auth
collection 0xFFF0 with F0/F1/F2/F3) as guesses to verify, not as the c269 layout.

### Physical c272 interface layout (TrueForce driver PROTOCOL_SPECIFICATION.md)

```text
IF0  game controller  EP 0x81 IN              30-byte input reports, 1 ms
IF1  HID++            EP 0x82 IN (+ SET_REPORT on EP0), 64-byte reports
IF2  force feedback   EP 0x83 IN / EP 0x03 OUT, 64-byte packets, report ID 0x01
```

- Input (30 bytes): buttons 0-3, wheel 4-5, accelerator 6-7, brake 8-9, clutch 10-11.
- FFB goes over IF2 EP 0x03 OUT, **not** HID++. Force in bytes 6-9, little-endian
  offset binary, 0x8000 = neutral.
- HID++ sub-devices on IF1: 0x01 rim/display, 0x02 pedals, 0x05 motor base, 0xFF
  wheel base.
- The spec lists c268 and c272 but says nothing about c269, PlayStation mode or
  authentication.

Our own host (rp-wheel-bridge `a25de5b`) enumerates c272 and streams IF0 reports
(~420/s while the wheel moves) and IF1 HID++ notifications.

### Proxy feasibility

- If c269 is c268/c272's protocol plus a PS auth layer (Phase 2's "ideal case"),
  IF0/IF1/IF2 traffic can be forwarded unchanged. Only the identity (PID,
  descriptors) and the auth reports are handled by the bridge.
- embassy-usb 0.6 device handlers (`Handler::control_in` / `control_out`,
  `hid::RequestHandler::get_report` / `set_report`) are **synchronous**. A control
  request from the PS5 cannot wait for a live answer from the wheel or the auth
  device. Either answer from a cache (descriptors, definition report, auth state
  polled through F2), or drive the lower-level async `embassy-usb-driver`
  `ControlPipe` ourselves.

---

## 0.1 DriveHub (owned; working reference for this exact goal)

The user owns a DriveHub. With it, a PS5 accepts the Xbox/PC G Pro (c272) as a
PlayStation G Pro. That makes it a working implementation of what this project
builds.

Per the DriveHub manual (compatibility page), non-native wheels on PS5 **require** a
licensed PS4-mode controller for authentication: HORI Wired Mini Gamepad, HORI
Fighting Commander OCTA (PS4 mode) or NACON Wired Compact Controller.

<https://collectiveminds.gitbook.io/drivehub/compatibility>

Community reports (section 8 below) mention GT7 showing "G Pro" through DriveHub
with steering not always working. For the user's setup this is solved by a specific
DriveHub **beta firmware** plus a **powered hub** feeding DriveHub enough power. The
user has a licensed auth pad. DriveHub's emulation is still evidence of what the PS5
accepts, not necessarily a byte-exact c269.

How to use it:

1. **Descriptor dump (no code changes).** Plug DriveHub, with the wheel and the auth
   controller attached, into the RP2350 USB-A port. The current firmware logs
   VID/PID, configuration, interfaces, endpoints, strings and HID report
   descriptors.
   - *Unverified:* DriveHub may present a different identity to a non-PS5 host.
2. **PS5 ⇄ DriveHub man-in-the-middle.** Connect PS5 → RP2350 (device) → RP2350
   (host) → DriveHub. This is the transparent proxy itself. It records the PS5
   enumeration sequence, GET_REPORT/SET_REPORT and auth traffic, and GT7
   startup/idle/FFB/TRUEFORCE traffic. Those are the items listed under
   "Current knowledge gaps".
3. **DriveHub ⇄ wheel man-in-the-middle.** Connect DriveHub → RP2350 (device,
   mirroring c272) → RP2350 (host) → G Pro. This shows how DriveHub initialises
   c272 and translates FFB.

### 0.2 DriveHub descriptor dump (2026-09-24, rp-wheel-bridge `53ea767`)

Setup: DriveHub (with the c272 wheel and the auth pad attached) connected directly
to the RP2350 USB-A port. Enumeration timed out twice at address 0 (DriveHub still
booting); the third attempt succeeded.

Device: **046d:c269**, bcdDevice 33.00, EP0 64, 1 configuration (98 bytes, attributes
0xC0, 200 mA).

Strings, verbatim including typos:

- Manufacturer: `Logitech `
- Product: `PRO Raccing Wheel for Playstation /PC`
- Serial: (redacted)

Compared with the real c272: bcdDevice 33.09, 91 bytes, 100 mA, product
`PRO Racing Wheel`.

| IF | DriveHub c269 | Physical c272 |
|---|---|---|
| 0 | HID, **0x01 OUT** 64/1 ms, **0x81 IN** 64/**5 ms**, report desc 193 B (DS4-style, below) | HID, 0x81 IN 64/1 ms, report desc 141 B (joystick, 30 B, no report ID) |
| 1 | HID++, **0x83 IN max 20**/5 ms, report desc 84 B | HID++, 0x82 IN 64/1 ms, report desc 84 B — **byte-identical** |
| 2 | FFB, **0x82 IN / 0x02 OUT** 64/1 ms, report desc 30 B | FFB, 0x83 IN / 0x03 OUT 64/1 ms, 30 B — identical except the last byte (DriveHub `00`, c272 `c0` End Collection) |

The IF1 and IF2 descriptors match c272 (0x10/0x11/0x12 HID++, and report 0x01 of
63 B in and out). Only the endpoint numbers and sizes differ, so these two
interfaces can be forwarded report for report.

IF0 report descriptor (193 bytes) is the DualShock 4 report layout plus the PS4
peripheral feature reports:

```text
0000: 05 01 09 05 a1 01 85 01 09 30 09 31 09 32 09 35
0010: 15 00 26 ff 00 75 08 95 04 81 02 09 39 15 00 25
0020: 07 35 00 46 3b 01 65 14 75 04 95 01 81 42 65 00
0030: 05 09 19 01 29 0d 15 00 25 01 75 01 95 0d 81 02
0040: 06 00 ff 09 20 75 07 95 01 81 02 05 01 09 33 09
0050: 34 15 00 26 ff 00 75 08 95 02 81 02 06 00 ff 09
0060: 21 95 36 81 02 85 05 09 22 95 1f 91 02 85 03 0a
0070: 21 27 95 2f b1 02 c0 06 f0 ff 09 40 a1 01 85 f0
0080: 09 47 95 3f b1 02 85 f1 09 48 95 3f b1 02 85 f2
0090: 09 49 95 0f b1 02 85 f3 0a 01 47 95 07 b1 02 c0
00a0: 05 01 09 04 a1 01 85 30 06 01 ff 09 02 95 07 91
00b0: 02 85 31 95 7e 75 10 05 10 19 01 2a ff ff b1 40
00c0: c0
```

- Game Pad collection:
  - Input 0x01, 63 B: LX LY RX RY, 4-bit hat, 13 buttons, 7-bit vendor field,
    L2 R2, then 54 vendor bytes.
  - Output 0x05, 31 B.
  - Feature 0x03 (usage 0x2721, 47 B): the controller definition.
- Auth collection, usage page 0xFFF0: F0 (63 B), F1 (63 B), F2 (15 B), F3 (7 B).
  These report IDs and sizes match what the old project guessed.
- Joystick collection: output 0x30 (7 B, vendor page 0xFF01) and feature 0x31
  (126 × 16 bit, Unicode usage page). Purpose unknown.

Input report 0x01 as observed (byte index includes the report ID):

```text
0      01 report ID
1-4    80 80 80 80 sticks, neutral
5      08 hat neutral (low nibble), buttons 1-4 (high nibble)
6      buttons 5-12
7      bit 0 button 13 (PS), bits 1-7 vendor (stays 0; first report had 01)
8-9    L2 R2 (00)
43-44  steering, uint16 LE, ~0x7ff3 centre (0x6e79..0x9110 seen while turning)
45-50  3 x uint16 ff ff: pedals, released = 0xffff (inverted)
51     00
52-53  ff ff: unknown
```

About 157 reports/s (bInterval 5 → at most 200/s).

**Consequence for the proxy:** IF1 (HID++) and IF2 (FFB) forward as-is. IF0 needs
one conversion: the c272 30-byte input becomes this 64-byte report. IF0 also carries
feature 0x03, F0-F3 and 0x30/0x31, plus output 0x05 on EP 0x01 OUT. Button/pedal
mapping and those reports still need to be captured.

### 0.3 IF0 input mapping, c272 → DriveHub c269 (2026-09-24)

Captured one control at a time: `direct.txt` is the c272 wired directly,
`drive_hub.txt` is the same wheel through DriveHub. Byte indices start at 0. For
c269 the index includes the report ID byte.

c272 30-byte layout, from its report descriptor:

```text
0 low hat   0 high btn1-4   1 btn5-12   2 btn13-20   3 btn21-28
4-5 X (wheel)   6-7 Rx (accel)   8-9 Ry (brake)   10-11 Rz (clutch)
12-17 Z, 0x36, 0x37 (0)   18-19 vendor (18 = 01)   20-27 btn29-92   28-29 Y (0)
```

Axes (all uint16 LE):

| control | c272 | c269 (DriveHub) |
|---|---|---|
| wheel | 4-5, 0x0000 left … 0xffff right | 43-44, same direction (e.g. 0x7e6d left, 0x8080 right of centre) |
| accel | 6-7, 0x0000 released … 0xffff | 45-46, **inverted**: 0xffff released |
| brake | 8-9, same | 47-48, inverted |
| clutch | 10-11, same | 49-50, inverted |

Buttons:

| control | c272 | c269 (DriveHub) | DS4 name |
|---|---|---|---|
| hat (0 up, 2 right, 4 down, 6 left, 8 none) | 0 low nibble | 5 low nibble | D-pad |
| A | 0 & 0x10 | 5 & 0x20 | Cross |
| X | 0 & 0x20 | 5 & 0x10 | Square |
| B | 0 & 0x40 | 5 & 0x40 | Circle |
| Y | 0 & 0x80 | 5 & 0x80 | Triangle |
| GEAR_UP (right paddle) | 1 & 0x01 | 6 & 0x02 | R1 |
| GEAR_DOWN (left paddle) | 1 & 0x02 | 6 & 0x01 | L1 |
| RT | 1 & 0x04 | 6 & 0x08 | R2 (button; analog byte 9 stays 0) |
| LT | 1 & 0x08 | 6 & 0x04 | L2 (button; analog byte 8 stays 0) |
| SHARE (View) | 1 & 0x10 | 6 & 0x10 | Share |
| MENU | 1 & 0x20 | 6 & 0x20 | Options |
| RSB | 1 & 0x40 | 6 & 0x80 | R3 |
| LSB | 1 & 0x80 | 6 & 0x40 | L3 |
| Xbox / PS | 3 & 0x80 | 7 & 0x01 | PS |
| right rotary CW | 3 & 0x02 | 54 & 0x04 | - |
| right rotary CCW | 3 & 0x04 | 54 & 0x02 | - |
| right rotary click | 3 & 0x08 | 54 & 0x01 | - |
| left rotary CW | 3 & 0x10 | 54 & 0x10 | - |
| left rotary CCW | 3 & 0x20 | 54 & 0x08 | - |
| left rotary click | 3 & 0x40 | 54 & 0x20 | - |

Other fixed c269 bytes: 1-4 sticks 0x80; 7 bits 1-7 = 0 (DriveHub doesn't count);
8-9 L2/R2 analog 0; 51 = 00; 52-53 = ff ff (unknown, maybe a released 4th axis);
everything else 0.

Confirmed by the user on DriveHub:

- The wheel is 0x0000 at full left and 0xffff at full right, the same scale as c272.
  So `c269 wheel = c272 wheel`.
- The pedals are 0xffff released and 0x0000 fully pressed. So
  `c269 pedal = 0xffff - c272 pedal`.
- The rotary bit order in byte 54 was recaptured with the same result, so the
  irregular order is DriveHub's real layout.
- c272 byte 2 never changed. No button tested lives there.

### 0.4 DriveHub feature reports (GET_REPORT, 2026-09-24)

Feature 0x03, the definition report (48 bytes including ID):

```text
0000: 03 21 27 04 10 06 00 00 00 00 00 00 00 00 00 00
0010: 00 00 0d 0d 00 00 00 00 9d 84 03 01 00 00 00 00
0020: 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

Against the old project's `PS5_DEFINITION_REPORT` (same ID-less offsets):

| byte (no ID) | DriveHub | old project |
|---|---|---|
| 2 | 04 | 03 |
| 3 | 10 | 00 |
| 23 | 9d | 0d |
| 26 | 01 | absent |
| 31 | 01 | absent |

Common to both: 21 27, controller type 06 at byte 4, 0d 0d at 17-18, 84 03 at
24-25.

Feature 0x31 (253 bytes including ID):

- Bytes 1-24: the serial number as UTF-16LE (redacted here). This matches the
  Unicode usage page in the descriptor.
- Byte 0x41: `05`.
- Byte 0x42 onward: the **IF2 descriptors**, in configuration-descriptor format:

  ```text
  09 04 02 00 02 03 00 00 00     interface 2, 2 EPs, HID
  09 21 11 01 00 01 22 1e 00     HID 1.11, report descriptor 30 bytes
  07 05 82 03 40 00 01           EP 0x82 IN  interrupt 64 B 1 ms
  07 05 02 03 40 00 01           EP 0x02 OUT interrupt 64 B 1 ms
  ```

- Everything after that is 0. This may be how the console finds the FFB interface.

### 0.5 DriveHub → wheel, captured through the bridge's c272 mirror (2026-09-24)

Setup: PS5 ← DriveHub ← [bridge: device = c272 mirror | host] ← wheel. The capture
is from a run where the wheel was already booted.

Wheel side:

- A freshly booted c272 switches itself off ~2 s after enumeration unless host
  software talks HID++ to it.
- G HUB does talk to it. A lone ping from the bridge was not enough, and results were
  mixed, so this is still open.

DriveHub's start-up, 30 ms after SET_CONFIGURATION (HID++ software ID 0xD):

1. SET_IDLE(0) on IF0, IF1 and IF2.
2. HID++ to device index 0xff (the base). Feature indexes are the base's (from G HUB's
   feature-set enumeration):

   ```text
   10 ff 00 1d            IRoot getProtocolVersion       -> 04 02 (4.2)
   10 ff 00 1d 00 00 39   ping
   10 ff 00 0d 00 03      getFeature 0x0003 (fw info)    -> index 0x02
   10 ff 02 1d            fw info
   10 ff 00 1d 00 00 f3   ping
   10 ff 00 0d 81 23      getFeature 0x8123              -> index 0x0e
   10 ff 16 2d 04 38      0x8138 (rotation) fn 2: set 0x0438 = 1080 degrees
   10 ff 00 1d 00 00 6e   ping
   10 ff 00 0d 80 7a      getFeature 0x807a              -> index 0x09
   10 ff 00 1d            ping
   ```

3. About 3 s later (3.68 s):

   ```text
   10 ff 0e 1d                                   0x8123 fn 1
   SET_REPORT 0x0212 on IF1:
   12 ff 0e 2d 00 86 00 00 00 00 33 32 33 32 00 00 00 00 33 32 33 32 ...
                                                 0x8123 fn 2 (write), wheel -> 12 ff 0e 2d 01
   ```

   Force feedback starts at the same moment.

Force feedback (IF2, report 0x01, 64 bytes):

- DriveHub → wheel, ~250 Hz: `01 00 00 00 <cmd> <seq> <u16 ...>`.
  - Examples: `01 00 00 00 05 01` first, then `01 00 00 00 0e 32 00 00 87 44`,
    `01 00 00 00 01 47 00 80 00 80`, and steady `01 00 00 00 02 d3 db 7f db 7f`.
  - Values near 0x7fxx/0x8000 are neutral.
- Wheel → DriveHub, ~440 Hz:
  `01 00 00 00 02 <seq> 10 60 00 ec 7f ec 7f <u32 counter?> ... 6a 98 00 00 6a 98 <ck>`.
- FFB flowed without a game running: DriveHub keeps the wheel under force control.

Notifications seen: `12 ff 1f 00 31/35/34` (feature index 0x1f = 0x812a).

GT7 driving (`captures/mirror_drive.txt`, race start ~49 s, driving 55-65 s):

- At race start, HID++ `10 ff 16 2d 03 84` sets the rotation to 0x0384 = 900 degrees
  (1080 before). The wheel echoes it and notifies `12 ff 16 00 03 84`.
- About 1-2 per second: SET_REPORT 0x0211 (long HID++) on IF1,
  `11 ff 09 6d 00 01 00 0a 00 NN` with NN = 04/06/08/0a. This is feature index 0x09
  (0x807a, LED effects) function 6 — **rev lights**, following the engine.
- All HID++ uses software ID 0xD, DriveHub's own. Whether GT7 talks HID++ and
  DriveHub re-issues it, or DriveHub generates it, needs the PS5 ⇄ DriveHub capture.
  So "the PS5 never uses IF1" only holds on the dashboard (without auth).
- Force feedback while driving is one command only, ~250 Hz:
  `01 00 00 00 02 <seq> <F lo> <F hi> <F lo> <F hi>`. It is the same u16 force twice,
  little-endian offset binary (0x8000 neutral). Seen range 0x5a20-0x8c30 (GT7 torque);
  idle before the race is steady 0x8075.
  - No other command types and no TRUEFORCE stream appeared while driving.
  - This matches PROTOCOL_SPECIFICATION.md (force in bytes 6-9).
- The wheel answers at about twice the rate (~490 Hz, status/position reports).
- IF0 input reached ~700 reports/s while driving. The mirror's 8-entry IF0 queue
  dropped ~2 % because DriveHub polls slower than that.

### 0.6 PS5 ⇄ DriveHub, captured through the bridge's relay (2026-09-24)

Setup: PS5 ← [bridge: device = c269 (DriveHub's descriptors, placeholder serial) |
host] ← DriveHub (c272 wheel + licensed auth pad). Auth goes through the bridge's
cache (`src/auth.rs`); everything else is passed through packet by packet. The PS5
accepted the device on the first try, and GT7 was driven.

The PS5's side, in order:

1. SET_CONFIGURATION, GET_REPORT feature 0x03 (48 B), SET_IDLE(0) on IF0/IF1/IF2.
   DriveHub STALLs SET_IDLE.
2. GET_REPORT feature 0x31 (253 B), 2.3 s later. The placeholder serial in it (and in
   the string descriptor) was accepted.
3. IF0 output report 0x30 (32 bytes on EP 0x01), classic Logitech commands:
   `f3`, `f4`, `f8 04 01`, `f8 12 00`, `f5`, `f8 04 01`, `f8 81 38 04` (range 1080),
   `f8 12 00`, `13`.
4. Force feedback on IF2 starts at the same time. See below.
5. Auth, about 3 s after connecting:

   ```text
   GET F3                       -> f3 00 38 38 00 00 00 00
   SET F0 x5 (1 s apart)        f0 <nonce id 01> <page 0..4> 00 <56 B> ...; last page
                                ends with 4 bytes (CRC?)
   (signer: F2 f2 01 10 ... "signing" -> f2 01 00 ... "ready" after 268 ms)
   GET F2 (2 s after last F0)   -> f2 01 00 ...
   GET F1 x19 (1 s apart)       f1 01 <page 0..18> 00 <56 B> 00 00 00 00
   ```

   The whole exchange takes ~27 s, paced by the PS5 at one page per second. The
   bridge's cache was ready 383 ms after the last nonce page. The auth layout in
   `src/auth.rs` (0x10 signing / 0x00 ready, 5 + 19 pages) is confirmed.
6. In GT7, via 0x30 again: `f8 04 01`, `f8 81 84 03` (range 900), then **rev LEDs**
   `f8 12 <mask>` with mask 0f/07/03/01 as the revs change.

**No HID++ at all** passed between the PS5 and DriveHub. On a c269, range and LEDs use
the classic G29-style 0x30 commands. DriveHub turns them into HID++ for the c272
(0x8138 set range, 0x807a function 6 for LEDs, §0.5).

Force feedback, PS5 → DriveHub (IF2 EP 0x02):

- The same native command format as DriveHub → c272
  (`01 00 00 00 02 <seq> <F16> <F16>`, ~250 Hz), starting with `01 00 00 00 05 01`.
- The packets are **cut short**: 12 bytes (7 and 9 for the first ones), not 64.
  DriveHub pads them with zeros to 64 for the wheel.
- DriveHub → PS5 (EP 0x82): 64-byte status reports, like the c272's IF2 IN.

Consequences for the bridge's own PS5 mode (c272 behind it):

- Pad FFB OUT to 64 bytes. Done.
- Translate 0x30 `f8 81 lo hi` into HID++ 0x8138 set range.
- Translate `f8 12 mask` into rev LEDs (0x807a fn 6). The mask → HID++ value mapping
  (`00 01 00 0a 00 NN`, NN 04/06/08/0a seen) still needs a correlated capture.
- `f3/f4/f5/13/f8 04 01` are G29 autocenter/force-slot commands. The native FFB stream
  makes them irrelevant, probably.

### 0.7 PS5 ⇄ bridge with the licensed pad as signer (2026-09-24)

Setup: GL850G hub on the bridge's USB-A port, c272 wheel + HORI Fighting Commander
OCTA (0f0d:0162, PS4 mode) behind it. The bridge presents c269 and relays auth to the
OCTA's HID interface 3 (same F0-F3 reports).

- 10 min of GT7 driving without a disconnect. G Pro recognized, steering and FFB OK.
- **The PS5 authenticates again and again**: a new round (GET F3, nonce id + 1)
  starts ~30 s after the previous round's last F1 page, i.e. every ~57 s.
- A round that never finishes (F2 stays "signing") is tolerated: the console started
  the next round ~70 s later without dropping the device.
- OCTA quirks:
  - F2/F1 carry the OCTA's own counter (04, 08, 0f, 13 ...) instead of the console's
    nonce id; the bridge rewrites byte 1 to the console's id (F1/F2 have no checksum,
    their last 4 bytes are zero).
  - Signing takes ~270 ms after the last nonce page.
  - It steps its F1 page on every GET it takes, even one whose reply the host never
    receives (timeout) or a resent SETUP. A lost page cannot be asked for again; the
    bridge checks the page number (byte 2) and signs the same nonce again.
  - F1 pages match the PS4 DS4 layout: page 13 ends with `01 00 01` (RSA exponent
    65537), data up to page 18.
- The wheel powers on by itself when the bridge boots (fresh USB host + immediate
  HID++ ping); nothing from the PS5 is needed for that.
- For auth, put the licensed pad behind the bridge as the signer (a hub or a second
  host port).

Both MITM setups need the native USB port for the device side. The CDC logger must
then move to a UART (or become part of a composite device).

---

## 1. Logitech G PRO Racing Wheel official product page

Official Logitech product information for the G PRO Racing Wheel.

Useful for:

- PlayStation/PC model identification
- 11 Nm torque specification
- TRUEFORCE support
- product/part-number information
- supported platforms

Link:

https://www.logitechg.com/en-us/products/driving/pro-racing-wheel.html

Known PlayStation/PC model part number:

```text
941-000175
```

---

## 2. SDL Logitech wheel VID/PID database

SDL contains explicit USB product IDs for Logitech wheels.

This is currently one of the most useful public references for distinguishing the different G Pro modes.

Link:

[https://github.com/libsdl-org/SDL/blob/main/src/joystick/SDL_joystick.c](https://github.com/libsdl-org/SDL/blob/main/src/joystick/SDL_joystick.c)

Relevant IDs:

```text
046d:c268
Logitech G Pro PlayStation/PC version
PC mode

046d:c269
Logitech G Pro PlayStation/PC version
PS4/PS5 mode

046d:c272
Logitech G Pro Xbox/PC version
PC mode
```

For this project:

```text
Physical input wheel:
046d:c272

Desired emulated PS5 device:
046d:c269
```

---

## 3. Logitech TrueForce Linux Driver

Open-source Linux driver supporting modern Logitech TrueForce wheels.

This is probably the most important reverse-engineering codebase for the G Pro.

Repository:

[https://github.com/mescon/logitech-trueforce-linux-driver](https://github.com/mescon/logitech-trueforce-linux-driver)

Useful for:

- G Pro input reports
- HID++
- force feedback
- TRUEFORCE
- wheel settings
- LEDs
- device initialization
- modern Logitech DD wheel protocol
- comparison between G Pro and RS50

The project supports devices including:

```text
G Pro PS/PC
G Pro Xbox/PC
RS50
```

---

## 4. G Pro / RS50 protocol specification

Detailed reverse-engineered protocol documentation from the TrueForce Linux driver project.

Link:

[https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/PROTOCOL_SPECIFICATION.md](https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/PROTOCOL_SPECIFICATION.md)

Important topics:

```text
USB interfaces
endpoint layout
HID++ features
input reports
FFB interface
64-byte interrupt packets
device initialization
wheel configuration
G Pro c268/c272 behavior
```

This should be treated as a primary implementation reference for the physical `c272` host-side driver.

---

## 5. TRUEFORCE protocol specification

Reverse-engineered TRUEFORCE protocol documentation.

Link:

[https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/TRUEFORCE_PROTOCOL.md](https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/TRUEFORCE_PROTOCOL.md)

Important topics:

```text
TRUEFORCE initialization
stream setup
packet format
audio/haptic streaming
G Pro / RS50 similarities
high-rate force/audio data
```

Potentially important for this project because the final bridge may be able to proxy native G Pro TRUEFORCE traffic rather than synthesize it.

Conceptual target:

```text
GT7 / PS5
    |
    | c269 TRUEFORCE
    v
RP2350 bridge
    |
    | minimal translation / direct forwarding
    v
physical G Pro c272
```

Whether the `c269` console-side packets are compatible with `c268/c272` still needs to be verified experimentally.

---

## 6. GTPlanet discussion: PlayStation authentication chip location

Discussion containing information from Logitech representative `LOGI_Rich`.

Link:

[https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-5](https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-5)

Important point:

The PlayStation version of the G Pro reportedly contains the Sony authentication hardware in the wheel base.

The Xbox version reportedly handles Xbox authentication differently, associated with the wheel/rim side.

This is very important for the bridge design because simply changing:

```text
VID/PID
046d:c272
    ->
046d:c269
```

is unlikely to be sufficient by itself.

PS authentication will probably need to be proxied or otherwise reproduced.

---

## 7. GTPlanet discussion: console device aggregation

Discussion regarding how Logitech peripherals are exposed to consoles.

Link:

[https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-80](https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-80)

Relevant concept:

On console, the wheel base is expected to expose the complete racing peripheral as a single logical device rather than exposing pedals, wheel, etc. as independent USB devices.

This supports the intended bridge architecture:

```text
Physical side

G Pro base
pedals
buttons
wheel rim
authentication device

       |
       v

RP2350

       |
       v

Single PS5-compatible G Pro device
```

---

## 8. GTPlanet discussion: DriveHub and GT7 detecting G Pro / G29

Useful real-world observations involving DriveHub.

Link:

[https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-167](https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-167)

There are reports where GT7 identifies an adapter-connected wheel differently depending on the emulated device.

Interesting observations include situations where:

```text
GT7 displays G Pro
buttons work
steering may not work correctly
```

This suggests that GT7 device recognition may depend on more than VID/PID.

Possible requirements may include:

```text
USB descriptors
HID report descriptor
feature reports
initialization sequence
authentication state
device-specific control requests
```

This is useful evidence that a `c269` implementation should reproduce the real device behavior as closely as possible.

---

## 9. GTPlanet discussion: Xbox/PC G Pro on PS5 with DriveHub

Community reports discussing use of the Xbox/PC G Pro on PlayStation through DriveHub and licensed controllers.

Link:

[https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-122](https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-122)

Useful as a practical reference for:

```text
Xbox/PC G Pro
+
DriveHub
+
licensed PS controller
+
PS5
```

Treat this as community evidence rather than formal protocol documentation.

---

## 10. Sony official PS5 specialty peripheral compatibility information

Official PlayStation documentation about PS4 peripherals on PS5.

Link:

[https://blog.playstation.com/2020/08/03/playstation-5-answering-your-questions-on-compatible-ps4-peripherals-accessories/](https://blog.playstation.com/2020/08/03/playstation-5-answering-your-questions-on-compatible-ps4-peripherals-accessories/)

Important concept:

Officially licensed PS4 specialty peripherals such as racing wheels may work with supported PS5 games.

This is relevant to authentication-proxy designs using a licensed specialty controller.

Possible architecture:

```text
             +--------------------+
             | licensed controller|
             | authentication     |
             +----------+---------+
                        |
                        v
G Pro c272 ---> RP2350 bridge ---> PS5
                        |
                        v
                 emulated c269
```

---

## 11. Logitech official game settings for PRO Wheels

Official Logitech game-specific settings page.

Link:

[https://support.logi.com/hc/ko/articles/8358055253271-In-Game-Settings-for-Pro-Wheels](https://support.logi.com/hc/ko/articles/8358055253271-In-Game-Settings-for-Pro-Wheels)

Useful for:

- GT7-specific G Pro behavior
- TRUEFORCE settings
- compatibility modes
- expected wheel-side configuration
- checking whether a game uses native PRO wheel support

This can help distinguish:

```text
G29 compatibility behavior

vs

native G Pro behavior
```

---

# Important known USB identities

```text
Logitech G Pro PlayStation/PC
PC mode:
VID 046d
PID c268

Logitech G Pro PlayStation/PC
PS4/PS5 mode:
VID 046d
PID c269

Logitech G Pro Xbox/PC
PC mode:
VID 046d
PID c272
```

Project direction:

```text
Physical G Pro
046d:c272

        |
        | USB Host
        v

RP2350
Rust / Embassy

        |
        | USB Device
        v

Emulated G Pro PS5 mode
046d:c269

        |
        v

PS5
        |
        v
Gran Turismo 7
```

---

# Current knowledge gaps

The largest missing piece is currently the PlayStation-side `046d:c269` protocol.

Public documentation for `c268` and `c272` is much better than for `c269`.

The following data would be especially valuable:

```text
c269 Device Descriptor
c269 Configuration Descriptor
c269 HID Report Descriptor
c269 interface layout
c269 endpoint layout

PS5 enumeration sequence

GET_DESCRIPTOR
GET_REPORT
SET_REPORT
vendor-specific control transfers

authentication challenge/response

GT7 startup traffic

GT7 idle traffic

GT7 driving FFB traffic

GT7 TRUEFORCE traffic

wheel initialization sequence
```

---

# Suggested research order

## Phase 1 — Physical G Pro c272

Use:

- TrueForce Linux driver
- PROTOCOL_SPECIFICATION.md
- TRUEFORCE_PROTOCOL.md

Implement:

```text
enumeration
input reports
HID++
FFB output
TRUEFORCE output
```

---

## Phase 2 — Understand c269 identity

Research or capture:

```text
USB descriptors
HID descriptors
feature reports
control transfers
initialization sequence
```

Compare:

```text
c268 PC mode
vs
c269 PS5 mode
```

The ideal case would be:

```text
c268 and c269
mostly identical Logitech protocol

+
PlayStation authentication layer
```

If true, the bridge implementation becomes significantly simpler.

---

## Phase 3 — Authentication

Determine whether authentication can be proxied from a licensed PS4 specialty controller.

Questions to answer:

```text
Is authentication generic?

Is it tied to peripheral class?

Is it tied to VID/PID?

Is it tied specifically to Logitech?

Is a device certificate involved?

Does PS5 verify descriptor identity against authentication identity?
```

---

## Phase 4 — GT7 native G Pro mode

Verify that GT7 actually selects its native G Pro profile.

Expected desired result:

```text
GT7
  recognizes Logitech G Pro
  uses native G Pro FFB
  enables TRUEFORCE
  exposes correct buttons
  exposes correct steering range
  exposes rev LEDs / wheel features
```

Avoid falling back to:

```text
G29 emulation
```

unless used purely as a debugging milestone.

---

# Highest-priority references

If only a few sources are going to be studied first, use this order:

1. SDL Logitech VID/PID list
   [https://github.com/libsdl-org/SDL/blob/main/src/joystick/SDL_joystick.c](https://github.com/libsdl-org/SDL/blob/main/src/joystick/SDL_joystick.c)

2. Logitech TrueForce Linux driver
   [https://github.com/mescon/logitech-trueforce-linux-driver](https://github.com/mescon/logitech-trueforce-linux-driver)

3. G Pro protocol specification
   [https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/PROTOCOL_SPECIFICATION.md](https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/PROTOCOL_SPECIFICATION.md)

4. TRUEFORCE protocol specification
   [https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/TRUEFORCE_PROTOCOL.md](https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/TRUEFORCE_PROTOCOL.md)

5. GTPlanet authentication-chip discussion
   [https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-5](https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-5)

6. Sony PS5 specialty peripheral compatibility information
   [https://blog.playstation.com/2020/08/03/playstation-5-answering-your-questions-on-compatible-ps4-peripherals-accessories/](https://blog.playstation.com/2020/08/03/playstation-5-answering-your-questions-on-compatible-ps4-peripherals-accessories/)

---

# Key implementation principle

Do not assume that emulating:

```text
VID = 0x046d
PID = 0xc269
```

is enough.

The implementation should aim to reproduce the real PlayStation G Pro as closely as possible:

```text
USB descriptors
HID report descriptors
endpoint layout
control requests
feature reports
initialization state machine
authentication
FFB protocol
TRUEFORCE protocol
```

The preferred architecture is a protocol proxy:

```text
PS5
 |
 | native c269 traffic
 v
RP2350
 |
 | minimal translation
 v
physical c272 G Pro
```

rather than converting G Pro behavior into a generic G29-style FFB model.
