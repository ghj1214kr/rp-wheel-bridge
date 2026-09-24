//! IF0 input translation: G Pro Xbox/PC (c272) 30-byte report → the 64-byte DS4-style
//! report 0x01 of the PlayStation-mode G Pro as DriveHub presents it (c269).
//!
//! Mapping captured control by control through DriveHub (docs/input-mapping.md).

/// Length of the c272 IF0 input report (no report ID).
pub const C272_LEN: usize = 30;

/// Length of the c269 IF0 input report, report ID included.
pub const C269_LEN: usize = 64;

/// (c272 bit, c269 bit) pairs within one byte.
type BitMap = [(u8, u8)];

/// c272 byte 0 high nibble → c269 byte 5 high nibble (Xbox face buttons at their DS4
/// positions).
const FACE: &BitMap = &[
    (0x10, 0x20), // A -> Cross
    (0x20, 0x10), // X -> Square
    (0x40, 0x40), // B -> Circle
    (0x80, 0x80), // Y -> Triangle
];

/// c272 byte 1 → c269 byte 6. c272 lists each pair right side first, DS4 left first.
const SHOULDER: &BitMap = &[
    (0x01, 0x02), // gear up (right paddle) -> R1
    (0x02, 0x01), // gear down (left paddle) -> L1
    (0x04, 0x08), // RT -> R2
    (0x08, 0x04), // LT -> L2
    (0x10, 0x10), // View -> Share
    (0x20, 0x20), // Menu -> Options
    (0x40, 0x80), // RSB -> R3
    (0x80, 0x40), // LSB -> L3
];

/// c272 byte 3 → c269 byte 54 (rotary encoders).
const ROTARY: &BitMap = &[
    (0x02, 0x04), // right CW
    (0x04, 0x02), // right CCW
    (0x08, 0x01), // right click
    (0x10, 0x10), // left CW
    (0x20, 0x08), // left CCW
    (0x40, 0x20), // left click
];

/// c272 byte 3 bit 7 (Xbox button) → c269 byte 7 bit 0 (PS button).
const XBOX_BUTTON: u8 = 0x80;
const PS_BUTTON: u8 = 0x01;

const REPORT_ID: u8 = 0x01;
const HAT_NEUTRAL: u8 = 0x08;
const STICK_CENTRE: u8 = 0x80;

/// The report sent before the wheel's first report: everything released, wheel
/// centred.
pub const fn neutral() -> [u8; C269_LEN] {
    let mut r = [0u8; C269_LEN];
    r[0] = REPORT_ID;
    r[1] = STICK_CENTRE;
    r[2] = STICK_CENTRE;
    r[3] = STICK_CENTRE;
    r[4] = STICK_CENTRE;
    r[5] = HAT_NEUTRAL;
    r[44] = 0x80; // wheel 0x8000
    // Pedals released (0xffff) and the unknown axis at 52-53, as DriveHub sends it.
    let mut i = 45;
    while i < 51 {
        r[i] = 0xff;
        i += 1;
    }
    r[52] = 0xff;
    r[53] = 0xff;
    r
}

/// Translate one c272 report.
pub fn translate(src: &[u8; C272_LEN]) -> [u8; C269_LEN] {
    let mut r = neutral();

    r[5] = (src[0] & 0x0f) | remap(src[0], FACE);
    r[6] = remap(src[1], SHOULDER);
    if src[3] & XBOX_BUTTON != 0 {
        r[7] = PS_BUTTON;
    }
    r[54] = remap(src[3], ROTARY);

    // Wheel: same scale (0x0000 full left .. 0xffff full right).
    r[43..45].copy_from_slice(&src[4..6]);
    // Accelerator, brake, clutch: c272 counts up when pressed, c269 counts down.
    for (dst, s) in [(45, 6), (47, 8), (49, 10)] {
        let v = u16::from_le_bytes([src[s], src[s + 1]]);
        r[dst..dst + 2].copy_from_slice(&(0xffff - v).to_le_bytes());
    }
    r
}

fn remap(byte: u8, map: &BitMap) -> u8 {
    map.iter()
        .filter(|&&(from, _)| byte & from != 0)
        .fold(0, |acc, &(_, to)| acc | to)
}
