# The PS5 side of a c269

What a PS5 (with Gran Turismo 7) exchanges with a PlayStation-mode G Pro, as seen
through the bridge: first relaying DriveHub, then as the c269 itself with a c272 behind
it. Authentication is in [ps5-auth.md](ps5-auth.md); the device identity in
[drivehub.md](drivehub.md#identity).

## Enumeration and start-up

1. SET_CONFIGURATION, GET_REPORT feature 0x03 (48 B), SET_IDLE(0) on IF0/IF1/IF2.
2. GET_REPORT feature 0x31 (253 B), a few seconds later. A placeholder serial
   (`000000000000`) in it and in the string descriptor is accepted.
3. IF0 output report 0x30 (32 bytes on EP 0x01), classic Logitech (G29-style)
   commands: `f3`, `f4`, `f8 04 01`, `f8 12 00`, `f5`, `f8 04 01`, `f8 81 38 04`,
   `f8 12 00`, `13`.
4. Force feedback on IF2 starts at the same time (below).
5. Authentication starts a few seconds after connecting and repeats every minute.

**No HID++ at all** is sent by the PS5. The c269's IF1 stays unused; everything the
wheel needs beyond force feedback comes as G29-style commands on IF0 output 0x30.

## IF0 output 0x30 commands

| command | meaning | bridge |
|---|---|---|
| `f8 81 <lo> <hi>` | wheel range in degrees: `38 04` = 1080 at start-up, `84 03` = 900 in GT7 | ignored: GT7 sets the range through force feedback type 0x0e, sent with it ([below](#steering-range)); DriveHub turns this into HID++ 0x8138 |
| `f8 12 <mask>` | rev lights, 5 LEDs as bits (01, 03, 07, 0f, 1f); GT7 flickers between neighbours at the shift point | translated to 0x807a function 6, level = 2 × lit LEDs |
| `f3`, `f4`, `f5`, `13`, `f8 04 01` | G29 autocenter / force-slot commands | ignored; the native FFB stream makes them irrelevant |

The rev-light command is `11 ff <0x807a index> 6b 00 01 00 0a 00 <level>` (SET_REPORT
0x0211 on IF1), level 0-10. How a level is drawn (center-out pairs, a sweep from one
side) is the wheel's LED profile. Per the
[TrueForce Linux driver's protocol notes](https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/PROTOCOL_SPECIFICATION.md),
this "level stream" needs no arming.

## Force feedback

IF2, report 0x01, in the G Pro's native format in both directions (force in bytes
6-9, as in the TrueForce driver's protocol notes), so it is forwarded unchanged.

- Console → wheel (EP 0x02 on the c269): `01 00 00 00 <cmd> <seq> ...`.
  - The packets are **cut short**: 12 bytes (6-16 during set-up), not 64, unless
    they carry TRUEFORCE samples (below). DriveHub and the bridge pad them with zeros
    to 64 for the c272.
  - While driving, one command only, ~770 per second:
    `01 00 00 00 01 <seq> <F16> <F16>`, the same force twice, uint16 LE offset binary
    (0x8000 neutral).
- Wheel → console (EP 0x82): 64-byte status reports, ~850 per second, e.g.
  `01 00 00 00 02 <seq> ...`.

Set-up sequence sent by GT7 right after start-up (logged in full since the bridge
records every non-stream command):

```text
01 00 00 00 05 01                       FFB on; wheel notifies HID++ 12 ff 1f 00 30
01 00 00 00 05 <seq> <param>            x48, params 30, 01..1d, 2b..3c; the wheel
                                        answers each with its value
01 00 00 00 0e 32 00 00 87 44
01 00 00 00 07 34                       wheel: 07 34 06 01 01 01 02 01 01 03 01 01 ...
01 00 00 00 06 36 01 01 / 06 38 01 02 / 06 3a 01 03
01 00 00 00 09 3c 02 02 00 00 80 3f 00 00 af 43
01 00 00 00 06 3e 01 04 01 / 06 40 01 05 / 06 42 01 06 01
01 00 00 00 0e 44 00 00 87 44
01 00 00 00 04 46
01 00 00 00 03 48
01 00 00 00 0c 4a 8f c2 75 3d
```

### Steering range

GT7 sets the wheel's rotation with FFB type 0x0e (`01 00 00 00 0e <seq> <f32 LE
degrees>`), forwarded unchanged: `00 00 87 44` = 1080 in the menus, and a
car-dependent range at race start (`00 00 61 44` = 900 for a road car; racing cars
get less). The wheel applies it at once and notifies the new value (`12 ff 16 00
<hi> <lo>`). The G29-style `f8 81` arrives at the same moments and is not needed.
Per the TrueForce driver's notes, a 0x0e push only takes effect while the stream is
started (after type 0x03).

The console sends the set-up only once. If the wheel drops out of force feedback later
(it notifies `12 ff 1f 00 20` and re-announces its rotation `12 ff 16 00 03 84`,
then STALLs FFB OUT), force feedback stays off; the one cause seen so far was on the
bridge's side, see [usb-host.md](usb-host.md#the-held-bus).

## TRUEFORCE

Per the [TRUEFORCE protocol notes](https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/TRUEFORCE_PROTOCOL.md),
TRUEFORCE rides on the same force packets: byte 10 is the number of new samples,
byte 11 a valid flag (0x0d) and the samples follow from byte 12. The G Pro has no USB
audio interface.

GT7 sends the TRUEFORCE start-up commands (05, 07, 06, 0e, 04, 03 above) in any
case, but the samples only while **vibration is on for controller 1**. With it off,
the force packets stay 12 bytes with byte 10 = 0. With it on, nearly every force
packet is a full 64 bytes with 4-5 new samples, about 4000 samples per second:

```text
01 00 00 00 01 0f c5 80 c5 80 05 0d 1f 80 1f 80 23 80 23 80 28 80 ...
               force       new valid  samples (u16 LE, duplicated) ...
```

Switching vibration on and off in a session starts and stops the samples. The
bridge forwards them unchanged and counts the force packets carrying samples in
its 5 s statistics (`TF n`).

The wheel's onboard TRUEFORCE level reaches the console on IF2: status report type
0x10 (`01 00 00 00 10 <seq> ...`) carries it in bytes 17-18, u16 LE (`ff ff` =
100 %). The wheel also notifies changes as HID++ `12 ff 17 10 <hi> <lo>`, which the
PS5 does not read.

## Behaviour worth knowing

- In the pits and in menus GT7 keeps sending force commands (with zero force), so
  the stream never pauses.
- A wheel that is switched off when the bridge powers up turns on by itself: it sees
  a fresh USB host, and the bridge talks HID++ to it at once. Nothing from the PS5 is
  needed for that.
