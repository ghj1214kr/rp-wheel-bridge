//! Native USB device, in one of three roles ([`PROFILE`]):
//!
//! - [`PS5`] ([`Role::Wheel`]): the PlayStation-mode G Pro (046d:c269), reproduced from
//!   what DriveHub presents (PS5_G_Pro_INFO.md §0.2-0.4): same descriptors, endpoint
//!   numbers and packet sizes, same feature reports; the c272 wheel behind it, its IF0
//!   input translated by [`crate::proxy`].
//! - [`RELAY_C269`] ([`Role::Relay`]): the same identity, with DriveHub (itself a c269)
//!   behind it instead of the wheel ([`crate::relay`]), to record what the PS5 and
//!   DriveHub exchange. Everything is passed through packet by packet, auth (F0-F3) via
//!   [`crate::auth`]. Connects only once DriveHub is enumerated.
//! - [`MIRROR_C272`] ([`Role::Mirror`]): the wheel itself (046d:c272), for putting the
//!   bridge between DriveHub and the wheel and recording what DriveHub sends. Everything
//!   is forwarded unchanged, class control requests included. Upstream requests sent
//!   before the wheel is enumerated wait in [`CONTROL_OUT`] (see
//!   [`MIRROR_WAIT_FOR_WHEEL`]).
//!
//! Data paths (the host side fills and drains the queues):
//! - IF0 input: wheel role, the latest translated wheel report ([`set_input`]), sent at
//!   every poll like a DS4 does; otherwise each backend report ([`INPUT_IN`]).
//! - IF0 output (0x05, 0x30): to the backend's side ([`IF0_OUT`]): relay, to DriveHub;
//!   wheel role, rev lights for the wheel (`crate::proxy`).
//! - IF1 HID++: backend reports → [`HIDPP_IN`] → IN endpoint; SET_REPORT → [`CONTROL_OUT`].
//! - IF2 force feedback: backend reports → [`FFB_IN`] → IN endpoint; OUT → [`FFB_OUT`].
//! - c269: IF0 feature 0x03/0x31 are answered with DriveHub's values; auth (F0-F3)
//!   through [`crate::auth`], signed by DriveHub (relay) or the licensed pad (wheel role).
//!
//! Control requests are answered synchronously (embassy-usb `Handler`), so nothing
//! here waits for the backend.

use core::cell::Cell;
use core::future::pending;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::join::{join, join5};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_usb::control::{InResponse, OutResponse, Recipient, Request, RequestType};
use embassy_usb::driver::{Direction, EndpointAddress, EndpointIn, EndpointOut};
use embassy_usb::{Builder, Config, Handler, UsbVersion};

use crate::hid::{GapMeter, Hex};
use crate::{auth, input_map};

/// The personality the native port presents.
pub static PROFILE: &Profile = &PS5;

/// Mirror mode: connect only once the wheel is enumerated. Off: with it on, DriveHub
/// saw no device for the ~5.5 s a booting wheel needs and power-cycled its port about
/// every 4.6 s (the bridge rebooted, the wheel restarted, the PS5's controller list was
/// reset).
const MIRROR_WAIT_FOR_WHEEL: bool = false;

/// Largest report on any endpoint.
pub const MAX_PACKET: usize = 64;

const CLASS_HID: u8 = 0x03;
const DESC_TYPE_HID: u8 = 0x21;
const DESC_TYPE_REPORT: u8 = 0x22;

pub const HID_SET_REPORT: u8 = 0x09;
const HID_GET_REPORT: u8 = 0x01;
const HID_GET_IDLE: u8 = 0x02;
const HID_GET_PROTOCOL: u8 = 0x03;
const HID_SET_IDLE: u8 = 0x0a;
const HID_SET_PROTOCOL: u8 = 0x0b;

const REPORT_TYPE_INPUT: u8 = 0x01;
const REPORT_TYPE_FEATURE: u8 = 0x03;

pub const IF_INPUT: u16 = 0;
pub const IF_HIDPP: u16 = 1;

/// An interrupt endpoint: number (without direction), max packet size, bInterval (ms).
#[derive(Clone, Copy)]
pub struct Ep(u8, u16, u8);

/// What is behind the device, and how it is forwarded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The c272 wheel; IF0 translated, HID++ and force feedback passed through.
    Wheel,
    /// A device with the same identity (DriveHub); everything passed through.
    Relay,
    /// The c272 wheel, presented as itself; everything passed through.
    Mirror,
}

/// Everything that differs between the personalities.
pub struct Profile {
    /// Log prefix.
    pub name: &'static str,
    pub role: Role,
    vid: u16,
    pid: u16,
    release: u16,
    manufacturer: &'static str,
    product: &'static str,
    serial: &'static str,
    max_power_ma: u16,
    /// bcdHID and report descriptor of IF0, IF1, IF2.
    hid: [(u16, &'static [u8]); 3],
    /// IF0's OUT endpoint, listed before its IN endpoint.
    if0_out: Option<Ep>,
    if0_in: Ep,
    if1_in: Ep,
    /// IF2: IN endpoint first, then OUT.
    if2_in: Ep,
    if2_out: Ep,
    /// IF0 feature reports answered locally (report ID first).
    features: &'static [&'static [u8]],
}

/// The PlayStation-mode G Pro as DriveHub presents it.
const C269: Profile = Profile {
    name: "PS device",
    role: Role::Wheel,
    vid: 0x046d,
    pid: 0xc269,
    release: 0x3300,
    // Strings verbatim from DriveHub, typos included; serial number replaced by a
    // placeholder (also in feature 0x31).
    manufacturer: "Logitech ",
    product: "PRO Raccing Wheel for Playstation /PC",
    serial: "000000000000",
    max_power_ma: 200,
    hid: [
        (0x0110, &C269_IF0_REPORT_DESC),
        (0x0111, &HIDPP_REPORT_DESC),
        (0x0111, &FFB_REPORT_DESC),
    ],
    if0_out: Some(Ep(1, 64, 1)),
    if0_in: Ep(1, 64, 5),
    // 20 bytes per 5 ms poll, as DriveHub (the wheel's own is 64 bytes, 1 ms).
    if1_in: Ep(3, 20, 5),
    if2_in: Ep(2, 64, 1),
    if2_out: Ep(2, 64, 1),
    features: &[&FEATURE_03, &FEATURE_31],
};

/// c269 with the wheel behind it.
#[allow(dead_code)] // one profile is selected at a time
pub static PS5: Profile = C269;

/// c269 with DriveHub behind it.
#[allow(dead_code)] // one profile is selected at a time
pub static RELAY_C269: Profile = Profile {
    name: "relay device",
    role: Role::Relay,
    ..C269
};

/// The G Pro Xbox/PC itself, as dumped from the wheel.
#[allow(dead_code)] // one profile is selected at a time
pub static MIRROR_C272: Profile = Profile {
    name: "mirror device",
    role: Role::Mirror,
    vid: 0x046d,
    pid: 0xc272,
    release: 0x3309,
    manufacturer: "Logitech ",
    product: "PRO Racing Wheel",
    // The wheel's own serial number is not reproduced.
    serial: "000000000000",
    max_power_ma: 100,
    hid: [
        (0x0111, &C272_IF0_REPORT_DESC),
        (0x0111, &HIDPP_REPORT_DESC),
        (0x0111, &FFB_REPORT_DESC),
    ],
    if0_out: None,
    if0_in: Ep(1, 64, 1),
    if1_in: Ep(2, 64, 1),
    if2_in: Ep(3, 64, 1),
    if2_out: Ep(3, 64, 1),
    features: &[],
};

/// c269 IF0: DS4 layout (gamepad report 0x01, output 0x05, definition feature 0x03), PS
/// auth collection (feature F0-F3), joystick collection (output 0x30, feature 0x31).
static C269_IF0_REPORT_DESC: [u8; 193] = [
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0x09, 0x39, 0x15, 0x00, 0x25,
    0x07, 0x35, 0x00, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0d, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0d, 0x81, 0x02,
    0x06, 0x00, 0xff, 0x09, 0x20, 0x75, 0x07, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x33, 0x09,
    0x34, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x06, 0x00, 0xff, 0x09,
    0x21, 0x95, 0x36, 0x81, 0x02, 0x85, 0x05, 0x09, 0x22, 0x95, 0x1f, 0x91, 0x02, 0x85, 0x03, 0x0a,
    0x21, 0x27, 0x95, 0x2f, 0xb1, 0x02, 0xc0, 0x06, 0xf0, 0xff, 0x09, 0x40, 0xa1, 0x01, 0x85, 0xf0,
    0x09, 0x47, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf1, 0x09, 0x48, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf2,
    0x09, 0x49, 0x95, 0x0f, 0xb1, 0x02, 0x85, 0xf3, 0x0a, 0x01, 0x47, 0x95, 0x07, 0xb1, 0x02, 0xc0,
    0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0x85, 0x30, 0x06, 0x01, 0xff, 0x09, 0x02, 0x95, 0x07, 0x91,
    0x02, 0x85, 0x31, 0x95, 0x7e, 0x75, 0x10, 0x05, 0x10, 0x19, 0x01, 0x2a, 0xff, 0xff, 0xb1, 0x40,
    0xc0,
];

/// c272 IF0: joystick, one 30-byte input report without report ID.
static C272_IF0_REPORT_DESC: [u8; 141] = [
    0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3b,
    0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x05, 0x09, 0x19, 0x01, 0x29, 0x1c, 0x65,
    0x00, 0x25, 0x01, 0x45, 0x01, 0x75, 0x01, 0x95, 0x1c, 0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x27,
    0xff, 0xff, 0x00, 0x00, 0x47, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0x09,
    0x33, 0x09, 0x34, 0x09, 0x35, 0x09, 0x32, 0x09, 0x36, 0x09, 0x37, 0x65, 0x00, 0x75, 0x10, 0x95,
    0x06, 0x81, 0x02, 0x06, 0x00, 0xff, 0x25, 0x01, 0x45, 0x01, 0x19, 0x00, 0x29, 0x0f, 0x75, 0x01,
    0x95, 0x10, 0x81, 0x02, 0x05, 0x09, 0x19, 0x1d, 0x29, 0x5c, 0x65, 0x00, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x40, 0x81, 0x02, 0x05, 0x01, 0x09, 0x31, 0x27, 0xff, 0xff, 0x00, 0x00, 0x47,
    0xff, 0xff, 0x00, 0x00, 0x65, 0x00, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0xc0,
];

/// IF1: HID++ short (0x10), long (0x11), very long (0x12). Identical on c272 and DriveHub.
static HIDPP_REPORT_DESC: [u8; 84] = [
    0x06, 0x43, 0xff, 0x0a, 0x01, 0x07, 0xa1, 0x01, 0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x15, 0x00,
    0x26, 0xff, 0x00, 0x09, 0x01, 0x81, 0x00, 0x09, 0x01, 0x91, 0x00, 0xc0, 0x06, 0x43, 0xff, 0x0a,
    0x02, 0x07, 0xa1, 0x01, 0x85, 0x11, 0x75, 0x08, 0x95, 0x13, 0x15, 0x00, 0x26, 0xff, 0x00, 0x09,
    0x02, 0x81, 0x00, 0x09, 0x02, 0x91, 0x00, 0xc0, 0x06, 0x43, 0xff, 0x0a, 0x04, 0x07, 0xa1, 0x01,
    0x85, 0x12, 0x75, 0x08, 0x95, 0x3f, 0x15, 0x00, 0x26, 0xff, 0x00, 0x09, 0x03, 0x81, 0x00, 0x09,
    0x03, 0x91, 0x00, 0xc0,
];

/// IF2: force feedback, report 0x01 in and out. The c272's version: DriveHub's ends in
/// 0x00 instead of End Collection (0xc0), which a strict HID parser rejects.
static FFB_REPORT_DESC: [u8; 30] = [
    0x06, 0xfd, 0xff, 0x0a, 0x01, 0xfd, 0xa1, 0x01, 0x85, 0x01, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
    0x08, 0x95, 0x3f, 0x09, 0x01, 0x81, 0x00, 0x95, 0x3f, 0x09, 0x01, 0x91, 0x00, 0xc0,
];

/// Class-specific HID descriptor (HID 1.11 §6.2.1), without bLength/bDescriptorType:
/// bcdHID, bCountryCode, bNumDescriptors, report descriptor type and length.
fn hid_descriptor(bcd_hid: u16, report_len: usize) -> [u8; 7] {
    let bcd = bcd_hid.to_le_bytes();
    let len = (report_len as u16).to_le_bytes();
    [bcd[0], bcd[1], 0x00, 0x01, DESC_TYPE_REPORT, len[0], len[1]]
}

/// c269 feature 0x03, the PS4 peripheral definition report (report ID included).
static FEATURE_03: [u8; 48] = [
    0x03, 0x21, 0x27, 0x04, 0x10, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x0d, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x9d, 0x84, 0x03, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// c269 feature 0x31 (report ID included): the serial number in UTF-16LE (here the
/// placeholder, as in the string descriptor), then IF2's interface, HID and endpoint
/// descriptors; zero after that.
static FEATURE_31: [u8; 253] = {
    const HEAD: [u8; 98] = [
        0x31, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00,
        0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x09, 0x04, 0x02, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00,
        0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x1e, 0x00, 0x07, 0x05, 0x82, 0x03, 0x40, 0x00,
        0x01, 0x07, 0x05, 0x02, 0x03, 0x40, 0x00, 0x01,
    ];
    let mut r = [0u8; 253];
    let mut i = 0;
    while i < HEAD.len() {
        r[i] = HEAD[i];
        i += 1;
    }
    r
};

/// One interrupt report (or HID++ message) in flight between the cores.
#[derive(Clone, Copy)]
pub struct Packet {
    len: u8,
    data: [u8; MAX_PACKET],
}

impl Packet {
    /// Copy of `data`, truncated to [`MAX_PACKET`].
    pub fn new(data: &[u8]) -> Self {
        let len = data.len().min(MAX_PACKET);
        let mut p = Self {
            len: len as u8,
            data: [0; MAX_PACKET],
        };
        p.data[..len].copy_from_slice(&data[..len]);
        p
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

/// A class, interface-recipient OUT control request to repeat to the wheel.
#[derive(Clone, Copy)]
pub struct ControlOut {
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub data: Packet,
}

type CS = CriticalSectionRawMutex;
const QUEUE_LEN: usize = 8;

/// HID++ to the PS5 drains slowly: DriveHub's EP 0x83 moves 20 bytes per 5 ms poll,
/// so a long report takes several polls, and G HUB's start-up burst (~200 messages in a
/// few seconds) overflowed 8 entries.
const HIDPP_IN_QUEUE_LEN: usize = 32;

pub static INPUT_IN: Channel<CS, Packet, QUEUE_LEN> = Channel::new();
pub static HIDPP_IN: Channel<CS, Packet, HIDPP_IN_QUEUE_LEN> = Channel::new();
pub static CONTROL_OUT: Channel<CS, ControlOut, QUEUE_LEN> = Channel::new();
pub static FFB_IN: Channel<CS, Packet, QUEUE_LEN> = Channel::new();
pub static FFB_OUT: Channel<CS, Packet, QUEUE_LEN> = Channel::new();
/// Relay: IF0 output reports from the console, for DriveHub.
pub static IF0_OUT: Channel<CS, Packet, QUEUE_LEN> = Channel::new();

/// The host side signals this once the device behind the bridge is enumerated (and,
/// for the relay, its feature reports are cached). Waited for by the relay, and by the
/// mirror if [`MIRROR_WAIT_FOR_WHEEL`].
pub static BACKEND_READY: Signal<CS, ()> = Signal::new();

static INPUT: Mutex<CS, Cell<[u8; input_map::C269_LEN]>> =
    Mutex::new(Cell::new(input_map::neutral()));

/// PS5: replace the IF0 input report sent from now on.
pub fn set_input(report: [u8; input_map::C269_LEN]) {
    INPUT.lock(|c| c.set(report));
}

fn input() -> [u8; input_map::C269_LEN] {
    INPUT.lock(Cell::get)
}

/// Device-side counters, logged by the proxy.
pub struct Stats {
    pub input_sent: AtomicU32,
    pub hidpp_sent: AtomicU32,
    pub hidpp_set_report: AtomicU32,
    pub ffb_sent: AtomicU32,
    pub ffb_received: AtomicU32,
    /// Messages lost because a queue was full.
    pub dropped: AtomicU32,
}

pub static STATS: Stats = Stats {
    input_sent: AtomicU32::new(0),
    hidpp_sent: AtomicU32::new(0),
    hidpp_set_report: AtomicU32::new(0),
    ffb_sent: AtomicU32::new(0),
    ffb_received: AtomicU32::new(0),
    dropped: AtomicU32::new(0),
};

pub fn count(counter: &AtomicU32) {
    counter.fetch_add(1, Ordering::Relaxed);
}

#[embassy_executor::task]
pub async fn task(driver: Driver<'static, USB>) -> ! {
    let p = PROFILE;
    let wait = match p.role {
        Role::Wheel => false,
        Role::Relay => true,
        Role::Mirror => MIRROR_WAIT_FOR_WHEEL,
    };
    if wait {
        // The D+ pull-up is enabled by `Builder::build`: stay off the bus until then.
        log::info!("{}: waiting for the device behind the bridge", p.name);
        BACKEND_READY.wait().await;
    }

    let mut config = Config::new(p.vid, p.pid);
    config.bcd_usb = UsbVersion::Two;
    config.composite_with_iads = false;
    config.device_class = 0x00;
    config.device_sub_class = 0x00;
    config.device_protocol = 0x00;
    config.device_release = p.release;
    config.manufacturer = Some(p.manufacturer);
    config.product = Some(p.product);
    config.serial_number = Some(p.serial);
    config.self_powered = true;
    config.max_power = p.max_power_ma;
    config.max_packet_size_0 = 64;

    let mut config_desc = [0u8; 128];
    // Feature 0x31 is the longest control transfer.
    let mut control_buf = [0u8; 256];
    let mut handler = Control;
    let mut builder = Builder::new(
        driver,
        config,
        &mut config_desc,
        &mut [],
        &mut [],
        &mut control_buf,
    );
    builder.handler(&mut handler);

    let hid_desc = p.hid.map(|(bcd, report)| hid_descriptor(bcd, report.len()));
    let mut func = builder.function(CLASS_HID, 0, 0);

    let mut iface = func.interface();
    let mut alt = iface.alt_setting(CLASS_HID, 0, 0, None);
    alt.descriptor(DESC_TYPE_HID, &hid_desc[0]);
    let mut if0_out = p
        .if0_out
        .map(|Ep(n, mps, ms)| alt.endpoint_interrupt_out(Some(ep(n, Direction::Out)), mps, ms));
    let Ep(n, mps, ms) = p.if0_in;
    let mut if0_in = alt.endpoint_interrupt_in(Some(ep(n, Direction::In)), mps, ms);

    let mut iface = func.interface();
    let mut alt = iface.alt_setting(CLASS_HID, 0, 0, None);
    alt.descriptor(DESC_TYPE_HID, &hid_desc[1]);
    let Ep(n, mps, ms) = p.if1_in;
    let mut if1_in = alt.endpoint_interrupt_in(Some(ep(n, Direction::In)), mps, ms);

    let mut iface = func.interface();
    let mut alt = iface.alt_setting(CLASS_HID, 0, 0, None);
    alt.descriptor(DESC_TYPE_HID, &hid_desc[2]);
    let Ep(n, mps, ms) = p.if2_in;
    let mut if2_in = alt.endpoint_interrupt_in(Some(ep(n, Direction::In)), mps, ms);
    let Ep(n, mps, ms) = p.if2_out;
    let mut if2_out = alt.endpoint_interrupt_out(Some(ep(n, Direction::Out)), mps, ms);
    drop(func);

    let mut usb = builder.build();
    log::info!("{}: {:04x}:{:04x} on native USB", p.name, p.vid, p.pid);

    join5(
        usb.run(),
        send_input(&mut if0_in),
        if0_output(if0_out.as_mut()),
        forward_in(&mut if1_in, &HIDPP_IN, &STATS.hidpp_sent),
        join(
            forward_in(&mut if2_in, &FFB_IN, &STATS.ffb_sent),
            forward_ffb_out(&mut if2_out),
        ),
    )
    .await;
    unreachable!("USB device futures never return")
}

fn ep(number: u8, dir: Direction) -> EndpointAddress {
    EndpointAddress::from_parts(usize::from(number), dir)
}

/// IF0 IN. Wheel role: the latest input report at every poll. Otherwise each backend
/// report.
async fn send_input(ep: &mut impl EndpointIn) -> ! {
    if PROFILE.role != Role::Wheel {
        forward_in(ep, &INPUT_IN, &STATS.input_sent).await
    }
    loop {
        ep.wait_enabled().await;
        while ep.write(&input()).await.is_ok() {
            count(&STATS.input_sent);
        }
    }
}

/// IF0 OUT (c269 output reports 0x05, 0x30). Relay: to DriveHub. Wheel role: not used
/// by the wheel yet. Logged when the content changes.
async fn if0_output(ep: Option<&mut impl EndpointOut>) -> ! {
    let Some(ep) = ep else { pending().await };
    let mut buf = [0u8; MAX_PACKET];
    let mut last = Packet::new(&[]);
    let mut repeats = 0u32;
    loop {
        ep.wait_enabled().await;
        while let Ok(n) = ep.read(&mut buf).await {
            let packet = Packet::new(&buf[..n]);
            if packet.bytes() == last.bytes() {
                repeats += 1;
            } else {
                log::info!(
                    "{}: IF0 OUT [{}] (+{} repeats): {}",
                    PROFILE.name,
                    n,
                    repeats,
                    Hex(packet.bytes())
                );
                last = packet;
                repeats = 0;
            }
            if PROFILE.role != Role::Mirror && IF0_OUT.try_send(packet).is_err() {
                count(&STATS.dropped);
            }
        }
    }
}

/// Queue → IN endpoint.
///
/// Relay: one packet per packet received from DriveHub, zero-length ones included (its
/// endpoints are the same as ours, so its framing is kept as is).
///
/// Otherwise one report per transfer. A report that exactly fills its last packet but
/// is shorter than the longest report ([`MAX_PACKET`]) needs a zero-length packet to end
/// the transfer (e.g. a 20-byte HID++ report on a 20-byte endpoint); a longest-size
/// report ends by itself.
async fn forward_in<const N: usize>(
    ep: &mut impl EndpointIn,
    queue: &'static Channel<CS, Packet, N>,
    sent: &AtomicU32,
) -> ! {
    loop {
        ep.wait_enabled().await;
        loop {
            let packet = queue.receive().await;
            let report = packet.bytes();
            let written = if PROFILE.role == Role::Relay {
                ep.write(report).await
            } else {
                ep.write_transfer(report, report.len() < MAX_PACKET).await
            };
            if written.is_err() {
                break;
            }
            count(sent);
        }
    }
}

/// IF2 OUT (force feedback) → wheel.
async fn forward_ffb_out(ep: &mut impl EndpointOut) -> ! {
    let mut buf = [0u8; MAX_PACKET];
    let mut gap = GapMeter::new(
        "FFB console -> bridge",
        embassy_time::Duration::from_millis(100),
    );
    loop {
        ep.wait_enabled().await;
        while let Ok(n) = ep.read(&mut buf).await {
            gap.tick();
            count(&STATS.ffb_received);
            if FFB_OUT.try_send(Packet::new(&buf[..n])).is_err() {
                count(&STATS.dropped);
            }
        }
    }
}

/// Standard interface GET_DESCRIPTOR (HID/report) and the HID class requests.
struct Control;

impl Handler for Control {
    fn reset(&mut self) {
        log::info!("{}: bus reset", PROFILE.name);
    }

    fn configured(&mut self, configured: bool) {
        log::info!("{}: configured = {}", PROFILE.name, configured);
    }

    fn suspended(&mut self, suspended: bool) {
        log::info!("{}: suspended = {}", PROFILE.name, suspended);
    }

    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        if req.recipient != Recipient::Interface {
            return None;
        }
        let [kind, id] = req.value.to_be_bytes();
        let data: &[u8] = match (req.request_type, req.request) {
            (RequestType::Standard, Request::GET_DESCRIPTOR) => {
                match (kind, PROFILE.hid.get(usize::from(req.index))) {
                    (DESC_TYPE_REPORT, Some((_, report))) => report,
                    _ => return reject_in(&req),
                }
            }
            (RequestType::Class, HID_GET_REPORT) => {
                let feature = PROFILE.features.iter().find(|f| f[0] == id);
                let auth_request = PROFILE.role != Role::Mirror
                    && kind == REPORT_TYPE_FEATURE
                    && req.index == IF_INPUT;
                if auth_request && let Some(n) = auth::get_report(id, buf) {
                    return Some(InResponse::Accepted(&buf[..n]));
                }
                match (kind, id, req.index, feature) {
                    (REPORT_TYPE_FEATURE, _, IF_INPUT, Some(f)) => f,
                    (REPORT_TYPE_INPUT, 0x01, IF_INPUT, _) if PROFILE.role == Role::Wheel => {
                        let report = input();
                        buf[..report.len()].copy_from_slice(&report);
                        &buf[..report.len()]
                    }
                    _ => return reject_in(&req),
                }
            }
            (RequestType::Class, HID_GET_IDLE) => {
                buf[0] = 0;
                &buf[..1]
            }
            (RequestType::Class, HID_GET_PROTOCOL) => {
                buf[0] = 1; // report protocol
                &buf[..1]
            }
            _ => return reject_in(&req),
        };
        if req.request_type == RequestType::Class {
            log::info!(
                "{}: GET_REPORT IF{} type {} id {:#04x} -> {} bytes",
                PROFILE.name,
                req.index,
                kind,
                id,
                data.len()
            );
        }
        Some(InResponse::Accepted(data))
    }

    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        if req.recipient != Recipient::Interface || req.request_type != RequestType::Class {
            return None;
        }
        let hidpp = req.request == HID_SET_REPORT && req.index == IF_HIDPP;
        if hidpp {
            count(&STATS.hidpp_set_report);
        }
        let [kind, id] = req.value.to_be_bytes();
        if PROFILE.role != Role::Mirror
            && req.request == HID_SET_REPORT
            && req.index == IF_INPUT
            && kind == REPORT_TYPE_FEATURE
            && auth::set_report(id, data)
        {
            return Some(OutResponse::Accepted);
        }
        // Relay and mirror: every class request goes to the backend. Wheel role: HID++.
        if PROFILE.role != Role::Wheel || hidpp {
            let msg = ControlOut {
                request: req.request,
                value: req.value,
                index: req.index,
                data: Packet::new(data),
            };
            if CONTROL_OUT.try_send(msg).is_err() {
                count(&STATS.dropped);
            }
            return Some(OutResponse::Accepted);
        }
        match req.request {
            HID_SET_IDLE | HID_SET_PROTOCOL => {}
            HID_SET_REPORT => {
                log::info!(
                    "{}: SET_REPORT IF{} type {} id {:#04x} [{}]: {}",
                    PROFILE.name,
                    req.index,
                    kind,
                    id,
                    data.len(),
                    Hex(data)
                );
            }
            _ => {
                log::warn!(
                    "{}: unhandled class OUT request {:#04x} value {:#06x} IF{}",
                    PROFILE.name,
                    req.request,
                    req.value,
                    req.index
                );
                return Some(OutResponse::Rejected);
            }
        }
        Some(OutResponse::Accepted)
    }
}

fn reject_in(req: &Request) -> Option<InResponse<'static>> {
    log::warn!(
        "{}: rejected IN request type {:?} {:#04x} value {:#06x} IF{} len {}",
        PROFILE.name,
        req.request_type,
        req.request,
        req.value,
        req.index,
        req.length
    );
    Some(InResponse::Rejected)
}
