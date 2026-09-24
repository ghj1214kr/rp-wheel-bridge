//! A USB hub on the USB-A port, for the wheel and the licensed auth pad together.
//!
//! Uses embassy-usb-host's [`HubHandler`] for port power and status changes. Each
//! device found on a port is reset and enumerated here ([`enumerate`], retried: the
//! wheel needs ~5 s to boot) and handed to one of [`CHILDREN`] runners, which serves it
//! like a device on the root port ([`crate::usb_host::serve_device`]) until the hub
//! reports it gone.
//!
//! The port reset is not `HubHandler::enumerate_port`'s: that sends SET_FEATURE
//! (PORT_RESET) with the default 50 ms no-data timeout, waits a fixed 50 ms and goes on.
//! The GL850G finishes that request's STATUS stage only once the reset is done (~25 ms,
//! not far from that timeout), and a fixed wait does not tell whether the port came up.
//! Here the request gets a longer timeout and the port status is polled until the port
//! is enabled.
//!
//! Limits: full-speed devices only (the PIO host has no low-speed PRE / split support),
//! and no hubs behind the hub. Behind a hub, unlike on the root port (R13), detaches
//! are seen.

use embassy_futures::join::join4;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb_driver::Speed;
use embassy_usb_driver::host::{TimeoutConfig, UsbPipe};
use embassy_usb_host::BusRoute;
use embassy_usb_host::class::hub::{HubEvent, HubHandler};
use embassy_usb_host::handler::{EnumerationInfo, HandlerEvent};

use crate::hid::{self, HidInterface, MAX_HID_INTERFACES};
use crate::usb_host::{self, ControlPipe, HostBus, Kind};

/// Ports tracked (a hub reporting more cannot be handled by `HubHandler`).
const MAX_PORTS: usize = 8;

/// Devices served at the same time: wheel, auth pad, one spare.
const CHILDREN: usize = 3;

/// Enumeration attempts per attach; each resets the port. One attempt already retries
/// for ~5.5 s inside the host stack.
const ENUM_ATTEMPTS: u32 = 4;

/// After a hub error, wait this long before listening again, and log a repeat of the
/// same error at most this often.
const ERROR_BACKOFF: Duration = Duration::from_millis(100);
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);

const CONFIG_BUF_LEN: usize = 512;

/// Hub class requests (USB 2.0 §11.24.2), recipient "other" (a port).
const SET_PORT_FEATURE: [u8; 2] = [0x23, 0x03];
const CLEAR_PORT_FEATURE: [u8; 2] = [0x23, 0x01];
const GET_PORT_STATUS: [u8; 2] = [0xa3, 0x00];
const PORT_RESET: u8 = 4;
const C_PORT_RESET: u8 = 20;
/// wPortStatus / wPortChange bits.
const PORT_ENABLE: u16 = 0x0002;
const C_RESET: u16 = 0x0010;

/// SET_FEATURE(PORT_RESET) may take until the reset is over (USB 2.0: 10-20 ms).
const PORT_RESET_REQUEST_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(1);
/// Give up on a port reset after this long.
const PORT_RESET_TIMEOUT: Duration = Duration::from_millis(500);
const PORT_POLL: Duration = Duration::from_millis(5);
/// TRSTRCY (USB 2.0 §7.1.7.5): at least 10 ms after reset before the first request.
const RESET_RECOVERY: Duration = Duration::from_millis(10);

type Ifaces = [Option<HidInterface>; MAX_HID_INTERFACES];

enum Event {
    Attached(EnumerationInfo, Ifaces, Kind),
    Detached,
}

type Slot = Signal<NoopRawMutex, Event>;

/// Serve the hub and the devices on it. Returns if the hub cannot be set up.
pub async fn run(bus: &HostBus, info: &EnumerationInfo) {
    let mut hub = match HubHandler::<_, MAX_PORTS>::try_register(bus, info).await {
        Ok(h) => h,
        Err(e) => {
            log::warn!("hub: cannot register: {:?}", e);
            return;
        }
    };
    log::info!("hub: ready; waiting for devices on its ports");

    let slots: [Slot; CHILDREN] = [Slot::new(), Slot::new(), Slot::new()];
    join4(
        events(bus, info, &mut hub, &slots),
        child(bus, 0, &slots[0]),
        child(bus, 1, &slots[1]),
        child(bus, 2, &slots[2]),
    )
    .await;
}

/// Port events: enumerate new devices and hand them to a free child; tell a child when
/// its device is gone.
async fn events(
    bus: &HostBus,
    hub_info: &EnumerationInfo,
    hub: &mut HubHandler<'static, usb_host::Allocator, MAX_PORTS>,
    slots: &[Slot],
) {
    let mut ep0 = match usb_host::open_ep0(bus, hub_info) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("hub: cannot open its EP0: {:?}", e);
            return;
        }
    };
    let mut timeout = TimeoutConfig::default();
    timeout.no_data_timeout = PORT_RESET_REQUEST_TIMEOUT;
    ep0.set_timeout(timeout);
    // Child serving each port, and which children are busy.
    let mut port_child: [Option<usize>; MAX_PORTS] = [None; MAX_PORTS];
    let mut busy = [false; CHILDREN];
    let mut config_buf = [0u8; CONFIG_BUF_LEN];
    let mut last_error_log: Option<Instant> = None;
    let mut errors_since_log = 0u32;

    loop {
        match hub.wait_for_event().await {
            Ok(HandlerEvent::HandlerEvent(HubEvent::DeviceDetected { port, speed })) => {
                log::info!(
                    "hub: device on port {} ({})",
                    port + 1,
                    usb_host::speed_name(speed)
                );
                if speed != Speed::Full {
                    log::warn!("hub: only full-speed devices are supported; trying anyway");
                }
                let Some(free) = busy.iter().position(|b| !b) else {
                    log::warn!("hub: {} devices already served; ignoring", CHILDREN);
                    continue;
                };
                let Some((info, len)) =
                    enumerate(bus, &mut ep0, &mut config_buf, port, speed).await
                else {
                    continue;
                };
                let ifaces = hid::find_interfaces(&config_buf[..len]);
                let kind = usb_host::classify(&info);
                if kind == Kind::Hub {
                    log::warn!("hub: hubs behind the hub are not supported");
                    continue;
                }
                if let Some(entry) = port_child.get_mut(usize::from(port)) {
                    *entry = Some(free);
                }
                busy[free] = true;
                slots[free].signal(Event::Attached(info, ifaces, kind));
            }
            Ok(HandlerEvent::HandlerEvent(HubEvent::DeviceRemoved { port, .. })) => {
                log::info!("hub: device on port {} removed", port + 1);
                if let Some(c) = port_child.get_mut(usize::from(port)).and_then(Option::take) {
                    busy[c] = false;
                    slots[c].signal(Event::Detached);
                }
            }
            Ok(_) => {}
            Err(e) => {
                // An unhandled status change stays set, so the same error can repeat at
                // every hub poll.
                errors_since_log += 1;
                let now = Instant::now();
                if last_error_log.is_none_or(|t| now - t >= ERROR_LOG_INTERVAL) {
                    log::warn!("hub: {:?} ({} since last logged)", e, errors_since_log);
                    last_error_log = Some(now);
                    errors_since_log = 0;
                }
                Timer::after(ERROR_BACKOFF).await;
            }
        }
    }
}

/// Reset the port and enumerate its device, a few times if needed; log it.
async fn enumerate(
    bus: &HostBus,
    hub_ep0: &mut ControlPipe,
    config_buf: &mut [u8],
    port: u8,
    speed: Speed,
) -> Option<(EnumerationInfo, usize)> {
    for attempt in 1..=ENUM_ATTEMPTS {
        if let Err(e) = reset_port(hub_ep0, port).await {
            log::warn!(
                "hub: port {} reset failed (attempt {}/{}): {}",
                port + 1,
                attempt,
                ENUM_ATTEMPTS,
                e
            );
            continue;
        }
        // Full-speed device behind a full-speed hub: addressed directly (no split).
        match bus.enumerate(BusRoute::Direct(speed), config_buf).await {
            Ok((info, len)) => {
                usb_host::log_device(bus, &info, &config_buf[..len]).await;
                return Some((info, len));
            }
            Err(e) => log::warn!(
                "hub: port {} enumeration failed (attempt {}/{}): {:?}",
                port + 1,
                attempt,
                ENUM_ATTEMPTS,
                e
            ),
        }
    }
    None
}

/// Reset one port (0-based) and wait until the hub reports it enabled.
async fn reset_port(ep0: &mut ControlPipe, port: u8) -> Result<(), &'static str> {
    let start = Instant::now();
    let p = port + 1;
    let [t, r] = SET_PORT_FEATURE;
    ep0.control_out(&[t, r, PORT_RESET, 0, p, 0, 0, 0], &[])
        .await
        .map_err(|_| "SET_FEATURE(PORT_RESET) failed")?;
    let requested = start.elapsed().as_millis();

    let (status, change) = loop {
        let [t, r] = GET_PORT_STATUS;
        let mut buf = [0u8; 4];
        ep0.control_in(&[t, r, 0, 0, p, 0, 4, 0], &mut buf)
            .await
            .map_err(|_| "GET_STATUS(port) failed")?;
        let status = u16::from_le_bytes([buf[0], buf[1]]);
        let change = u16::from_le_bytes([buf[2], buf[3]]);
        if status & PORT_ENABLE != 0 && change & C_RESET != 0 {
            break (status, change);
        }
        if start.elapsed() >= PORT_RESET_TIMEOUT {
            log::warn!(
                "hub: port {} not enabled after reset (status {:#06x} change {:#06x})",
                p,
                status,
                change
            );
            return Err("port reset timed out");
        }
        Timer::after(PORT_POLL).await;
    };

    let [t, r] = CLEAR_PORT_FEATURE;
    ep0.control_out(&[t, r, C_PORT_RESET, 0, p, 0, 0, 0], &[])
        .await
        .map_err(|_| "CLEAR_FEATURE(C_PORT_RESET) failed")?;
    log::info!(
        "hub: port {} reset: request {} ms, enabled after {} ms (status {:#06x} change {:#06x})",
        p,
        requested,
        start.elapsed().as_millis(),
        status,
        change
    );
    Timer::after(RESET_RECOVERY).await;
    Ok(())
}

/// Serve whatever device the events loop hands over, until it is removed.
async fn child(bus: &HostBus, index: usize, slot: &Slot) {
    loop {
        let Event::Attached(info, ifaces, kind) = slot.wait().await else {
            continue;
        };
        log::info!(
            "hub: child {} serves {:04x}:{:04x} (address {})",
            index,
            info.device_desc.vendor_id,
            info.device_desc.product_id,
            info.device_address
        );
        let serve = usb_host::serve_device(bus, &info, &ifaces, kind);
        let removed = async {
            loop {
                if let Event::Detached = slot.wait().await {
                    return;
                }
            }
        };
        if let Either::First(()) = select(serve, removed).await {
            // Served out (a transfer failed); keep the port until the hub says it is gone.
            while !matches!(slot.wait().await, Event::Detached) {}
        }
        log::info!(
            "hub: child {} released address {}",
            index,
            info.device_address
        );
        bus.free_address(info.device_address);
    }
}
