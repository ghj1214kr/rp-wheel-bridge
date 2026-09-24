# rp-wheel-bridge

[English](#english) | [한국어](#한국어)

---

## English

Firmware for an RP2350 board that lets a **Logitech G PRO Racing Wheel for Xbox/PC**
(046d:c272) work on a **PlayStation 5** as the PlayStation-mode G PRO (046d:c269).
Written in pure Rust with [Embassy](https://embassy.dev); the USB host runs on the
RP2350's PIO.

Tested with Gran Turismo 7: the wheel is recognised as a G PRO, steering, pedals,
buttons, force feedback, TRUEFORCE and rev lights work, and authentication passes
through a licensed controller.

### How it works

```text
PS5 ── native USB ──► RP2350 ◄── PIO USB host ── USB hub ─┬─ G PRO (c272)
       (c269 device)                                      └─ licensed PS4 controller
                                                              (authentication)
```

- **Device side (native USB):** presents a c269 with the descriptors and feature
  reports of a PlayStation G PRO.
- **Host side (PIO USB, pure Rust):** drives the wheel and the auth controller
  through a hub.
- **Proxy:** HID++ and force feedback are forwarded as they are; only the input
  report is translated (30-byte c272 → 64-byte c269). The PS5's G29-style rev-light
  commands become the wheel's HID++ LED commands.
- **Authentication:** the PS5's challenge is relayed to a licensed PS4-mode
  controller and its signature is returned. The PS5 re-authenticates about every
  minute.

### Hardware

- Waveshare RP2350-USB-A (its USB-A port, on GPIO12/13, is the host port).
- Logitech G PRO Racing Wheel for Xbox/PC (c272).
- A licensed PS4-mode controller for authentication; tested with the HORI Fighting
  Commander OCTA in PS4 mode.
- A USB 2.0 hub (tested with a Genesys Logic GL850G) to connect both to the board's
  USB-A port. A powered hub is recommended; an unpowered one also ran for 30 minutes,
  with more retries towards the auth controller.
- Optional: a USB-UART adapter on GPIO0 (TX) / GPIO1 (RX), 921600 8N1, for logs.

### Build and flash

Requirements: Rust (stable, the toolchain in `rust-toolchain.toml` installs the
`thumbv8m.main-none-eabihf` target) and
[picotool](https://github.com/raspberrypi/picotool).

```sh
cargo build --release
# board in BOOTSEL mode:
cargo run --release      # picotool load --update --verify --execute
```

### Usage

1. Connect the wheel and the licensed controller to the hub, and the hub to the
   board's USB-A port.
2. Connect the board's native USB port to the PS5.
3. The wheel turns on and calibrates; the PS5 sees a G PRO.

The board's USB-A port has a pull-up (R13) fitted for device use, so the root port
never sees a detach: plug the hub in first, then (re)power the board. Devices behind
the hub can be plugged and unplugged freely.

### Limitations

- Occasional USB errors on the PIO bus are recovered automatically. The wheel has
  been seen to drop out of force feedback a few times; see
  [docs/usb-host.md](docs/usb-host.md#known-issues).
- TRUEFORCE needs vibration switched on for controller 1: with it off, GT7 sends
  force feedback without TRUEFORCE samples ([docs/ps5.md](docs/ps5.md#trueforce)).

### Documentation

[docs/](docs/README.md) collects what was found on the hardware: the PIO USB host
and its hardware-timed SOF, the c272 → c269 input mapping, the PS5 protocol and
authentication, and DriveHub captures.

### Disclaimer

Not affiliated with Logitech, Sony or Collective Minds. Authentication requires a
licensed controller that you own; the bridge only relays its answers.

### License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

---

## 한국어

**Logitech G PRO Racing Wheel(Xbox/PC용, 046d:c272)**을 **PlayStation 5**에서
PlayStation용 G PRO(046d:c269)로 쓸 수 있게 해 주는 RP2350 보드용 펌웨어입니다.
[Embassy](https://embassy.dev) 기반의 순수 Rust로 작성했고, USB 호스트는 RP2350의
PIO로 구현했습니다.

그란 투리스모 7에서 테스트했습니다. 휠이 G PRO로 인식되고 조향, 페달, 버튼, 포스
피드백, TRUEFORCE, 레브 LED가 동작하며, 인증은 정품 라이선스 컨트롤러를 거쳐 통과합니다.

### 동작 방식

```text
PS5 ── 네이티브 USB ──► RP2350 ◄── PIO USB 호스트 ── USB 허브 ─┬─ G PRO (c272)
       (c269 장치)                                              └─ 정품 PS4 컨트롤러
                                                                    (인증용)
```

- **장치 쪽(네이티브 USB):** PlayStation용 G PRO와 같은 디스크립터와 피처 리포트를
  가진 c269로 보입니다.
- **호스트 쪽(PIO USB, 순수 Rust):** 허브를 통해 휠과 인증용 컨트롤러를 다룹니다.
- **프록시:** HID++와 포스 피드백은 그대로 전달하고, 입력 리포트만 변환합니다
  (c272의 30바이트 → c269의 64바이트). PS5가 보내는 G29 방식의 레브 LED 명령은 휠의
  HID++ LED 명령으로 바꿉니다.
- **인증:** PS5의 인증 요청을 정품 PS4 모드 컨트롤러에 전달하고, 그 서명을 돌려줍니다.
  PS5는 약 1분마다 인증을 다시 합니다.

### 준비물

- Waveshare RP2350-USB-A (GPIO12/13의 USB-A 포트가 호스트 포트)
- Logitech G PRO Racing Wheel Xbox/PC용 (c272)
- 인증용 정품 PS4 모드 컨트롤러. HORI Fighting Commander OCTA(PS4 모드)로
  테스트했습니다.
- 두 장치를 보드의 USB-A 포트에 연결할 USB 2.0 허브. Genesys Logic GL850G로
  테스트했습니다. 전원 공급형 허브를 권장합니다. 무전원 허브로도 30분 동안 동작했지만
  인증용 컨트롤러와의 재시도가 더 잦았습니다.
- 선택: 로그용 USB-UART 어댑터. GPIO0(TX) / GPIO1(RX), 921600 8N1.

### 빌드와 플래시

필요한 것: Rust(stable. `rust-toolchain.toml`이 `thumbv8m.main-none-eabihf` 타깃을
설치합니다)와 [picotool](https://github.com/raspberrypi/picotool).

```sh
cargo build --release
# 보드를 BOOTSEL 모드로 연결한 뒤:
cargo run --release      # picotool load --update --verify --execute
```

### 사용법

1. 휠과 정품 컨트롤러를 허브에 연결하고, 허브를 보드의 USB-A 포트에 연결합니다.
2. 보드의 네이티브 USB 포트를 PS5에 연결합니다.
3. 휠이 켜지고 캘리브레이션을 마치면 PS5가 G PRO로 인식합니다.

보드의 USB-A 포트에는 장치용 풀업 저항(R13)이 달려 있어서 루트 포트는 분리를
감지하지 못합니다. 허브를 먼저 꽂은 다음 보드에 전원을 넣어(또는 다시 넣어)
주세요. 허브 뒤의 장치는 자유롭게 꽂고 뺄 수 있습니다.

### 한계

- PIO 버스의 간헐적인 USB 오류는 자동으로 복구됩니다. 휠이 포스 피드백에서 빠지는
  현상이 몇 번 있었습니다. [docs/usb-host.md](docs/usb-host.md#known-issues)를
  참고하세요.
- TRUEFORCE를 쓰려면 컨트롤러 1의 진동을 켜야 합니다. 꺼져 있으면 GT7은 TRUEFORCE
  샘플 없이 포스 피드백만 보냅니다([docs/ps5.md](docs/ps5.md#trueforce)).

### 문서

[docs/](docs/README.md)에 하드웨어에서 알아낸 내용을 정리했습니다(영어). PIO USB
호스트와 하드웨어 SOF, c272 → c269 입력 매핑, PS5 프로토콜과 인증, DriveHub 캡처
분석이 들어 있습니다.

### 면책

Logitech, Sony, Collective Minds와 관계없는 프로젝트입니다. 인증에는 직접 소유한
정품 컨트롤러가 필요하며, 브리지는 그 응답을 전달할 뿐입니다.

### 라이선스

[Apache License 2.0](LICENSE-APACHE) 또는 [MIT 라이선스](LICENSE-MIT) 중 하나를
선택해 따를 수 있습니다.
