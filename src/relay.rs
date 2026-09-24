//! Host side of the relay role ([`crate::device::RELAY_C269`]): DriveHub (046d:c269) on
//! the USB-A port, passed through to the PS5 on the native port so that what the two
//! exchange can be recorded. Both sides have the same endpoints, so every endpoint is
//! forwarded packet by packet:
//!
//! - IF0: input (EP 0x81) to the PS5; output reports 0x05/0x30 (EP 0x01) to DriveHub.
//! - IF1 HID++: EP 0x83 (20-byte packets) to the PS5; SET_REPORTs to DriveHub.
//! - IF2 force feedback: EP 0x82 to the PS5, EP 0x02 to DriveHub.
//! - Other class requests from the PS5 are repeated to DriveHub; auth (F0-F3) goes
//!   through [`crate::auth`] with DriveHub as the signer.
//!
//! HID++ and auth are logged in full, force feedback sampled, IF0 output on change.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::join::{join3, join5};
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Ticker};
use embassy_usb_driver::host::{UsbPipe, pipe};
use embassy_usb_host::descriptor::EndpointDescriptor;
use embassy_usb_host::handler::EnumerationInfo;

use crate::auth;
use crate::device::{
    BACKEND_READY, CONTROL_OUT, FFB_IN, FFB_OUT, HIDPP_IN, IF0_OUT, INPUT_IN, Packet, STATS,
};
use crate::hid::{HidInterface, MAX_HID_INTERFACES};
use crate::proxy::{Sampler, forward_in, log_hidpp, open, repeat_control};
use crate::usb_host::{ControlPipe, HostBus, open_ep0};

pub const BACKEND_VID: u16 = 0x046d;
pub const BACKEND_PID: u16 = 0xc269;

const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// DriveHub-side counters (the PS5 side's are in [`crate::device::STATS`]).
static BACKEND_INPUT: AtomicU32 = AtomicU32::new(0);
static BACKEND_HIDPP: AtomicU32 = AtomicU32::new(0);
static BACKEND_FFB: AtomicU32 = AtomicU32::new(0);
static IF0_WRITTEN: AtomicU32 = AtomicU32::new(0);
static FFB_WRITTEN: AtomicU32 = AtomicU32::new(0);
static HIDPP_WRITTEN: AtomicU32 = AtomicU32::new(0);

type Queue = Channel<CriticalSectionRawMutex, Packet, 8>;

/// Relay DriveHub until a transfer fails. Returns immediately if it does not have the
/// c269's interface layout.
pub async fn run(
    bus: &HostBus,
    info: &EnumerationInfo,
    ifaces: &[Option<HidInterface>; MAX_HID_INTERFACES],
) {
    let find = |n: u8| ifaces.iter().flatten().find(|i| i.number == n);
    let (Some((if0_in, if0_out)), Some(if1_in), Some((if2_in, if2_out))) = (
        find(0).and_then(|i| Some((i.in_ep?, i.out_ep?))),
        find(1).and_then(|i| i.in_ep),
        find(2).and_then(|i| Some((i.in_ep?, i.out_ep?))),
    ) else {
        log::warn!("relay: unexpected interface layout; not relaying");
        return;
    };
    let mut ep0 = match open_ep0(bus, info) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("relay: cannot open EP0: {:?}", e);
            return;
        }
    };

    auth::prefetch(&mut ep0).await;
    log::info!("relay: forwarding DriveHub (c269) to the PS5");
    BACKEND_READY.signal(());

    join5(
        forward_in(bus, info, &if0_in, "relay IF0", |p| {
            count(&BACKEND_INPUT);
            INPUT_IN.try_send(p)
        }),
        forward_out(bus, info, &if0_out, &IF0_OUT, &IF0_WRITTEN, None),
        forward_in(bus, info, &if1_in, "relay HID++", |p| {
            count(&BACKEND_HIDPP);
            log_hidpp("DriveHub -> PS5", p.bytes());
            HIDPP_IN.try_send(p)
        }),
        serve_ep0(&mut ep0),
        join3(
            forward_in(bus, info, &if2_in, "relay FFB", {
                let mut sampler = Sampler::new();
                move |p| {
                    count(&BACKEND_FFB);
                    sampler.log("FFB DriveHub -> PS5", p.bytes());
                    FFB_IN.try_send(p)
                }
            }),
            forward_out(
                bus,
                info,
                &if2_out,
                &FFB_OUT,
                &FFB_WRITTEN,
                Some("FFB PS5 -> DriveHub"),
            ),
            log_stats(),
        ),
    )
    .await;
}

/// Queue → DriveHub OUT endpoint, one packet per packet. `sampled`: log label for a
/// sampled log of the stream.
async fn forward_out(
    bus: &HostBus,
    info: &EnumerationInfo,
    ep: &EndpointDescriptor,
    queue: &'static Queue,
    written: &AtomicU32,
    sampled: Option<&str>,
) {
    let Some(mut pipe) = open::<pipe::Out>(bus, info, ep) else {
        return;
    };
    let mut sampler = Sampler::new();
    loop {
        let packet = queue.receive().await;
        if let Some(label) = sampled {
            sampler.log(label, packet.bytes());
        }
        match pipe.request_out(packet.bytes(), false).await {
            Ok(()) => count(written),
            Err(e) => {
                log::warn!(
                    "relay: OUT {:#04x} failed: {:?}; stopped",
                    ep.endpoint_address,
                    e
                );
                return;
            }
        }
    }
}

/// DriveHub's EP0: the PS5's class requests, and the auth relay.
async fn serve_ep0(ep0: &mut ControlPipe) {
    loop {
        match select(CONTROL_OUT.receive(), auth::CMD.receive()).await {
            Either::First(msg) => {
                if repeat_control(ep0, &msg, "PS5 -> DriveHub").await {
                    count(&HIDPP_WRITTEN);
                }
            }
            Either::Second(cmd) => auth::handle(cmd, ep0).await,
        }
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
            "relay {}s: input {} -> {} | IF0 out -> {} | HID++ in {} -> {}, out {} -> {} | FFB in {} -> {}, out {} -> {} | dropped {}",
            STATS_INTERVAL.as_secs(),
            take(&BACKEND_INPUT),
            take(&STATS.input_sent),
            take(&IF0_WRITTEN),
            take(&BACKEND_HIDPP),
            take(&STATS.hidpp_sent),
            take(&STATS.hidpp_set_report),
            take(&HIDPP_WRITTEN),
            take(&BACKEND_FFB),
            take(&STATS.ffb_sent),
            take(&STATS.ffb_received),
            take(&FFB_WRITTEN),
            take(&STATS.dropped),
        );
    }
}
