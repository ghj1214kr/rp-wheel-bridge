//! Host side of the bridge: the G Pro Xbox/PC (c272) on the USB-A port, forwarded to
//! the PS device ([`crate::ps_device`]) on the native port.
//!
//! - IF0 input (EP 0x81, 30 bytes) is translated ([`crate::input_map`]); the only
//!   interface whose format differs.
//! - IF1 HID++ (EP 0x82 IN, SET_REPORT on EP0) and IF2 force feedback (EP 0x83 IN,
//!   EP 0x03 OUT) carry the same reports on both sides and are forwarded unchanged.
//!   HID++ messages are logged in both directions.
//! - The wheel is checked once a second with GET_STATUS: with R13 fitted a detach is
//!   never seen, so this is the only way to notice it switching off.
//!
//! A freshly booted wheel switches itself off about 2 s after enumeration unless host
//! software talks HID++ to it (G HUB does; the PS5 never uses IF1). What seems to count
//! is a request still pending when the wheel finishes booting (~0.3 s after the proxy
//! starts): a single ping or pings 400 ms apart missed that moment and the wheel went
//! off, while G HUB's burst of four requests (replayed from [`crate::ghub_init`]) kept it
//! on. See [`WakeUp`]. The wheel's answers during the wake-up are logged, not forwarded.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_futures::join::{join, join5};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Ticker, Timer};
use embassy_usb_driver::EndpointInfo;
use embassy_usb_driver::host::{UsbHostAllocator, UsbPipe, pipe};
use embassy_usb_host::control::SetupPacket;
use embassy_usb_host::descriptor::EndpointDescriptor;
use embassy_usb_host::handler::EnumerationInfo;

use crate::hid::{Hex, HidInterface, MAX_HID_INTERFACES};
use crate::ps_device::{self, FFB_IN, FFB_OUT, HIDPP_IN, HIDPP_OUT, Packet, STATS};
use crate::usb_host::{ControlPipe, HostBus, open_ep0};
use crate::{ghub_init, input_map};

pub const WHEEL_VID: u16 = 0x046d;
pub const WHEEL_PID: u16 = 0xc272;

const IF_INPUT: u8 = 0;
const IF_HIDPP: u8 = 1;
const IF_FFB: u8 = 2;

const HID_SET_REPORT: u8 = 0x09;

const STATS_INTERVAL: Duration = Duration::from_secs(5);

const LIVENESS_INTERVAL: Duration = Duration::from_secs(1);

/// Standard GET_STATUS (device), 2 bytes.
const GET_STATUS: [u8; 8] = [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00];

/// SET_REPORT wValue for a HID++ short report: Output (0x02) << 8 | report ID 0x10.
const HIDPP_SHORT_OUTPUT: u16 = 0x0210;

/// How the bridge keeps a freshly booted wheel on.
#[allow(dead_code)] // one variant is selected at a time while this is being narrowed down
enum WakeUp {
    /// Ping every [`PING_INTERVAL`] until the wheel answers.
    RapidPing,
    /// Replay G HUB's 276 start-up requests with their captured spacing.
    GHubReplay,
}

const WAKE_UP: WakeUp = WakeUp::RapidPing;

/// IRoot (feature index 0) function 1, getProtocolVersion, software ID 0xB, ping data
/// 0x5a. G HUB starts with the same request (software IDs 0xF/0xC, data 0).
const PING: [u8; 7] = [0x10, 0xff, 0x00, 0x1b, 0x00, 0x00, 0x5a];

/// Short enough that a request is always pending when the wheel finishes booting.
const PING_INTERVAL: Duration = Duration::from_millis(20);

/// Give up pinging after this long.
const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// Set while the bridge wakes the wheel: wheel HID++ messages are then answers to the
/// bridge and are not forwarded.
static WAKING: AtomicBool = AtomicBool::new(false);

/// Set when the wheel answers a request (not a notification) during the wake-up.
static WHEEL_ANSWERED: AtomicBool = AtomicBool::new(false);

/// Wheel-side counters (the device side's are in [`ps_device::STATS`]).
static WHEEL_INPUT: AtomicU32 = AtomicU32::new(0);
static WHEEL_HIDPP: AtomicU32 = AtomicU32::new(0);
static WHEEL_FFB: AtomicU32 = AtomicU32::new(0);
static FFB_WRITTEN: AtomicU32 = AtomicU32::new(0);
static HIDPP_WRITTEN: AtomicU32 = AtomicU32::new(0);

/// Forward the wheel until a transfer fails. Returns immediately if the device does not
/// have the c272's interface layout.
pub async fn run(
    bus: &HostBus,
    info: &EnumerationInfo,
    ifaces: &[Option<HidInterface>; MAX_HID_INTERFACES],
) {
    let find = |n| ifaces.iter().flatten().find(|i| i.number == n);
    let (Some(if0_in), Some(if1_in), Some((if2_in, if2_out))) = (
        find(IF_INPUT).and_then(|i| i.in_ep),
        find(IF_HIDPP).and_then(|i| i.in_ep),
        find(IF_FFB).and_then(|i| Some((i.in_ep?, i.out_ep?))),
    ) else {
        log::warn!("proxy: unexpected interface layout; not forwarding");
        return;
    };
    let mut ep0 = match open_ep0(bus, info) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("proxy: cannot open EP0: {:?}", e);
            return;
        }
    };

    log::info!("proxy: forwarding c272 IF0/IF1/IF2 to the PS device");
    join5(
        forward_input(bus, info, &if0_in),
        forward_in(bus, info, &if1_in, "HID++", |p| {
            count(&WHEEL_HIDPP);
            if WAKING.load(Ordering::Relaxed) {
                log_hidpp("wheel -> bridge", p.bytes());
                // Byte 3 is function << 4 | software ID; notifications have ID 0.
                if p.bytes().get(3).is_some_and(|b| b & 0x0f != 0) {
                    WHEEL_ANSWERED.store(true, Ordering::Relaxed);
                }
                return Ok(());
            }
            log_hidpp("wheel -> console", p.bytes());
            HIDPP_IN.try_send(p)
        }),
        serve_ep0(&mut ep0),
        forward_in(bus, info, &if2_in, "FFB", |p| {
            count(&WHEEL_FFB);
            FFB_IN.try_send(p)
        }),
        join(forward_ffb_out(bus, info, &if2_out), log_stats()),
    )
    .await;
}

/// Wheel IF0 → translated report for the PS device.
async fn forward_input(bus: &HostBus, info: &EnumerationInfo, ep: &EndpointDescriptor) {
    let Some(mut pipe) = open::<pipe::In>(bus, info, ep) else {
        return;
    };
    let mut buf = [0u8; ps_device::MAX_PACKET];
    loop {
        match pipe.request_in(&mut buf).await {
            Ok(input_map::C272_LEN) => {
                count(&WHEEL_INPUT);
                let report = buf[..input_map::C272_LEN].try_into().unwrap();
                ps_device::set_input(input_map::translate(report));
            }
            Ok(n) => log::warn!("proxy: IF0 report of {} bytes ignored", n),
            Err(e) => {
                log::warn!("proxy: IF0 IN failed: {:?}; input stopped", e);
                return;
            }
        }
    }
}

/// Wheel IN endpoint → queue to the PS device.
async fn forward_in<E>(
    bus: &HostBus,
    info: &EnumerationInfo,
    ep: &EndpointDescriptor,
    name: &str,
    mut push: impl FnMut(Packet) -> Result<(), E>,
) {
    let Some(mut pipe) = open::<pipe::In>(bus, info, ep) else {
        return;
    };
    let mut buf = [0u8; ps_device::MAX_PACKET];
    loop {
        match pipe.request_in(&mut buf).await {
            Ok(n) => {
                if push(Packet::new(&buf[..n])).is_err() {
                    ps_device::count(&STATS.dropped);
                }
            }
            Err(e) => {
                log::warn!("proxy: {} IN failed: {:?}; stopped", name, e);
                return;
            }
        }
    }
}

/// The wheel's EP0: HID++ SET_REPORTs from the console (forwarded as the same request
/// to IF1), and the liveness check.
async fn serve_ep0(ep0: &mut ControlPipe) {
    WAKING.store(true, Ordering::Relaxed);
    match WAKE_UP {
        WakeUp::RapidPing => ping_until_answered(ep0).await,
        WakeUp::GHubReplay => replay_ghub_init(ep0).await,
    }
    // Let the last answers arrive before forwarding resumes.
    Timer::after_millis(100).await;
    WAKING.store(false, Ordering::Relaxed);

    let mut ticker = Ticker::every(LIVENESS_INTERVAL);
    let mut responding = true;
    loop {
        match select(HIDPP_OUT.receive(), ticker.next()).await {
            Either::First(msg) => {
                let data = msg.data.bytes();
                log::debug!("HID++ console -> wheel: wValue {:#06x}", msg.value);
                log_hidpp("console -> wheel", data);
                let setup = SetupPacket::class_interface_out(
                    HID_SET_REPORT,
                    msg.value,
                    u16::from(IF_HIDPP),
                    data.len() as u16,
                );
                match ep0.control_out(&setup.to_bytes(), data).await {
                    Ok(()) => count(&HIDPP_WRITTEN),
                    Err(e) => log::warn!("proxy: HID++ SET_REPORT to wheel failed: {:?}", e),
                }
            }
            Either::Second(()) => {
                let mut status = [0u8; 2];
                let result = ep0.control_in(&GET_STATUS, &mut status).await;
                match (result, responding) {
                    (Err(e), true) => {
                        log::warn!("proxy: wheel stopped responding (GET_STATUS: {:?})", e)
                    }
                    (Ok(_), false) => log::info!("proxy: wheel responding again"),
                    _ => {}
                }
                responding = result.is_ok();
            }
        }
    }
}

/// Force feedback from the console → wheel EP 0x03.
async fn forward_ffb_out(bus: &HostBus, info: &EnumerationInfo, ep: &EndpointDescriptor) {
    let Some(mut pipe) = open::<pipe::Out>(bus, info, ep) else {
        return;
    };
    loop {
        let packet = FFB_OUT.receive().await;
        match pipe.request_out(packet.bytes(), false).await {
            Ok(()) => count(&FFB_WRITTEN),
            Err(e) => {
                log::warn!("proxy: FFB OUT failed: {:?}; stopped", e);
                return;
            }
        }
    }
}

fn open<D: pipe::Direction>(
    bus: &HostBus,
    info: &EnumerationInfo,
    ep: &EndpointDescriptor,
) -> Option<<HostBus as UsbHostAllocator<'static>>::Pipe<pipe::Interrupt, D>> {
    match bus.alloc_pipe::<pipe::Interrupt, D>(
        info.device_address,
        &EndpointInfo::from(*ep),
        info.split(),
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!(
                "proxy: cannot open endpoint {:#04x}: {:?}",
                ep.endpoint_address,
                e
            );
            None
        }
    }
}

/// Ping the wheel every [`PING_INTERVAL`] until it answers (or [`PING_TIMEOUT`]).
async fn ping_until_answered(ep0: &mut ControlPipe) {
    WHEEL_ANSWERED.store(false, Ordering::Relaxed);
    log_hidpp("bridge -> wheel", &PING);
    let setup = hidpp_short_setup();
    let start = Instant::now();
    let mut ticker = Ticker::every(PING_INTERVAL);
    let mut pings = 0u32;
    let mut failed = 0u32;
    while !WHEEL_ANSWERED.load(Ordering::Relaxed) {
        if start.elapsed() >= PING_TIMEOUT {
            log::warn!(
                "proxy: wheel did not answer {} HID++ pings ({} failed)",
                pings,
                failed
            );
            return;
        }
        pings += 1;
        if ep0.control_out(&setup, &PING).await.is_err() {
            failed += 1;
        }
        ticker.next().await;
    }
    log::info!(
        "proxy: wheel answered HID++ after {} ping(s) ({} failed), {} ms",
        pings,
        failed,
        start.elapsed().as_millis()
    );
}

/// Send G HUB's start-up requests with their captured spacing.
async fn replay_ghub_init(ep0: &mut ControlPipe) {
    let setup = hidpp_short_setup();
    let start = Instant::now();
    let mut failed = 0u32;
    for (delay_ms, msg) in &ghub_init::REQUESTS {
        Timer::after_millis(u64::from(*delay_ms)).await;
        log_hidpp("bridge -> wheel", msg);
        if ep0.control_out(&setup, msg).await.is_err() {
            failed += 1;
        }
    }
    log::info!(
        "proxy: G HUB start-up replayed ({} requests, {} failed, {} ms)",
        ghub_init::REQUESTS.len(),
        failed,
        start.elapsed().as_millis()
    );
}

/// SET_REPORT(Output, 0x10) on IF1: a HID++ short request.
fn hidpp_short_setup() -> [u8; 8] {
    SetupPacket::class_interface_out(
        HID_SET_REPORT,
        HIDPP_SHORT_OUTPUT,
        u16::from(IF_HIDPP),
        PING.len() as u16,
    )
    .to_bytes()
}

/// One HID++ message, trailing zero padding left out (the length is the full one).
fn log_hidpp(direction: &str, msg: &[u8]) {
    let used = msg.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    log::debug!("HID++ {} [{}]: {}", direction, msg.len(), Hex(&msg[..used]));
}

fn count(counter: &AtomicU32) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Per-interval message counts in both directions.
async fn log_stats() {
    let mut ticker = Ticker::every(STATS_INTERVAL);
    loop {
        ticker.next().await;
        let take = |c: &AtomicU32| c.swap(0, Ordering::Relaxed);
        log::info!(
            "proxy {}s: input {} -> {} | HID++ in {} -> {}, out {} -> {} | FFB in {} -> {}, out {} -> {} | dropped {}",
            STATS_INTERVAL.as_secs(),
            take(&WHEEL_INPUT),
            take(&STATS.input_sent),
            take(&WHEEL_HIDPP),
            take(&STATS.hidpp_sent),
            take(&STATS.hidpp_set_report),
            take(&HIDPP_WRITTEN),
            take(&WHEEL_FFB),
            take(&STATS.ffb_sent),
            take(&STATS.ffb_received),
            take(&FFB_WRITTEN),
            take(&STATS.dropped),
        );
    }
}
