//! Host side of the bridge: the G Pro Xbox/PC (c272) on the USB-A port, forwarded to
//! the native-port device ([`crate::device`]); "upstream" below is whatever drives that
//! device (the PS5, or in mirror mode the wheel's host, e.g. DriveHub).
//!
//! - IF0 input (EP 0x81, 30 bytes): translated for the PS5 ([`crate::input_map`]), the
//!   only interface whose format differs; forwarded unchanged in mirror mode.
//! - IF1 HID++ (EP 0x82 IN, SET_REPORT on EP0) and IF2 force feedback (EP 0x83 IN,
//!   EP 0x03 OUT) carry the same reports on both sides and are forwarded unchanged.
//!   HID++ messages are logged in both directions, force feedback sampled.
//! - Upstream class control requests (HID++ SET_REPORT; in mirror mode all of them) are
//!   repeated to the wheel.
//! - The wheel is checked once a second with GET_STATUS: with R13 fitted a detach is
//!   never seen, so this is the only way to notice it switching off.
//!
//! The wheel switches itself off about 2 s after enumeration unless host software talks
//! to it early (G HUB and DriveHub do; the PS5 never uses IF1). DriveHub, seen through
//! the mirror, sends SET_IDLE(0) to the three interfaces and its first HID++ request
//! within 15 ms of SET_CONFIGURATION, and the wheel stayed on even when freshly booted;
//! the bridge used to start ~470 ms later (after a descriptor dump) and the wheel stayed
//! on only about half the time, with G HUB's requests as with the bridge's own. So the
//! proxy starts right after enumeration and, for the PS5, does what DriveHub does:
//! SET_IDLE, then HID++ ([`WakeUp`]). The wheel's answers during the wake-up are logged,
//! not forwarded.
//! Mirror mode sends nothing of its own: finding out what the wheel's host sends is the
//! point of it.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_futures::join::{join, join5};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Ticker, Timer};
use embassy_usb_driver::EndpointInfo;
use embassy_usb_driver::host::{PipeError, UsbHostAllocator, UsbPipe, pipe};
use embassy_usb_host::control::SetupPacket;
use embassy_usb_host::descriptor::EndpointDescriptor;
use embassy_usb_host::handler::EnumerationInfo;

use crate::device::{
    self, BACKEND_READY, CONTROL_OUT, ControlOut, FFB_IN, FFB_OUT, HID_SET_REPORT, HIDPP_IN,
    IF_HIDPP, INPUT_IN, PROFILE, Packet, Role, STATS,
};
use crate::hid::{Hex, HidInterface, MAX_HID_INTERFACES};
use crate::usb_host::{ControlPipe, HostBus, open_ep0};
use crate::{ghub_init, input_map};

pub const WHEEL_VID: u16 = 0x046d;
pub const WHEEL_PID: u16 = 0xc272;

const IF_INPUT: u16 = 0;
const IF_FFB: u16 = 2;

const HID_SET_IDLE: u8 = 0x0a;

/// Force feedback runs at up to 1 kHz; log at most one packet per direction this often.
const FFB_LOG_INTERVAL: Duration = Duration::from_millis(100);

const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// Pause after a failed IN transfer before polling again.
const IN_ERROR_BACKOFF: Duration = Duration::from_millis(50);

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

/// Wheel-side counters (the device side's are in [`device::STATS`]).
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
    let find = |n| ifaces.iter().flatten().find(|i| u16::from(i.number) == n);
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

    log::info!("proxy: forwarding c272 IF0/IF1/IF2 to the {}", PROFILE.name);
    BACKEND_READY.signal(());
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
            log_hidpp("wheel -> upstream", p.bytes());
            HIDPP_IN.try_send(p)
        }),
        serve_ep0(&mut ep0),
        forward_in(bus, info, &if2_in, "FFB", {
            let mut sampler = Sampler::new();
            move |p| {
                count(&WHEEL_FFB);
                sampler.log("FFB wheel -> upstream", p.bytes());
                FFB_IN.try_send(p)
            }
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
    let mut buf = [0u8; device::MAX_PACKET];
    loop {
        match pipe.request_in(&mut buf).await {
            Ok(input_map::C272_LEN) if PROFILE.role == Role::Mirror => {
                count(&WHEEL_INPUT);
                if INPUT_IN
                    .try_send(Packet::new(&buf[..input_map::C272_LEN]))
                    .is_err()
                {
                    device::count(&STATS.dropped);
                }
            }
            Ok(input_map::C272_LEN) => {
                count(&WHEEL_INPUT);
                let report = buf[..input_map::C272_LEN].try_into().unwrap();
                device::set_input(input_map::translate(report));
            }
            Ok(n) => log::warn!("proxy: IF0 report of {} bytes ignored", n),
            Err(e) => {
                if !keep_polling("proxy: IF0", e).await {
                    return;
                }
            }
        }
    }
}

/// After a failed IN transfer: stop on a detach; otherwise (a STALL or garbled reply,
/// seen now and then) log it and go on polling shortly.
async fn keep_polling(name: &str, e: PipeError) -> bool {
    if matches!(e, PipeError::Disconnected | PipeError::Canceled) {
        log::warn!("{} IN failed: {:?}; stopped", name, e);
        return false;
    }
    log::warn!("{} IN failed: {:?}; polling on", name, e);
    Timer::after(IN_ERROR_BACKOFF).await;
    true
}

/// Backend IN endpoint → queue to the native-port device.
pub(crate) async fn forward_in<E>(
    bus: &HostBus,
    info: &EnumerationInfo,
    ep: &EndpointDescriptor,
    name: &str,
    mut push: impl FnMut(Packet) -> Result<(), E>,
) {
    let Some(mut pipe) = open::<pipe::In>(bus, info, ep) else {
        return;
    };
    let mut buf = [0u8; device::MAX_PACKET];
    loop {
        match pipe.request_in(&mut buf).await {
            Ok(n) => {
                if push(Packet::new(&buf[..n])).is_err() {
                    device::count(&STATS.dropped);
                }
            }
            Err(e) => {
                if !keep_polling(name, e).await {
                    return;
                }
            }
        }
    }
}

/// The wheel's EP0: upstream class requests (repeated as the same request), and the
/// liveness check.
async fn serve_ep0(ep0: &mut ControlPipe) {
    if PROFILE.role != Role::Mirror {
        set_idle_all(ep0).await;
        WAKING.store(true, Ordering::Relaxed);
        match WAKE_UP {
            WakeUp::RapidPing => ping_until_answered(ep0).await,
            WakeUp::GHubReplay => replay_ghub_init(ep0).await,
        }
        // Let the last answers arrive before forwarding resumes.
        Timer::after_millis(100).await;
        WAKING.store(false, Ordering::Relaxed);
    }

    let mut ticker = Ticker::every(LIVENESS_INTERVAL);
    let mut responding = true;
    loop {
        match select(CONTROL_OUT.receive(), ticker.next()).await {
            Either::First(msg) => {
                if repeat_control(ep0, &msg, "upstream -> wheel").await {
                    count(&HIDPP_WRITTEN);
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

/// Repeat an upstream class request to the backend, logged as `direction`. Returns
/// whether it was a HID++ request that went through.
pub(crate) async fn repeat_control(
    ep0: &mut ControlPipe,
    msg: &ControlOut,
    direction: &str,
) -> bool {
    let data = msg.data.bytes();
    let hidpp = msg.request == HID_SET_REPORT && msg.index == IF_HIDPP;
    if hidpp && msg.value == HIDPP_SHORT_OUTPUT {
        log_hidpp(direction, data);
    } else {
        log::debug!(
            "control {}: request {:#04x} value {:#06x} IF{} [{}]: {}",
            direction,
            msg.request,
            msg.value,
            msg.index,
            data.len(),
            Hex(data)
        );
    }
    let setup =
        SetupPacket::class_interface_out(msg.request, msg.value, msg.index, data.len() as u16);
    match ep0.control_out(&setup.to_bytes(), data).await {
        Ok(()) => hidpp,
        Err(e) => {
            log::warn!(
                "request {:#04x} IF{} ({}) failed: {:?}",
                msg.request,
                msg.index,
                direction,
                e
            );
            false
        }
    }
}

/// Force feedback from upstream → wheel EP 0x03.
///
/// The PS5 sends report 0x01 cut short (7-12 bytes, relay capture); DriveHub pads it
/// with zeros to the declared 64 bytes before giving it to the wheel, and so does this.
async fn forward_ffb_out(bus: &HostBus, info: &EnumerationInfo, ep: &EndpointDescriptor) {
    let Some(mut pipe) = open::<pipe::Out>(bus, info, ep) else {
        return;
    };
    let mut sampler = Sampler::new();
    let mut report = [0u8; device::MAX_PACKET];
    loop {
        let packet = FFB_OUT.receive().await;
        let data = packet.bytes();
        sampler.log("FFB upstream -> wheel", data);
        report.fill(0);
        report[..data.len()].copy_from_slice(data);
        match pipe.request_out(&report, false).await {
            Ok(()) => count(&FFB_WRITTEN),
            Err(e) => {
                log::warn!("proxy: FFB OUT failed: {:?}; stopped", e);
                return;
            }
        }
    }
}

pub(crate) fn open<D: pipe::Direction>(
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
            log::warn!("cannot open endpoint {:#04x}: {:?}", ep.endpoint_address, e);
            None
        }
    }
}

/// SET_IDLE(0) on the wheel's three interfaces, as DriveHub starts. HID 1.11 §7.2.4
/// makes the request optional, so a STALL is fine.
async fn set_idle_all(ep0: &mut ControlPipe) {
    for iface in [IF_INPUT, IF_HIDPP, IF_FFB] {
        let setup = SetupPacket::class_interface_out(HID_SET_IDLE, 0, iface, 0);
        match ep0.control_out(&setup.to_bytes(), &[]).await {
            Ok(()) | Err(PipeError::Stall) => {}
            Err(e) => log::warn!("proxy: SET_IDLE IF{} failed: {:?}", iface, e),
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
        IF_HIDPP,
        PING.len() as u16,
    )
    .to_bytes()
}

/// One HID++ message, trailing zero padding left out (the length is the full one).
pub(crate) fn log_hidpp(direction: &str, msg: &[u8]) {
    let used = msg.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    log::debug!("HID++ {} [{}]: {}", direction, msg.len(), Hex(&msg[..used]));
}

/// Logs a high-rate stream at most once per [`FFB_LOG_INTERVAL`], with the number of
/// packets since the previous logged one.
pub(crate) struct Sampler {
    last: Option<Instant>,
    skipped: u32,
}

impl Sampler {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            skipped: 0,
        }
    }

    pub(crate) fn log(&mut self, what: &str, packet: &[u8]) {
        let now = Instant::now();
        if self.last.is_some_and(|t| now - t < FFB_LOG_INTERVAL) {
            self.skipped += 1;
            return;
        }
        let used = packet.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        log::debug!(
            "{} [{}] (+{} since last): {}",
            what,
            packet.len(),
            self.skipped,
            Hex(&packet[..used])
        );
        self.last = Some(now);
        self.skipped = 0;
    }
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
