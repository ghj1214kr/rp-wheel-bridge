//! Native USB device, in one of two personalities ([`PROFILE`]):
//!
//! - [`PS5`]: the PlayStation-mode G Pro (046d:c269), reproduced from what DriveHub
//!   presents (PS5_G_Pro_INFO.md §0.2-0.4): same descriptors, endpoint numbers and
//!   packet sizes, same feature reports. IF0 input is translated by the proxy.
//! - [`MIRROR_C272`]: the wheel itself (046d:c272), for putting the bridge between
//!   DriveHub and the wheel and recording what DriveHub sends. Everything is forwarded
//!   unchanged, class control requests included. Upstream requests sent before the
//!   wheel is enumerated wait in [`CONTROL_OUT`] (see [`MIRROR_WAIT_FOR_WHEEL`]).
//!
//! Data paths (the host side, [`crate::proxy`], fills and drains the queues):
//! - IF0 input: PS5, the latest translated wheel report ([`set_input`]), sent at every
//!   poll like a DS4 does; mirror, each wheel report ([`INPUT_IN`]).
//! - IF1 HID++: wheel reports → [`HIDPP_IN`] → IN endpoint; SET_REPORT → [`CONTROL_OUT`].
//! - IF2 force feedback: wheel reports → [`FFB_IN`] → IN endpoint; OUT → [`FFB_OUT`].
//! - PS5 only: IF0 feature 0x03/0x31 are answered with DriveHub's values. Auth reports
//!   (F0-F3), output 0x05/0x30 and anything else are logged; auth is not implemented yet.
//!
//! Control requests are answered synchronously (embassy-usb `Handler`), so nothing
//! here waits for the wheel.

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

use crate::hid::Hex;
use crate::input_map;

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

/// Everything that differs between the two personalities.
pub struct Profile {
    /// Log prefix.
    pub name: &'static str,
    /// Mirror the wheel: forward everything unchanged, connect once the wheel is up.
    pub mirror: bool,
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
#[allow(dead_code)] // one profile is selected at a time
pub static PS5: Profile = Profile {
    name: "PS device",
    mirror: false,
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

/// The G Pro Xbox/PC itself, as dumped from the wheel.
#[allow(dead_code)] // one profile is selected at a time
pub static MIRROR_C272: Profile = Profile {
    name: "mirror device",
    mirror: true,
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

/// Mirror only: the proxy signals this once the wheel is enumerated.
pub static WHEEL_READY: Signal<CS, ()> = Signal::new();

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
    if p.mirror && MIRROR_WAIT_FOR_WHEEL {
        // The D+ pull-up is enabled by `Builder::build`: stay off the bus until then.
        log::info!("{}: waiting for the wheel", p.name);
        WHEEL_READY.wait().await;
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
        log_if0_output(if0_out.as_mut()),
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

/// IF0 IN. PS5: the latest input report at every poll. Mirror: each wheel report.
async fn send_input(ep: &mut impl EndpointIn) -> ! {
    if PROFILE.mirror {
        forward_in(ep, &INPUT_IN, &STATS.input_sent).await
    }
    loop {
        ep.wait_enabled().await;
        while ep.write(&input()).await.is_ok() {
            count(&STATS.input_sent);
        }
    }
}

/// IF0 OUT (output report 0x05, PS5 only): not used by the wheel yet; logged.
async fn log_if0_output(ep: Option<&mut impl EndpointOut>) -> ! {
    let Some(ep) = ep else { pending().await };
    let mut buf = [0u8; MAX_PACKET];
    loop {
        ep.wait_enabled().await;
        while let Ok(n) = ep.read(&mut buf).await {
            log::info!("{}: IF0 OUT [{}]: {}", PROFILE.name, n, Hex(&buf[..n]));
        }
    }
}

/// Queue → IN endpoint, one report per transfer. A report that exactly fills its last
/// packet but is shorter than the longest report ([`MAX_PACKET`]) needs a zero-length
/// packet to end the transfer (e.g. a 20-byte HID++ report on a 20-byte endpoint);
/// a longest-size report ends by itself.
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
            if ep
                .write_transfer(report, report.len() < MAX_PACKET)
                .await
                .is_err()
            {
                break;
            }
            count(sent);
        }
    }
}

/// IF2 OUT (force feedback) → wheel.
async fn forward_ffb_out(ep: &mut impl EndpointOut) -> ! {
    let mut buf = [0u8; MAX_PACKET];
    loop {
        ep.wait_enabled().await;
        while let Ok(n) = ep.read(&mut buf).await {
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
                match (kind, id, req.index, feature) {
                    (REPORT_TYPE_FEATURE, _, IF_INPUT, Some(f)) => f,
                    (REPORT_TYPE_INPUT, 0x01, IF_INPUT, _) if !PROFILE.mirror => {
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
        // Mirror: every class request goes to the wheel. PS5: only HID++.
        if PROFILE.mirror || hidpp {
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
                let [kind, id] = req.value.to_be_bytes();
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
