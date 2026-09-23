//! Host side of the bridge: the G Pro Xbox/PC (c272) on the USB-A port, forwarded to
//! the PS device ([`crate::ps_device`]) on the native port.
//!
//! - IF0 input (EP 0x81, 30 bytes) is translated ([`crate::input_map`]); the only
//!   interface whose format differs.
//! - IF1 HID++ (EP 0x82 IN, SET_REPORT on EP0) and IF2 force feedback (EP 0x83 IN,
//!   EP 0x03 OUT) carry the same reports on both sides and are forwarded unchanged.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::join::{join, join5};
use embassy_time::{Duration, Ticker};
use embassy_usb_driver::EndpointInfo;
use embassy_usb_driver::host::{UsbHostAllocator, UsbPipe, pipe};
use embassy_usb_host::control::SetupPacket;
use embassy_usb_host::descriptor::EndpointDescriptor;
use embassy_usb_host::handler::EnumerationInfo;

use crate::hid::{HidInterface, MAX_HID_INTERFACES};
use crate::input_map;
use crate::ps_device::{self, FFB_IN, FFB_OUT, HIDPP_IN, HIDPP_OUT, Packet, STATS};
use crate::usb_host::{ControlPipe, HostBus, open_ep0};

pub const WHEEL_VID: u16 = 0x046d;
pub const WHEEL_PID: u16 = 0xc272;

const IF_INPUT: u8 = 0;
const IF_HIDPP: u8 = 1;
const IF_FFB: u8 = 2;

const HID_SET_REPORT: u8 = 0x09;

const STATS_INTERVAL: Duration = Duration::from_secs(5);

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
            HIDPP_IN.try_send(p)
        }),
        forward_hidpp_out(&mut ep0),
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

/// HID++ SET_REPORTs from the console → the same request to the wheel's IF1.
async fn forward_hidpp_out(ep0: &mut ControlPipe) {
    loop {
        let msg = HIDPP_OUT.receive().await;
        let data = msg.data.bytes();
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
