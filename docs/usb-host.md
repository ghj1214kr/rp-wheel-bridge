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
- A powered hub is used.

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
- The bridge's own HID++ requests use software ID 0xB; answers carrying it are not
  forwarded to the console.
- Notifications seen: `12 ff 1f 00 30` when the console switches force feedback on,
  `12 ff 1f 00 20` when it is off, `12 ff 16 00 03 84` = rotation 900 degrees.

## Known issues

- **Sporadic bus errors:** a few per minute, mostly garbled handshakes read as STALL
  (FFB IN, IF0 IN, the 1 s GET_STATUS liveness check) and lost OCTA replies. All are
  recovered automatically: IN endpoints are polled on, FFB OUT drops just the packet,
  auth signs again. A fork change that stopped the SOF-preparation interrupt from
  retrying during transfers and re-checked the RX start flag did not reduce them and
  was dropped.
- **Wheel dropping out of force feedback:** seen four times: three times right after
  the OCTA refused a nonce page, once 0.5 s after the console's FFB set-up, before any
  auth. The wheel notifies `12 ff 1f 00 20` + `12 ff 16 00 03 84`, STALLs FFB OUT,
  and the console stops force feedback for good. Not seen in the last two 10-minute
  drives. The bridge logs any pause over 100 ms in the FFB streams (console → bridge,
  wheel → bridge) and any FFB packet taking over 20 ms to reach the wheel, to find
  out whether the wheel has a watchdog on the force stream.
- Wheel range commands (`f8 81`) are not translated; the wheel keeps its own
  setting.
