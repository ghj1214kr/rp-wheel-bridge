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

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use embassy_futures::join::{join, join5};
use embassy_futures::select::{Either4, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer, with_timeout};
use embassy_usb_driver::EndpointInfo;
use embassy_usb_driver::host::{PipeError, UsbHostAllocator, UsbPipe, pipe};
use embassy_usb_host::control::SetupPacket;
use embassy_usb_host::descriptor::EndpointDescriptor;
use embassy_usb_host::handler::EnumerationInfo;

use crate::device::{
    self, BACKEND_READY, CONTROL_OUT, ControlOut, FFB_IN, FFB_OUT, HID_SET_REPORT, HIDPP_IN,
    IF_HIDPP, IF0_OUT, INPUT_IN, PROFILE, Packet, Role, STATS,
};
use crate::hid::{GapMeter, Hex, HidInterface, MAX_HID_INTERFACES};
use crate::usb_host::{ControlPipe, HostBus, open_ep0};
use crate::{ghub_init, input_map};

pub const WHEEL_VID: u16 = 0x046d;
pub const WHEEL_PID: u16 = 0xc272;

const IF_INPUT: u16 = 0;
const IF_FFB: u16 = 2;

const HID_SET_IDLE: u8 = 0x0a;

/// Force feedback runs at up to 1 kHz; log at most one packet per direction this often.
const FFB_LOG_INTERVAL: Duration = Duration::from_secs(2);

const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// Force feedback recovery. If the wheel stops taking force feedback, it STALLs its FFB
/// OUT endpoint and notifies `12 ff 1f 00 20`, and the console, which sets force
/// feedback up only once, loses it for good. The one cause seen so far was the host
/// holding the bus after its packets (fixed in rp-pio-usb-host; docs/usb-host.md).
/// The FFB OUT task then asks [`serve_ep0`] to clear the endpoint halt
/// ([`FFB_RECOVER`], answered through [`FFB_HALT_CLEARED`]) and replays the console's
/// FFB set-up commands to the wheel.
static FFB_RECOVER: Signal<CriticalSectionRawMutex, u8> = Signal::new();
static FFB_HALT_CLEARED: Signal<CriticalSectionRawMutex, bool> = Signal::new();
/// Set while the set-up is replayed: the wheel's answers to it are not forwarded.
static FFB_REPLAYING: AtomicBool = AtomicBool::new(false);
/// At most one recovery per this long.
const FFB_RECOVERY_INTERVAL: Duration = Duration::from_secs(2);
/// Console FFB set-up commands kept for a replay (GT7 sends ~60).
const FFB_SETUP_MAX: usize = 96;
/// FFB report command byte (`01 00 00 00 <cmd> <seq> ...`): the force stream is 0x01 to
/// the wheel and 0x02 back; 0x05 with sequence 1 starts the console's set-up.
const FFB_CMD: usize = 4;
const FFB_CMD_FORCE: u8 = 0x01;
const FFB_CMD_STATUS: u8 = 0x02;
const FFB_CMD_SETTING: u8 = 0x05;

/// An FFB packet taking this long to reach the wheel is logged.
const SLOW_FFB_OUT: Duration = Duration::from_millis(20);

/// Pause after a failed IN transfer before polling again.
const IN_ERROR_BACKOFF: Duration = Duration::from_millis(50);

const LIVENESS_INTERVAL: Duration = Duration::from_secs(1);

/// Standard GET_STATUS (device), 2 bytes.
const GET_STATUS: [u8; 8] = [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00];

/// SET_REPORT wValue for a HID++ short report: Output (0x02) << 8 | report ID 0x10.
const HIDPP_SHORT_OUTPUT: u16 = 0x0210;
/// The same for a long report (ID 0x11, 20 bytes).
const HIDPP_LONG_OUTPUT: u16 = 0x0211;
const HIDPP_LONG_LEN: usize = 20;

/// Software ID of the bridge's own HID++ requests. The wheel's answers carrying it are
/// the bridge's and are not forwarded.
const BRIDGE_SWID: u8 = 0x0b;

/// Wheel feature index of 0x807a (LED effects; 0: unknown or absent).
static LED_FEATURE: AtomicU8 = AtomicU8::new(0);
/// The wheel's latest answer carrying [`BRIDGE_SWID`] (first 20 bytes).
static BRIDGE_ANSWER: Signal<CriticalSectionRawMutex, [u8; 20]> = Signal::new();
const FEATURE_QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// PS5 IF0 output report 0x30, G29 command `f8 12 <mask>`: rev lights, one bit per
/// LED (5 LEDs).
const REV_LIGHTS: [u8; 2] = [0xf8, 0x12];

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
            let b = p.bytes();
            // Byte 3 is function << 4 | software ID; notifications have ID 0. An error
            // answer (`.. ff <index> <function|ID> <error>`) has it in byte 4.
            let at = if b.get(2) == Some(&0xff) { 4 } else { 3 };
            let swid = b.get(at).map_or(0, |x| x & 0x0f);
            let waking = WAKING.load(Ordering::Relaxed);
            if waking || swid == BRIDGE_SWID {
                log_hidpp("wheel -> bridge", b);
                if waking && swid != 0 {
                    WHEEL_ANSWERED.store(true, Ordering::Relaxed);
                }
                if swid == BRIDGE_SWID {
                    let mut answer = [0u8; 20];
                    let n = b.len().min(answer.len());
                    answer[..n].copy_from_slice(&b[..n]);
                    BRIDGE_ANSWER.signal(answer);
                }
                return Ok(());
            }
            log_hidpp("wheel -> upstream", p.bytes());
            HIDPP_IN.try_send(p)
        }),
        serve_ep0(&mut ep0),
        forward_in(bus, info, &if2_in, "FFB", {
            let mut sampler = Sampler::new();
            let mut gap = GapMeter::new("FFB wheel -> bridge", Duration::from_millis(100));
            move |p| {
                gap.tick();
                count(&WHEEL_FFB);
                // Answers to a replayed set-up are the bridge's, not the console's.
                if FFB_REPLAYING.load(Ordering::Relaxed)
                    && p.bytes().get(FFB_CMD).is_some_and(|&c| c != FFB_CMD_STATUS)
                {
                    return Ok(());
                }
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

    if PROFILE.role != Role::Mirror {
        log_feature_table(ep0).await;
        query_led_feature(ep0).await;
    }

    let mut ticker = Ticker::every(LIVENESS_INTERVAL);
    let mut responding = true;
    let mut rev_level = None;
    loop {
        match select4(
            CONTROL_OUT.receive(),
            IF0_OUT.receive(),
            FFB_RECOVER.wait(),
            ticker.next(),
        )
        .await
        {
            Either4::First(msg) => {
                if repeat_control(ep0, &msg, "upstream -> wheel").await {
                    count(&HIDPP_WRITTEN);
                }
            }
            Either4::Second(report) => rev_lights(ep0, report.bytes(), &mut rev_level).await,
            Either4::Third(ep) => {
                // CLEAR_FEATURE(ENDPOINT_HALT) to endpoint `ep`.
                let setup = [0x02, 0x01, 0x00, 0x00, ep, 0x00, 0x00, 0x00];
                let result = ep0.control_out(&setup, &[]).await;
                log::info!("proxy: clear halt on EP {:#04x}: {:?}", ep, result);
                FFB_HALT_CLEARED.signal(result.is_ok());
            }
            Either4::Fourth(()) => {
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

/// Send a HID++ short request (software ID [`BRIDGE_SWID`]) and wait for the answer.
/// `None` on a transfer error, a timeout or an HID++ error answer.
async fn hidpp_request(ep0: &mut ControlPipe, msg: &[u8; 7]) -> Option<[u8; 20]> {
    BRIDGE_ANSWER.reset();
    log_hidpp("bridge -> wheel", msg);
    if let Err(e) = ep0.control_out(&hidpp_short_setup(), msg).await {
        log::warn!("proxy: HID++ request failed: {:?}", e);
        return None;
    }
    let answer = with_timeout(FEATURE_QUERY_TIMEOUT, BRIDGE_ANSWER.wait())
        .await
        .ok()?;
    (answer[2] != 0xff).then_some(answer)
}

/// IRoot (index 0) function 0 getFeature: the wheel's index of `feature` (0: absent).
async fn get_feature(ep0: &mut ControlPipe, feature: u16) -> Option<u8> {
    let [hi, lo] = feature.to_be_bytes();
    let msg = [0x10, 0xff, 0x00, BRIDGE_SWID, hi, lo, 0x00];
    hidpp_request(ep0, &msg).await.map(|a| a[4])
}

/// Ask the wheel for the index of its LED effects feature (0x807a).
async fn query_led_feature(ep0: &mut ControlPipe) {
    match get_feature(ep0, 0x807a).await {
        None => log::warn!("proxy: no answer to getFeature(0x807a); rev lights off"),
        Some(0) => log::warn!("proxy: wheel has no LED effects feature (0x807a)"),
        Some(index) => {
            LED_FEATURE.store(index, Ordering::Relaxed);
            log::info!(
                "proxy: LED effects (0x807a) at feature index {:#04x}",
                index
            );
        }
    }
}

/// HID++ feature IFeatureSet: function 0 getCount, function 1 getFeatureID(index).
const FEATURE_SET: u16 = 0x0001;
/// HID++ feature TRUEFORCE (index 0x17 on the c272; level 0-0xffff).
const TRUEFORCE: u16 = 0x8139;

/// Log the wheel's feature table (index → feature ID, debug level) and its TRUEFORCE
/// state. The wheel notifies onboard setting changes as `12 ff <index> ..`; the table
/// names the index. On the c272 function 0 of TRUEFORCE returns the onboard level
/// (`ff ff` = 100%); function 1 returned `00 00`.
async fn log_feature_table(ep0: &mut ControlPipe) {
    let Some(set @ 1..) = get_feature(ep0, FEATURE_SET).await else {
        log::warn!("proxy: wheel has no IFeatureSet");
        return;
    };
    let Some(count) = hidpp_request(ep0, &[0x10, 0xff, set, BRIDGE_SWID, 0, 0, 0]).await
    else {
        return;
    };
    for index in 1..=count[4] {
        let msg = [0x10, 0xff, set, 0x10 | BRIDGE_SWID, index, 0, 0];
        if let Some(a) = hidpp_request(ep0, &msg).await {
            log::debug!(
                "proxy: HID++ feature {:#04x} = {:04x} (type {:02x})",
                index,
                u16::from_be_bytes([a[4], a[5]]),
                a[6]
            );
        }
    }
    let Some(tf @ 1..) = get_feature(ep0, TRUEFORCE).await else {
        log::info!("proxy: wheel has no TRUEFORCE feature");
        return;
    };
    for function in 0..2u8 {
        let msg = [0x10, 0xff, tf, function << 4 | BRIDGE_SWID, 0, 0, 0];
        if let Some(a) = hidpp_request(ep0, &msg).await {
            log::info!(
                "proxy: TRUEFORCE ({:#04x}) function {}: {}",
                tf,
                function,
                Hex(&a[4..16])
            );
        }
    }
}

/// PS5 rev lights (IF0 output `30 f8 12 <mask>`, 5 LEDs as bits) → the wheel's 10 rev
/// LEDs: 0x807a function 6 `00 01 00 0a 00 <level>`, level 0-10 (DriveHub sends the
/// same command), 2 levels per PS5 LED. How a level is drawn (center-out pairs, a sweep
/// from one side, ...) is the wheel's LED profile. Sent on change.
///
/// DriveHub maps differently: it was seen sending only 04-0a, lighting up at high revs
/// only; its exact mapping is not known.
async fn rev_lights(ep0: &mut ControlPipe, report: &[u8], last: &mut Option<u8>) {
    let [0x30, a, b, mask, ..] = *report else {
        return;
    };
    let index = LED_FEATURE.load(Ordering::Relaxed);
    if [a, b] != REV_LIGHTS || index == 0 {
        return;
    }
    let lit = (mask & 0x1f).count_ones() as u8 * 2;
    if *last == Some(lit) {
        return;
    }
    *last = Some(lit);
    let mut msg = [0u8; HIDPP_LONG_LEN];
    msg[..10].copy_from_slice(&[
        0x11,
        0xff,
        index,
        0x60 | BRIDGE_SWID,
        0x00,
        0x01,
        0x00,
        0x0a,
        0x00,
        lit,
    ]);
    log_hidpp("bridge -> wheel (rev lights)", &msg);
    let setup = SetupPacket::class_interface_out(
        HID_SET_REPORT,
        HIDPP_LONG_OUTPUT,
        IF_HIDPP,
        msg.len() as u16,
    );
    if let Err(e) = ep0.control_out(&setup.to_bytes(), &msg).await {
        log::warn!("proxy: rev lights failed: {:?}", e);
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
    let mut setup = [Packet::new(&[]); FFB_SETUP_MAX];
    let mut setup_len = 0usize;
    let mut last_recovery: Option<Instant> = None;
    loop {
        let packet = FFB_OUT.receive().await;
        let data = packet.bytes();
        sampler.log("FFB upstream -> wheel", data);
        if carries_trueforce(data) {
            count(&FFB_TRUEFORCE);
        }
        if let [0x01, _, _, _, cmd, seq, ..] = *data {
            if cmd == FFB_CMD_SETTING && seq == 0x01 {
                setup_len = 0;
            }
            if cmd != FFB_CMD_FORCE && setup_len < FFB_SETUP_MAX {
                setup[setup_len] = packet;
                setup_len += 1;
            }
        }
        report.fill(0);
        report[..data.len()].copy_from_slice(data);
        let sent = Instant::now();
        let result = pipe.request_out(&report, false).await;
        let took = sent.elapsed();
        if took >= SLOW_FFB_OUT {
            log::info!(
                "FFB bridge -> wheel: one packet took {} ms",
                took.as_millis()
            );
        }
        match result {
            Ok(()) => count(&FFB_WRITTEN),
            // The packet is dropped; the next one goes out as usual.
            Err(e @ (PipeError::Disconnected | PipeError::Canceled)) => {
                log::warn!("proxy: FFB OUT failed: {:?}; stopped", e);
                return;
            }
            Err(e) => {
                log::warn!("proxy: FFB OUT failed: {:?}; packet dropped", e);
                let due = last_recovery.is_none_or(|t| t.elapsed() >= FFB_RECOVERY_INTERVAL);
                if e == PipeError::Stall && due && setup_len > 0 {
                    last_recovery = Some(Instant::now());
                    recover_ffb(&mut pipe, ep.endpoint_address, &setup[..setup_len]).await;
                }
            }
        }
    }
}

/// Force stream packets from the console carrying TRUEFORCE samples, per stats interval.
static FFB_TRUEFORCE: AtomicU32 = AtomicU32::new(0);
/// Byte 10 of a force packet: the number of new TRUEFORCE samples (mescon's
/// TRUEFORCE_PROTOCOL.md). GT7 sends samples only with vibration on for controller 1.
const FFB_TF_SAMPLES: usize = 10;

fn carries_trueforce(data: &[u8]) -> bool {
    data.get(FFB_CMD) == Some(&FFB_CMD_FORCE)
        && (data.len() > 12 || data.get(FFB_TF_SAMPLES).is_some_and(|&n| n != 0))
}

/// The wheel STALLed force feedback: clear the halt and replay the console's set-up.
async fn recover_ffb(
    pipe: &mut <HostBus as UsbHostAllocator<'static>>::Pipe<pipe::Interrupt, pipe::Out>,
    ep: u8,
    setup: &[Packet],
) {
    log::warn!(
        "proxy: wheel stopped taking force feedback; recovering ({} set-up commands)",
        setup.len()
    );
    FFB_HALT_CLEARED.reset();
    FFB_RECOVER.signal(ep);
    match with_timeout(Duration::from_secs(1), FFB_HALT_CLEARED.wait()).await {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            log::warn!("proxy: FFB recovery: halt not cleared");
            return;
        }
    }
    pipe.reset_data_toggle();
    FFB_REPLAYING.store(true, Ordering::Relaxed);
    let mut report = [0u8; device::MAX_PACKET];
    let mut failed = 0u32;
    for packet in setup {
        let data = packet.bytes();
        report.fill(0);
        report[..data.len()].copy_from_slice(data);
        if pipe.request_out(&report, false).await.is_err() {
            failed += 1;
        }
        Timer::after_millis(3).await;
    }
    // Let the wheel's answers to the replay arrive before forwarding them again.
    Timer::after_millis(100).await;
    FFB_REPLAYING.store(false, Ordering::Relaxed);
    log::info!(
        "proxy: FFB set-up replayed ({} commands, {} failed)",
        setup.len(),
        failed
    );
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

    /// Force feedback reports `01 00 00 00 <cmd> ...`: the force stream (cmd 01 to the
    /// wheel, 02 back) is sampled, anything else (mode and setting commands, e.g.
    /// `05 01` switching FFB on) is always logged.
    pub(crate) fn log(&mut self, what: &str, packet: &[u8]) {
        let now = Instant::now();
        let stream = packet.first() != Some(&0x01) || matches!(packet.get(4), Some(0x01 | 0x02));
        if stream && self.last.is_some_and(|t| now - t < FFB_LOG_INTERVAL) {
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
            "proxy {}s: input {} -> {} | HID++ in {} -> {}, out {} -> {} | FFB in {} -> {}, out {} -> {} (TF {}) | dropped {}",
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
            take(&FFB_TRUEFORCE),
            take(&STATS.dropped),
        );
        // Host bus anomalies (rp-pio-usb-host). Handshakes missed after an OUT/SETUP
        // ("none", tens per second, retried) alone are not logged.
        let d = rp_pio_usb_host::diag::take();
        let rare = rp_pio_usb_host::diag::Counters {
            hs_no_reply: 0,
            ..d
        };
        if !rare.is_clean() {
            log::info!(
                "PIO USB: EOP timeout {}, bus held after TX {}, handshake after OUT/SETUP: none {}, short {}, STALL {}, other {} (last {:#04x})",
                d.tx_eop_timeout,
                d.tx_bus_held,
                d.hs_no_reply,
                d.hs_short,
                d.hs_stall,
                d.hs_other,
                d.hs_other_last_pid,
            );
        }
    }
}
