# IF0 input mapping: c272 → c269

The only interface whose format differs between the two wheels is IF0, the game
controller input. The bridge translates the G Pro Xbox/PC report (c272, 30 bytes)
into the report that the PlayStation-mode G Pro presents (c269, 64 bytes,
DualShock 4 style) in [`src/input_map.rs`](../src/input_map.rs).

The c269 layout was taken from DriveHub, which presents the same wheel as a c269.
Every control was captured one at a time, once with the wheel connected directly and
once through DriveHub. The raw reports are in [`data/`](data/):

- [`data/c272-controls.txt`](data/c272-controls.txt): the c272 on the bridge's host
  port.
- [`data/drivehub-c269-controls.txt`](data/drivehub-c269-controls.txt): the same
  wheel through DriveHub.

Each control there has two lines: pressed (or moved) and released.

Byte indices below start at 0. For c269 the index includes the report ID byte.

## c272 report (30 bytes, no report ID)

From its report descriptor:

```text
0      low nibble: hat; high nibble: buttons 1-4
1      buttons 5-12
2      buttons 13-20 (no button tested lives here)
3      buttons 21-28
4-5    X  (wheel)
6-7    Rx (accelerator)
8-9    Ry (brake)
10-11  Rz (clutch)
12-17  Z, 0x36, 0x37 (0)
18-19  vendor (18 = 01)
20-27  buttons 29-92
28-29  Y (0)
```

## c269 report 0x01 (64 bytes)

```text
0      01 report ID
1-4    80 80 80 80 sticks, neutral
5      low nibble: hat; high nibble: buttons 1-4
6      buttons 5-12
7      bit 0 button 13 (PS), bits 1-7 vendor (0)
8-9    L2 R2 analog (0)
43-44  wheel, uint16 LE
45-50  accelerator, brake, clutch, uint16 LE, 0xffff released
51     00
52-53  ff ff (unknown, maybe a released 4th axis)
54     rotary encoders
```

Everything else is 0.

## Axes

All axes are uint16 little-endian.

| control | c272 | c269 |
|---|---|---|
| wheel | 4-5, 0x0000 full left … 0xffff full right | 43-44, same scale |
| accelerator | 6-7, 0x0000 released … 0xffff pressed | 45-46, **inverted**: 0xffff released … 0x0000 pressed |
| brake | 8-9, same | 47-48, inverted |
| clutch | 10-11, same | 49-50, inverted |

So `c269 wheel = c272 wheel` and `c269 pedal = 0xffff - c272 pedal`.

## Buttons

| control | c272 | c269 | DS4 name |
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

The rotary bit order in byte 54 is irregular; it was captured twice with the same
result, so it is DriveHub's real layout.

## Rates

- c272: one report per change, up to ~1000/s (~700/s seen while driving).
- c269 as DriveHub presents it: EP 0x81, ~157 reports/s. The bridge keeps the latest
  translated report and sends it at every poll of the endpoint, like a DS4 does
  (~250/s on the PS5).
