//! USB host on the RP2350-USB-A's USB-A port.
//!
//! Stack: `embassy-usb-host` -> `rp-pio-usb-host` -> embassy-rp PIO0 -> GPIO12/GPIO13.
//!
//! Waveshare RP2350-USB-A wiring (schematic):
//! - GPIO12 = D+ (27 Ω series R12), GPIO13 = D- (27 Ω series R11)
//! - R13 = 1.5 kΩ pull-up on D+ (fitted on stock boards, meant for PIO USB *device* mode)
//! - no 15 kΩ host pull-downs on the board, so the RP2350 internal pull-downs are used
//!
//! With R13 fitted, D+ always looks like a full-speed device, so attach is detected at
//! boot even with nothing plugged in and detach is never seen. Plug the device in first,
//! then reset the RP2350.
//!
//! Scope: one full-speed device directly on the port, or a hub with full-speed devices
//! behind it ([`crate::hub`]; the wheel and the licensed auth pad together).
//!
//! SOFs: the G Pro powers off about a second after SET_CONFIGURATION unless the SOF
//! period is steady (USB 2.0 §7.1.12: 1.000 ms ± 0.5 µs); SOFs sent by an interrupt
//! were tens to hundreds of microseconds late. The binary therefore enables the fork's
//! hardware SOF (`Bus::enable_hw_sof`: a PWM wrap triggers a DMA write that starts a
//! pre-loaded PIO state machine), and the frame-timer interrupt only prepares the next
//! SOF.

use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use embassy_futures::select::{Either, select};
use embassy_rp::Peri;
use embassy_rp::interrupt::typelevel::{Binding, PIO0_IRQ_0};
use embassy_rp::peripherals::{PIN_12, PIN_13, PIO0};
use embassy_rp::pio::InterruptHandler;
use embassy_usb_driver::host::{
    DeviceEvent, HostError, UsbHostAllocator, UsbHostController, UsbPipe, pipe,
};
use embassy_usb_driver::{Direction, EndpointAddress, EndpointInfo, EndpointType, Speed};
use embassy_usb_host::control::SetupPacket;
use embassy_usb_host::descriptor::{
    ConfigurationDescriptor, DescriptorVisitor, DeviceDescriptor, EndpointDescriptor,
    InterfaceDescriptor,
};
use embassy_usb_host::handler::EnumerationInfo;
use embassy_usb_host::{BusRoute, BusState, EnumerationError};
use rp_pio_usb_host::{Bus, PioPipe, PioUsbAllocator, PioUsbController, Pulldown};
use static_cell::StaticCell;

use crate::device::{PROFILE, Role};
use crate::hid::{HidInterface, MAX_HID_INTERFACES};
use crate::{auth, hid, hub, proxy, relay};

type HostController =
    embassy_usb_host::BusController<'static, PioUsbController<'static, 'static, PIO0>>;
pub type Allocator = PioUsbAllocator<'static, 'static, PIO0>;
pub type HostBus = embassy_usb_host::BusHandle<'static, Allocator>;
pub type ControlPipe = PioPipe<'static, 'static, pipe::Control, pipe::InOut, PIO0>;

/// Room for the full configuration descriptor. Typical HID devices need < 100 bytes.
const CONFIG_BUF_LEN: usize = 512;

/// Max bytes of a string descriptor (bLength is a u8).
const STRING_BUF_LEN: usize = 255;

/// Fallback LANGID when the device has no usable LANGID table (English (US)).
const LANGID_EN_US: u16 = 0x0409;

const DESC_TYPE_STRING: u8 = 0x03;
const DESC_TYPE_HID: u8 = 0x21;
const CLASS_HUB: u8 = 0x09;

/// Bus served by the frame-timer interrupt (set once by [`init`]).
static FRAME_TIMER_BUS: AtomicPtr<Bus<'static, PIO0>> = AtomicPtr::new(ptr::null_mut());

/// Claim PIO0 + GPIO12/13 for the USB host bus and start its frame timer.
///
/// Call on the core that runs the host: the frame-timer interrupt (`TIMER0_IRQ_1`) is
/// enabled there, and the binary must route it to [`on_frame_timer_irq`].
pub fn init(
    pio: Peri<'static, PIO0>,
    dp: Peri<'static, PIN_12>,
    dm: Peri<'static, PIN_13>,
    irq: impl Binding<PIO0_IRQ_0, InterruptHandler<PIO0>>,
) -> &'static Bus<'static, PIO0> {
    static BUS: StaticCell<Bus<'static, PIO0>> = StaticCell::new();
    let bus = BUS.init(Bus::new(pio, dp, dm, irq, Pulldown::Internal));
    FRAME_TIMER_BUS.store(ptr::from_ref(bus).cast_mut(), Ordering::Release);
    bus.start_frame_timer();
    bus
}

/// Body of the 1 ms frame-timer interrupt.
pub fn on_frame_timer_irq() {
    let bus = FRAME_TIMER_BUS.load(Ordering::Acquire);
    // SAFETY: set once from a `&'static Bus` before the interrupt is enabled.
    if let Some(bus) = unsafe { bus.as_ref() } {
        bus.on_frame_timer();
    }
}

/// Sends full-speed SOFs every 1 ms between transfers. Must run for the device to stay
/// out of suspend.
#[embassy_executor::task]
pub async fn idle_task(bus: &'static Bus<'static, PIO0>) {
    bus.idle_task().await;
}

/// Waits for a device on the root port, enumerates it and serves it
/// ([`serve_device`]), or the devices behind it if it is a hub. Runs until the device
/// detaches.
#[embassy_executor::task]
pub async fn host_task(bus: &'static Bus<'static, PIO0>) {
    static BUS_STATE: BusState = BusState::new();
    let (mut ctrl, bus) = embassy_usb_host::bus(bus.controller(), &BUS_STATE);
    let mut config_buf = [0u8; CONFIG_BUF_LEN];

    loop {
        log::info!("waiting for USB device...");
        let speed = ctrl.wait_for_connection().await;
        log::info!("USB device detected ({})", speed_name(speed));

        if speed != Speed::Full {
            log::warn!("only full-speed devices are targeted for now; trying anyway");
        }

        let (info, config_len) =
            enumerate_with_retry(&mut ctrl, &bus, speed, &mut config_buf).await;

        let ifaces = hid::find_interfaces(&config_buf[..config_len]);
        let serve = async {
            match classify(&info) {
                Kind::Hub => {
                    hub::run(&bus, &info).await;
                    log::warn!("hub stopped");
                }
                kind => serve_device(&bus, &info, &ifaces, kind).await,
            }
        };
        // With R13 fitted a detach is never seen, so this runs until reset.
        if let Either::First(()) = select(serve, wait_for_disconnect(&mut ctrl)).await {
            wait_for_disconnect(&mut ctrl).await;
        }
        log::info!("USB device disconnected");
        bus.free_address(info.device_address);
    }
}

/// What an enumerated device is to the bridge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The G Pro Xbox/PC (c272).
    Wheel,
    /// DriveHub (c269), in the relay role.
    RelayBackend,
    Hub,
    /// Anything else: in the wheel role, a candidate signer for the console's auth.
    Other,
}

pub fn classify(info: &EnumerationInfo) -> Kind {
    let d = &info.device_desc;
    match (d.vendor_id, d.product_id) {
        (proxy::WHEEL_VID, proxy::WHEEL_PID) => Kind::Wheel,
        (relay::BACKEND_VID, relay::BACKEND_PID) if PROFILE.role == Role::Relay => {
            Kind::RelayBackend
        }
        _ if d.device_class == CLASS_HUB => Kind::Hub,
        _ => Kind::Other,
    }
}

/// Serve one enumerated device (not a hub) until a transfer fails:
/// - the wheel: forwarded to the native-port device ([`proxy`]);
/// - DriveHub in the relay role: relayed ([`relay`]);
/// - anything else: its HID interfaces are probed; in the wheel role, if it answers
///   the auth reset report it signs the console's auth ([`auth`]); otherwise its input
///   reports are logged.
pub async fn serve_device(
    bus: &HostBus,
    info: &EnumerationInfo,
    ifaces: &[Option<HidInterface>; MAX_HID_INTERFACES],
    kind: Kind,
) {
    match kind {
        // No probe for the wheel: its descriptors are known, and it wants host software
        // to talk to it right after SET_CONFIGURATION (see `proxy`); the probe's ~0.5 s
        // of descriptor reads and log pauses made it switch off about half the time.
        Kind::Wheel => {
            proxy::run(bus, info, ifaces).await;
            log::warn!("proxy stopped");
        }
        Kind::RelayBackend => {
            hid::probe(bus, info, ifaces).await;
            relay::run(bus, info, ifaces).await;
            log::warn!("relay stopped");
        }
        Kind::Hub => log::warn!("hub not expected here"),
        Kind::Other => {
            hid::probe(bus, info, ifaces).await;
            if PROFILE.role == Role::Wheel
                && let Some(iface) = ifaces.iter().flatten().next()
                && let Ok(mut ep0) = open_ep0(bus, info)
                && auth::try_signer(&mut ep0, u16::from(iface.number)).await
            {
                auth::serve(&mut ep0, u16::from(iface.number)).await;
            }
            log::info!("monitoring HID input reports (changes only)...");
            hid::monitor(bus, info, ifaces).await;
            log::warn!("all HID monitors stopped");
        }
    }
}

/// Open the default control pipe of an enumerated device.
pub fn open_ep0(bus: &HostBus, info: &EnumerationInfo) -> Result<ControlPipe, HostError> {
    let ep0 = EndpointInfo {
        addr: EndpointAddress::from_parts(0, Direction::In),
        ep_type: EndpointType::Control,
        max_packet_size: u16::from(info.device_desc.max_packet_size0),
        interval_ms: 0,
    };
    bus.alloc_pipe::<pipe::Control, pipe::InOut>(info.device_address, &ep0, info.split())
}

/// Enumerate and log the device, retrying until it answers. Returns the enumeration info
/// and the length of the configuration descriptor written to `config_buf`.
///
/// No attempt limit: a booting device (DriveHub, the wheel after a power cycle) can take
/// several attempts, and with R13 fitted a detach is never seen, so giving up could only
/// be undone by a reset.
async fn enumerate_with_retry(
    ctrl: &mut HostController,
    bus: &HostBus,
    speed: Speed,
    config_buf: &mut [u8],
) -> (EnumerationInfo, usize) {
    let mut attempt = 1u32;
    loop {
        match enumerate_and_report(bus, speed, config_buf).await {
            Ok(r) => return r,
            Err(e) => {
                log::warn!("USB enumeration failed (attempt {}): {:?}", attempt, e);
                // Put the device back into the default (address 0) state.
                ctrl.controller_mut().bus_reset().await;
                attempt += 1;
            }
        }
    }
}

async fn enumerate_and_report(
    bus: &HostBus,
    speed: Speed,
    config_buf: &mut [u8],
) -> Result<(EnumerationInfo, usize), EnumerationError> {
    // enumerate(): GET_DESCRIPTOR(device, 8) -> SET_ADDRESS -> GET_DESCRIPTOR(device)
    // -> GET_DESCRIPTOR(config) -> SET_CONFIGURATION.
    let (info, config_len) = bus.enumerate(BusRoute::Direct(speed), config_buf).await?;
    log_device(bus, &info, &config_buf[..config_len]).await;
    Ok((info, config_len))
}

/// Log an enumerated device: device and configuration descriptors, strings.
pub async fn log_device(bus: &HostBus, info: &EnumerationInfo, config: &[u8]) {
    log_device_descriptor(info);
    log_config_descriptor(config);
    // String descriptors are informational only; failures are logged, not fatal.
    log_strings(bus, info).await;
}

async fn wait_for_disconnect(ctrl: &mut HostController) {
    loop {
        if let DeviceEvent::Disconnected = ctrl.wait_for_device_event().await {
            return;
        }
    }
}

fn log_device_descriptor(info: &EnumerationInfo) {
    let d: &DeviceDescriptor = &info.device_desc;
    log::info!("VID: {:#06x}", d.vendor_id);
    log::info!("PID: {:#06x}", d.product_id);
    log::info!("address: {}", info.device_address);
    log::info!(
        "USB {:x}.{:02x}, device release {:x}.{:02x}",
        d.bcd_usb >> 8,
        d.bcd_usb & 0xff,
        d.bcd_device >> 8,
        d.bcd_device & 0xff
    );
    log::info!(
        "device class: {:#04x} ({}), subclass {:#04x}, protocol {:#04x}",
        d.device_class,
        class_name(d.device_class),
        d.device_subclass,
        d.device_protocol
    );
    log::info!("EP0 max packet size: {}", d.max_packet_size0);
    log::info!("configuration count: {}", d.num_configurations);
}

fn log_config_descriptor(buf: &[u8]) {
    let cfg = match ConfigurationDescriptor::try_from_slice(buf) {
        Ok(cfg) => cfg,
        Err(e) => {
            log::warn!("bad configuration descriptor: {:?}", e);
            return;
        }
    };
    if let Err(e) = cfg.visit_descriptors(&mut ConfigLogger) {
        log::warn!("configuration descriptor parse error: {:?}", e);
    }
}

/// Logs the interface/endpoint tree of the active configuration.
struct ConfigLogger;

impl<'a> DescriptorVisitor<'a> for ConfigLogger {
    type Error = core::convert::Infallible;

    fn on_configuration(&mut self, c: &ConfigurationDescriptor<'a>) -> bool {
        log::info!(
            "configuration {}: {} interface(s), {} bytes, attributes {:#04x}, max power {} mA",
            c.configuration_value,
            c.num_interfaces,
            c.total_len,
            c.attributes,
            u16::from(c.max_power) * 2
        );
        true
    }

    fn on_interface(&mut self, i: &InterfaceDescriptor<'a>) -> bool {
        log::info!(
            "  interface {} alt {}: class {:#04x} ({}), subclass {:#04x}, protocol {:#04x}, {} endpoint(s)",
            i.interface_number,
            i.alternate_setting,
            i.interface_class,
            class_name(i.interface_class),
            i.interface_subclass,
            i.interface_protocol,
            i.num_endpoints
        );
        true
    }

    fn on_endpoint(&mut self, _i: &InterfaceDescriptor<'a>, e: &EndpointDescriptor) -> bool {
        log::info!(
            "    endpoint {:#04x} {} {:?}, max packet {}, interval {}",
            e.endpoint_address,
            if e.is_in() { "IN" } else { "OUT" },
            e.ep_type(),
            e.max_packet_size,
            e.interval
        );
        true
    }

    fn on_other(
        &mut self,
        _i: Option<&InterfaceDescriptor<'a>>,
        raw: &[u8],
    ) -> Result<bool, Self::Error> {
        if raw[1] == DESC_TYPE_HID && raw.len() >= 9 {
            // HID 1.11 §6.2.1: bcdHID, bCountryCode, bNumDescriptors, then (type, length) pairs.
            log::info!(
                "    HID {:x}.{:02x}, report descriptor {} bytes",
                raw[3],
                raw[2],
                u16::from_le_bytes([raw[7], raw[8]])
            );
        } else {
            log::info!("    descriptor type {:#04x}, {} bytes", raw[1], raw[0]);
        }
        Ok(true)
    }
}

async fn log_strings(bus: &HostBus, info: &EnumerationInfo) {
    let d = &info.device_desc;
    if d.manufacturer == 0 && d.product == 0 && d.serial_number == 0 {
        log::info!("no string descriptors");
        return;
    }

    let mut ep0 = match open_ep0(bus, info) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("cannot open EP0 for string descriptors: {:?}", e);
            return;
        }
    };

    let mut buf = [0u8; STRING_BUF_LEN];

    // String index 0 holds the LANGID table.
    let langid = match get_string(&mut ep0, 0, 0, &mut buf).await {
        Some(n) if n >= 4 => u16::from_le_bytes([buf[2], buf[3]]),
        _ => LANGID_EN_US,
    };

    for (label, index) in [
        ("Manufacturer", d.manufacturer),
        ("Product", d.product),
        ("Serial", d.serial_number),
    ] {
        if index == 0 {
            continue;
        }
        match get_string(&mut ep0, index, langid, &mut buf).await {
            Some(n) => log::info!("{}: {}", label, Utf16Le(&buf[2..n])),
            None => log::warn!("{}: string descriptor {} unreadable", label, index),
        }
    }
}

/// GET_DESCRIPTOR(STRING). Returns the validated descriptor length (header included).
async fn get_string(
    ep0: &mut ControlPipe,
    index: u8,
    langid: u16,
    buf: &mut [u8],
) -> Option<usize> {
    // ControlPipeExt::request_descriptor_bytes always sends wIndex=0, but string
    // descriptors take the LANGID in wIndex (USB 2.0 §9.4.3), so build the SETUP here.
    let mut setup = SetupPacket::get_descriptor(false, DESC_TYPE_STRING, index, buf.len() as u16);
    setup.index = langid;

    let n = match ep0.control_in(&setup.to_bytes(), buf).await {
        Ok(n) => n,
        Err(e) => {
            log::debug!("string descriptor {} failed: {:?}", index, e);
            return None;
        }
    };
    if n < 2 || buf[1] != DESC_TYPE_STRING {
        return None;
    }
    // Trust the smaller of bLength and the bytes received, and keep it even.
    Some(usize::from(buf[0]).min(n) & !1)
}

/// Displays a UTF-16LE string descriptor payload.
struct Utf16Le<'a>(&'a [u8]);

impl fmt::Display for Utf16Le<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let units = self
            .0
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c));
        for c in char::decode_utf16(units) {
            fmt::Write::write_char(f, c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        Ok(())
    }
}

pub fn speed_name(speed: Speed) -> &'static str {
    match speed {
        Speed::Low => "low-speed",
        Speed::Full => "full-speed",
        Speed::High => "high-speed",
    }
}

fn class_name(class: u8) -> &'static str {
    match class {
        0x00 => "per-interface",
        0x01 => "audio",
        0x02 => "CDC",
        0x03 => "HID",
        0x08 => "mass storage",
        0x09 => "hub",
        0x0a => "CDC data",
        0xe0 => "wireless",
        0xef => "miscellaneous",
        0xfe => "application specific",
        0xff => "vendor specific",
        _ => "other",
    }
}
