# DriveHub as a reference

[DriveHub](https://collectiveminds.gitbook.io/drivehub/compatibility) (Collective
Minds) makes a PS5 accept the Xbox/PC G Pro (046d:c272) as a PlayStation G Pro. It
is a working implementation of what this project builds, so its behaviour was
captured with the bridge in three setups (see [`src/device.rs`](../src/device.rs)
for the roles):

1. **Descriptor dump:** DriveHub (with the wheel and an auth pad) on the bridge's
   host port.
2. **Mirror:** DriveHub → bridge (device side presents c272) → bridge (host side) →
   wheel. Shows how DriveHub drives the c272.
3. **Relay:** PS5 → bridge (device side presents c269) → bridge (host side) →
   DriveHub. Shows what the PS5 exchanges with a c269. See [ps5.md](ps5.md) for the
   console side and [ps5-auth.md](ps5-auth.md) for authentication.

Per DriveHub's manual, non-native wheels on PS5 **require** a licensed PS4-mode
controller for authentication (e.g. HORI Wired Mini Gamepad, HORI Fighting Commander
OCTA in PS4 mode, NACON Wired Compact Controller).

DriveHub's emulation is evidence of what the PS5 accepts, not necessarily a
byte-exact copy of a real c269.

## Identity

DriveHub presents **046d:c269**, bcdDevice 33.00, EP0 64 bytes, one configuration
(98 bytes, attributes 0xC0, 200 mA).

Strings, verbatim including the typos:

- Manufacturer: `Logitech `
- Product: `PRO Raccing Wheel for Playstation /PC`
- Serial: a 12-character serial (the bridge uses the placeholder `000000000000`,
  which the PS5 accepts).

For comparison, the real c272: bcdDevice 33.09, 91 bytes, 100 mA, product
`PRO Racing Wheel`.

| IF | DriveHub c269 | Physical c272 |
|---|---|---|
| 0 | HID, **0x01 OUT** 64/1 ms, **0x81 IN** 64/**5 ms**, report descriptor 193 B (DS4-style) | HID, 0x81 IN 64/1 ms, report descriptor 141 B (joystick, 30 B, no report ID) |
| 1 | HID++, **0x83 IN max 20**/5 ms, report descriptor 84 B | HID++, 0x82 IN 64/1 ms, report descriptor 84 B, **byte-identical** |
| 2 | FFB, **0x82 IN / 0x02 OUT** 64/1 ms, report descriptor 30 B | FFB, 0x83 IN / 0x03 OUT 64/1 ms, 30 B, identical except the last byte (DriveHub `00`, c272 `c0` End Collection) |

IF1 and IF2 carry the same reports as the c272 (HID++ 0x10/0x11/0x12, and report
0x01 of 63 B in and out); only the endpoint numbers and sizes differ, so they can be
forwarded report for report. IF0 needs a translation, see
[input-mapping.md](input-mapping.md).

### IF0 report descriptor (193 bytes)

The DualShock 4 layout plus the PS4 peripheral feature reports:

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
  - Input 0x01, 63 B: LX LY RX RY, 4-bit hat, 13 buttons, 7-bit vendor field, L2 R2,
    then 54 vendor bytes.
  - Output 0x05, 31 B.
  - Feature 0x03 (usage 0x2721, 47 B): the controller definition.
- Auth collection, usage page 0xFFF0: F0 (63 B), F1 (63 B), F2 (15 B), F3 (7 B).
- Joystick collection: output 0x30 (7 B, vendor page 0xFF01; the PS5 sends G29-style
  commands here, see [ps5.md](ps5.md)) and feature 0x31 (126 × 16 bit, Unicode usage
  page).

## Feature reports

Feature 0x03, the definition report (48 bytes including the ID):

```text
0000: 03 21 27 04 10 06 00 00 00 00 00 00 00 00 00 00
0010: 00 00 0d 0d 00 00 00 00 9d 84 03 01 00 00 00 00
0020: 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

Controller type 0x06 at byte 5; `0d 0d` at 18-19; `84 03` at 25-26.

Feature 0x31 (253 bytes including the ID):

- Bytes 1-24: the serial number as UTF-16LE.
- Byte 0x41: `05`.
- Byte 0x42 onward: the **IF2 descriptors**, in configuration-descriptor format:

  ```text
  09 04 02 00 02 03 00 00 00     interface 2, 2 endpoints, HID
  09 21 11 01 00 01 22 1e 00     HID 1.11, report descriptor 30 bytes
  07 05 82 03 40 00 01           EP 0x82 IN  interrupt 64 B 1 ms
  07 05 02 03 40 00 01           EP 0x02 OUT interrupt 64 B 1 ms
  ```

- Everything after that is 0. Probably how the console finds the FFB interface.

The auth reports F0-F3 are covered in [ps5-auth.md](ps5-auth.md).

## DriveHub → wheel

Captured through the mirror setup with a wheel that had already booted.

Start-up, 30 ms after SET_CONFIGURATION (HID++ software ID 0xD):

1. SET_IDLE(0) on IF0, IF1 and IF2.
2. HID++ to device index 0xff (the base):

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

3. About 3.7 s later:

   ```text
   10 ff 0e 1d                                   0x8123 fn 1
   SET_REPORT 0x0212 on IF1:
   12 ff 0e 2d 00 86 00 00 00 00 33 32 33 32 00 00 00 00 33 32 33 32 ...
                                                 0x8123 fn 2 (write), wheel -> 12 ff 0e 2d 01
   ```

   Force feedback starts at the same moment and keeps the wheel under force control
   even without a game running.

In GT7 (DriveHub turns the PS5's G29-style commands into HID++):

- Race start: `10 ff 16 2d 03 84` sets the rotation to 0x0384 = 900 degrees. The
  wheel echoes it and notifies `12 ff 16 00 03 84`.
- Rev lights, 1-2 per second, SET_REPORT 0x0211 on IF1:
  `11 ff 09 6d 00 01 00 0a 00 NN` = 0x807a (LED effects) function 6, rev-light level
  NN of 10. Only NN = 04/06/08/0a were seen: DriveHub lights up only at high revs.
  Its exact mapping from the PS5's 5-LED mask is not known.

Force feedback is the native format both ways; see [ps5.md](ps5.md#force-feedback).

Notifications seen from the wheel: `12 ff 1f 00 31/35/34` (feature index 0x1f =
0x812a).

## DriveHub quirks

- Enumeration at address 0 timed out twice while DriveHub was still booting.
- DriveHub STALLs SET_IDLE from the PS5.
- Steering not working in GT7 through DriveHub is a known community report; for the
  author it was fixed by a DriveHub beta firmware plus a powered hub feeding it enough
  power.
