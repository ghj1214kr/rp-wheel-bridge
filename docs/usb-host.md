# USB host side

The bridge's host port runs a full-speed USB host in software on the RP2350's PIO,
in pure Rust: `embassy-usb-host` → [`rp-pio-usb-host`] → embassy-rp PIO0. Everything
here was found on the hardware; most of it concerns timing.

[`rp-pio-usb-host`]: https://github.com/ghj1214kr/rp-pio-usb-host/tree/sof-during-transfers

## Board and wiring

Waveshare RP2350-USB-A:

| signal | pin | note |
|---|---|---|
| USB-A D+ | GPIO12 | 27 Ω series (R12) |
| USB-A D- | GPIO13 | 27 Ω series (R11) |
| log TX / RX | GPIO0 / GPIO1 | UART0, 921600 8N1 |

- **R13**, a 1.5 kΩ pull-up on D+ meant for PIO USB *device* use, is fitted on stock
  boards. With it, the root port always looks like a full-speed device is attached:
  a detach is never seen and low-speed devices cannot work. Plug the device (or hub)
  in first, then reset the RP2350. Behind a hub, attach and detach work normally.
- There are no host pull-down resistors on the board; the RP2350's internal
  pull-downs are used.

## Firmware layout

- `clk_sys` 192 MHz at a 1.15 V core voltage, so the PIO dividers for the TX (48 MHz)
  and RX (96 MHz) state machines are integers.
- **Core 0:** the native USB device (towards the PS5) and the UART logger.
- **Core 1:** only the PIO USB host, so its timing-critical transactions never share
  an executor or interrupts with the device stack.
- Logging goes to UART0 because the native USB port is the device side.

## rp-pio-usb-host fork

The bridge uses a fork (branch `sof-during-transfers`) with:

- **Hardware-timed SOF:** a PWM slice (7) wraps every 1 ms and paces a DMA channel
  (10) that sets PIO state machine 3's enable bit through the PIO `CTRL` set alias.
  SM3 holds the next SOF, prepared by the CPU between frames. SOFs sent by an
  interrupt were 20-400 µs late; USB allows ±0.5 µs, and a hub (GL850G) disabled its
  ports over the jitter (C_PORT_ENABLE storms every ~100 ms).
- **SOFs during transfers**, and transfers kept clear of the frame boundary.
- **Stage-by-stage control transfers:** SETUP is resent only without an ACK. Resending
  an ACKed SETUP restarts the request (a hub's port reset never completed).

### The SM0 pitfall

The hardware SOF at first never reached the wire. SM3 ran its SOF every frame, but
SM0 (the transfer player), left enabled and stalled after each packet, kept asserting
its side-set (idle J) on the same pins and overrode SM3 between SM3's own writes.
Devices kept busy by 1 ms polling did not notice; a hub did: without SOFs it went
silent after SET_FEATURE(PORT_RESET) and did not answer at address 0 after a bus
reset. The fix keeps SM0 disabled whenever it is idle (after each packet, and after a
bus reset). Found with a loopback capture of our own SOF and a probe of the PIO pad
debug registers.

## Hub

A GL850G hub (05e3:0610) lets the wheel and the auth pad share the one host port
([`src/hub.rs`](../src/hub.rs)):

- `embassy-usb-host`'s `HubHandler` handles port power and status changes.
- The port reset is the bridge's own: SET_FEATURE(PORT_RESET) with a 1 s timeout (the
  GL850G finishes that request only once the reset is done, ~25 ms), then GET_STATUS
  every 5 ms until the port is enabled, CLEAR_FEATURE(C_PORT_RESET), 10 ms recovery.
- Up to 3 devices, full speed only (no split transactions or low-speed PRE).
- A powered hub is recommended. An unpowered one also ran for 30 minutes, with more
  timeouts towards the auth pad (7 vs 2 in 10 minutes), all recovered by signing
  again.

## The wheel (c272)

Interfaces (see also the TrueForce driver's protocol notes, [references.md](references.md)):

```text
IF0  game controller  EP 0x81 IN              30-byte input reports, 1 ms
IF1  HID++            EP 0x82 IN (+ SET_REPORT on EP0), 64-byte reports
IF2  force feedback   EP 0x83 IN / EP 0x03 OUT, 64-byte packets, report ID 0x01
```

HID++ device indexes on IF1: 0x01 rim/display, 0x02 pedals, 0x05 motor base, 0xff
wheel base (the bridge talks to 0xff).

- **HID++ deadline:** a freshly booted G Pro switches itself off ~2 s after
  enumeration unless host software (G HUB on a PC) talks HID++ to it right after
  SET_CONFIGURATION. The bridge skips its descriptor probe for the wheel, sends
  SET_IDLE(0) on IF0-IF2 and then an HID++ ping (IRoot getProtocolVersion) every
  20 ms until the wheel answers (~20 ms). A wheel that is off turns on when the
  bridge boots.
- **LED feature:** the bridge asks for the index of feature 0x807a (getFeature) and
  drives the rev lights through it ([ps5.md](ps5.md)).
- **Feature table:** at start-up the bridge reads the wheel's feature table
  (IFeatureSet) and logs it at debug level. The indexes differ from the RS50 table in
  the TrueForce driver's notes. Settings and other features of interest:

  | index | feature | notes |
  |---|---|---|
  | 0x09 | 0x807a LED effects | rev lights |
  | 0x12 | 0x8133 damping | |
  | 0x13 | 0x8134 brake force | |
  | 0x14 | 0x8136 FFB strength | |
  | 0x15 | 0x8137 profile | notifies `05` on entering the onboard menu, `05 01` on leaving it |
  | 0x16 | 0x8138 rotation | `03 84` = 900 degrees |
  | 0x17 | 0x8139 TRUEFORCE | function 0 reads the level; notifies `12 ff 17 10 <hi> <lo>`, 0-0xffff = 0-100 % |
  | 0x18 | 0x8140 FFB filter | |
  | 0x1f | 0x812a | FFB state: `30` on, `20` off |

  The full table (0x01-0x23): 0001, 0003, 0005, 00c2, 1e00, 0009, 1bc0, 8040, 807a,
  807b, 80a4, 80d0, 8120, 8123, 8127, 8130, 8132, 8133, 8134, 8136, 8137, 8138, 8139,
  8140, 1802, 1806, 1830, 18b1, 1eb0, 8129, 812a, 92c0, 92d2, 92e1, 1801.
- The bridge's own HID++ requests use software ID 0xB; answers carrying it are not
  forwarded to the console.
- Notifications seen: `12 ff 1f 00 30` when the console switches force feedback on,
  `12 ff 1f 00 20` when it is off, `12 ff 16 00 03 84` = rotation 900 degrees, and
  the onboard settings above whenever they are changed on the wheel. They are
  forwarded; the PS5 does not use HID++.

## Known issues

- **Sporadic bus errors:** a few per minute, mostly garbled handshakes read as STALL
  (FFB IN, IF0 IN, the 1 s GET_STATUS liveness check) and lost OCTA replies. All are
  recovered automatically: IN endpoints are polled on, FFB OUT drops just the packet,
  auth signs again. A fork change that stopped the SOF-preparation interrupt from
  retrying during transfers and re-checked the RX start flag did not reduce them and
  was dropped.
- **Wheel dropping out of force feedback:** seen five times, each at the very moment
  the OCTA answered a nonce page (SET F0) with STALL, on both a powered and an
  unpowered hub; a SET F0 timeout never did it. The wheel NAKs and then STALLs
  FFB OUT, notifies `12 ff 1f 00 20` + `12 ff 16 00 03 84` about 0.5 s later (the
  notifications it sends at power-up, so its force feedback side seems to restart),
  and usually stops answering control transfers for good while its input reports go
  on. The console stops force feedback, and steering no longer works in the game.
  The bridge then clears the FFB OUT halt and replays the console's FFB set-up, which
  cannot help while the wheel's control endpoint is dead. Re-enumerating the wheel
  through a port reset was not tried. The cause is not known; the bridge logs pauses
  over 100 ms in the FFB streams and FFB packets taking over 20 ms to reach the
  wheel.
- Wheel range commands (`f8 81`) are not translated; the wheel keeps its own
  setting.
