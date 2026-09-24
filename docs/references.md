# References

## USB identities

| VID:PID | device |
|---|---|
| 046d:c268 | Logitech G PRO Racing Wheel for PlayStation/PC, PC mode |
| 046d:c269 | Logitech G PRO Racing Wheel for PlayStation/PC, PS4/PS5 mode |
| 046d:c272 | Logitech G PRO Racing Wheel for Xbox/PC, PC mode |

The bridge takes a c272 and presents a c269. (An earlier project presented c268,
which is the PS/PC wheel's *PC* mode.)

## Sources

- **SDL joystick database** — the VID/PIDs above:
  <https://github.com/libsdl-org/SDL/blob/main/src/joystick/SDL_joystick.c>
- **Logitech TrueForce Linux driver** (mescon) — G Pro input reports, HID++,
  force feedback, LEDs, initialisation:
  <https://github.com/mescon/logitech-trueforce-linux-driver>
  - Protocol specification (interfaces, endpoints, HID++ features, FFB, 0x807a
    LIGHTSYNC / rev-light level stream):
    <https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/PROTOCOL_SPECIFICATION.md>
  - TRUEFORCE protocol:
    <https://github.com/mescon/logitech-trueforce-linux-driver/blob/master/docs/TRUEFORCE_PROTOCOL.md>
- **GP2040-CE** and **jfedor2's wheel adapter** — the PS4/PS5 auth report layout
  (F0-F3) and relaying auth to a licensed controller.
- **DriveHub compatibility** (licensed PS4 controllers usable for authentication):
  <https://collectiveminds.gitbook.io/drivehub/compatibility>
- **Logitech G PRO Racing Wheel** product page:
  <https://www.logitechg.com/en-us/products/driving/pro-racing-wheel.html>
- **Logitech in-game settings for PRO wheels:**
  <https://support.logi.com/hc/ko/articles/8358055253271-In-Game-Settings-for-Pro-Wheels>
- **Sony: PS4 peripherals on PS5** (licensed specialty peripherals such as wheels
  work with supported PS5 games):
  <https://blog.playstation.com/2020/08/03/playstation-5-answering-your-questions-on-compatible-ps4-peripherals-accessories/>
- **GTPlanet G PRO thread** — community reports:
  - PlayStation auth hardware sits in the PS wheel's base (page 5):
    <https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-5>
  - On console the base exposes the whole wheel as one device (page 80):
    <https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-80>
  - GT7 detecting a DriveHub-adapted wheel as G Pro or G29 (page 167):
    <https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-167>
  - Xbox/PC G Pro on PS5 with DriveHub (page 122):
    <https://www.gtplanet.net/forum/threads/logitech-g-pro-racing-wheel.412554/page-122>
