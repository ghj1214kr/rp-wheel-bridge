# rp-wheel-bridge documentation

What was found on the hardware while building the bridge.

| document | contents |
|---|---|
| [usb-host.md](usb-host.md) | Board wiring, the PIO USB host and its hardware-timed SOF, the hub, waking the G Pro, known issues |
| [input-mapping.md](input-mapping.md) | c272 → c269 input report translation, with the raw captures in [`data/`](data/) |
| [ps5.md](ps5.md) | What the PS5 sends a c269: start-up, G29-style commands (range, rev lights), force feedback |
| [ps5-auth.md](ps5-auth.md) | PS4/PS5 peripheral authentication and relaying it to a licensed controller (HORI OCTA quirks) |
| [drivehub.md](drivehub.md) | DriveHub's c269 descriptors and feature reports, and how it drives the c272 |
| [references.md](references.md) | USB IDs and outside sources |
