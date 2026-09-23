//! Generic HID probing for the device on the USB-A host port.
//!
//! Dumps each HID interface's report descriptor (hex + per-report size summary), reads
//! its feature reports, and logs raw input reports from every interrupt IN endpoint.
//!
//! Done here rather than with `embassy_usb_host::class::hid` because:
//! - `HidHost::new` only binds the *first* HID interface (`find_hid`), and the G Pro
//!   exposes three;
//! - `hid_report::ReportDescriptor` parses Input items only, while Output/Feature
//!   report sizes matter for FFB / HID++.

use core::fmt;

use embassy_futures::join::join_array;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embassy_usb_driver::host::{PipeError, UsbHostAllocator, UsbPipe, pipe};
use embassy_usb_driver::{EndpointInfo, EndpointType};
use embassy_usb_host::control::SetupPacket;
use embassy_usb_host::descriptor::{
    ConfigurationDescriptor, DescriptorVisitor, EndpointDescriptor, InterfaceDescriptor,
};
use embassy_usb_host::handler::EnumerationInfo;

use crate::usb_host::{ControlPipe, HostBus, open_ep0};

/// HID interfaces tracked per device (the G Pro has 3).
pub const MAX_HID_INTERFACES: usize = 4;

/// Largest report descriptor fetched; longer ones are truncated.
const REPORT_DESC_BUF_LEN: usize = 512;

/// Full-speed interrupt endpoints carry at most 64 bytes per transaction.
const MAX_REPORT_LEN: usize = 64;

/// Minimum spacing between logged reports per interface. Changed reports inside the
/// window are coalesced and only the latest is logged.
const REPORT_LOG_INTERVAL: Duration = Duration::from_millis(100);

/// Report-rate statistics period.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Pause after a large log burst so the logger pipe can drain (it drops on full).
const LOG_DRAIN: Duration = Duration::from_millis(50);

/// Largest feature report read (plus its ID byte); longer ones are truncated.
const FEATURE_BUF_LEN: usize = 256;

/// Usage page of the PlayStation authentication reports (F0..F3). Not read: a GET_REPORT
/// there can change the device's auth state.
const USAGE_PAGE_PS_AUTH: u16 = 0xfff0;

/// (report ID, type) pairs tracked per report descriptor.
const MAX_REPORTS: usize = 24;

const CLASS_HID: u8 = 0x03;
const DESC_TYPE_HID: u8 = 0x21;
const DESC_TYPE_REPORT: u8 = 0x22;
const HID_REQ_GET_REPORT: u8 = 0x01;
const HID_REQ_SET_IDLE: u8 = 0x0a;
const HID_REPORT_TYPE_FEATURE: u16 = 0x03;

#[derive(Clone, Copy)]
pub struct HidInterface {
    pub number: u8,
    pub report_desc_len: u16,
    pub in_ep: Option<EndpointDescriptor>,
    pub out_ep: Option<EndpointDescriptor>,
}

/// Collect the HID interfaces (alternate setting 0) of a configuration descriptor.
pub fn find_interfaces(config: &[u8]) -> [Option<HidInterface>; MAX_HID_INTERFACES] {
    let mut finder = HidFinder {
        ifaces: [None; MAX_HID_INTERFACES],
        current: None,
    };
    if let Ok(cfg) = ConfigurationDescriptor::try_from_slice(config) {
        let _ = cfg.visit_descriptors(&mut finder);
    }
    finder.ifaces
}

struct HidFinder {
    ifaces: [Option<HidInterface>; MAX_HID_INTERFACES],
    /// Slot of the HID interface whose sub-descriptors are being visited.
    current: Option<usize>,
}

impl<'a> DescriptorVisitor<'a> for HidFinder {
    type Error = core::convert::Infallible;

    fn on_interface(&mut self, i: &InterfaceDescriptor<'a>) -> bool {
        self.current = None;
        if i.interface_class != CLASS_HID || i.alternate_setting != 0 {
            return true;
        }
        match self.ifaces.iter().position(Option::is_none) {
            Some(slot) => {
                self.ifaces[slot] = Some(HidInterface {
                    number: i.interface_number,
                    report_desc_len: 0,
                    in_ep: None,
                    out_ep: None,
                });
                self.current = Some(slot);
            }
            None => log::warn!(
                "more than {} HID interfaces; ignoring the rest",
                MAX_HID_INTERFACES
            ),
        }
        true
    }

    fn on_endpoint(&mut self, _i: &InterfaceDescriptor<'a>, e: &EndpointDescriptor) -> bool {
        if let Some(iface) = self.current.and_then(|slot| self.ifaces[slot].as_mut())
            && e.ep_type() == EndpointType::Interrupt
        {
            let ep = if e.is_in() {
                &mut iface.in_ep
            } else {
                &mut iface.out_ep
            };
            ep.get_or_insert(*e);
        }
        true
    }

    fn on_other(
        &mut self,
        _i: Option<&InterfaceDescriptor<'a>>,
        raw: &[u8],
    ) -> Result<bool, Self::Error> {
        let Some(iface) = self.current.and_then(|slot| self.ifaces[slot].as_mut()) else {
            return Ok(true);
        };
        if raw[1] == DESC_TYPE_HID && raw.len() >= 6 {
            // HID 1.11 §6.2.1: bNumDescriptors at [5], then (bDescriptorType, wDescriptorLength).
            let listed = raw[6..].as_chunks::<3>().0;
            for entry in listed.iter().take(usize::from(raw[5])) {
                if entry[0] == DESC_TYPE_REPORT {
                    iface.report_desc_len = u16::from_le_bytes([entry[1], entry[2]]);
                    break;
                }
            }
        }
        Ok(true)
    }
}

/// Fetch and log each interface's report descriptor and feature reports, then
/// SET_IDLE(0) it.
pub async fn probe(bus: &HostBus, info: &EnumerationInfo, ifaces: &[Option<HidInterface>]) {
    let mut ep0 = match open_ep0(bus, info) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("cannot open EP0 for HID probing: {:?}", e);
            return;
        }
    };
    let mut buf = [0u8; REPORT_DESC_BUF_LEN];

    for iface in ifaces.iter().flatten() {
        let len = usize::from(iface.report_desc_len).min(buf.len());
        log::info!(
            "HID interface {} report descriptor ({} bytes):",
            iface.number,
            iface.report_desc_len
        );
        if len < usize::from(iface.report_desc_len) {
            log::warn!("  truncated to {} bytes", len);
        }

        let setup = SetupPacket::get_hid_report_descriptor(iface.number, len as u16);
        match ep0.control_in(&setup.to_bytes(), &mut buf[..len]).await {
            Ok(n) => {
                log_hex_dump(&buf[..n]);
                Timer::after(LOG_DRAIN).await;
                let reports = log_report_summary(&buf[..n]);
                Timer::after(LOG_DRAIN).await;
                read_feature_reports(&mut ep0, iface.number, &reports).await;
            }
            Err(e) => log::warn!("  GET_DESCRIPTOR(report) failed: {:?}", e),
        }

        // SET_IDLE(duration 0, all reports): report only on change. HID 1.11 §7.2.4
        // makes this optional, so a STALL is fine.
        let setup =
            SetupPacket::class_interface_out(HID_REQ_SET_IDLE, 0, u16::from(iface.number), 0);
        match ep0.control_out(&setup.to_bytes(), &[]).await {
            Ok(()) => {}
            Err(PipeError::Stall) => log::info!("  SET_IDLE stalled (optional, ignored)"),
            Err(e) => log::warn!("  SET_IDLE failed: {:?}", e),
        }

        Timer::after(LOG_DRAIN).await;
    }
}

/// GET_REPORT(Feature) every feature report with a report ID, except the auth reports,
/// and log its contents.
async fn read_feature_reports(
    ep0: &mut ControlPipe,
    iface: u8,
    reports: &[Option<ReportSize>; MAX_REPORTS],
) {
    let mut buf = [0u8; FEATURE_BUF_LEN];
    let features = reports
        .iter()
        .flatten()
        .filter(|r| r.kind == ReportKind::Feature && r.id != 0);
    for r in features {
        if r.usage_page == USAGE_PAGE_PS_AUTH {
            log::info!("  feature {:#04x}: auth report, not read", r.id);
            continue;
        }
        let len = (r.bits.div_ceil(8) as usize + 1).min(buf.len());
        // HID 1.11 §7.2.1: wValue = report type << 8 | report ID.
        let setup = SetupPacket::class_interface_in(
            HID_REQ_GET_REPORT,
            HID_REPORT_TYPE_FEATURE << 8 | u16::from(r.id),
            u16::from(iface),
            len as u16,
        );
        match ep0.control_in(&setup.to_bytes(), &mut buf[..len]).await {
            Ok(n) => {
                log::info!("  feature {:#04x} ({} bytes):", r.id, n);
                log_hex_dump(&buf[..n]);
            }
            Err(e) => log::warn!("  feature {:#04x}: GET_REPORT failed: {:?}", r.id, e),
        }
        Timer::after(LOG_DRAIN).await;
    }
}

/// Log input reports from every HID interface. Returns only if all monitors stop.
pub async fn monitor(
    bus: &HostBus,
    info: &EnumerationInfo,
    ifaces: &[Option<HidInterface>; MAX_HID_INTERFACES],
) {
    join_array(core::array::from_fn::<_, MAX_HID_INTERFACES, _>(|i| {
        monitor_interface(bus, info, ifaces[i])
    }))
    .await;
}

async fn monitor_interface(bus: &HostBus, info: &EnumerationInfo, iface: Option<HidInterface>) {
    let Some((iface, ep)) = iface.and_then(|i| Some((i, i.in_ep?))) else {
        return;
    };
    if usize::from(ep.max_packet_size) > MAX_REPORT_LEN {
        log::warn!(
            "IF{}: max packet {} unsupported",
            iface.number,
            ep.max_packet_size
        );
        return;
    }

    let mut pipe = match bus.alloc_pipe::<pipe::Interrupt, pipe::In>(
        info.device_address,
        &EndpointInfo::from(ep),
        info.split(),
    ) {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "IF{}: cannot open IN endpoint {:#04x}: {:?}",
                iface.number,
                ep.endpoint_address,
                e
            );
            return;
        }
    };

    let mut buf = [0u8; MAX_REPORT_LEN];
    let mut last = [0u8; MAX_REPORT_LEN];
    let mut last_len = 0usize;
    let mut unlogged_change = false;
    let mut last_logged = Instant::from_ticks(0);
    let mut reports: u32 = 0;
    let mut stats_start = Instant::now();

    loop {
        // The timeout only bounds how long a coalesced change can stay unlogged.
        // Cancelling request_in is safe: it awaits only between transactions.
        match with_timeout(REPORT_LOG_INTERVAL, pipe.request_in(&mut buf)).await {
            Ok(Ok(n)) => {
                reports += 1;
                if buf[..n] != last[..last_len] {
                    last[..n].copy_from_slice(&buf[..n]);
                    last_len = n;
                    unlogged_change = true;
                }
            }
            Ok(Err(e)) => {
                log::warn!(
                    "IF{}: IN {:#04x} failed: {:?}; monitor stopped",
                    iface.number,
                    ep.endpoint_address,
                    e
                );
                return;
            }
            Err(_timeout) => {}
        }

        let now = Instant::now();
        if unlogged_change && now - last_logged >= REPORT_LOG_INTERVAL {
            log::info!(
                "IF{} [{}]: {}",
                iface.number,
                last_len,
                Hex(&last[..last_len])
            );
            unlogged_change = false;
            last_logged = now;
        }
        if now - stats_start >= STATS_INTERVAL {
            if reports > 0 {
                log::info!(
                    "IF{}: {} reports in {} s",
                    iface.number,
                    reports,
                    STATS_INTERVAL.as_secs()
                );
            }
            reports = 0;
            stats_start = now;
        }
    }
}

fn log_hex_dump(data: &[u8]) {
    for (i, line) in data.chunks(16).enumerate() {
        log::info!("  {:04x}: {}", i * 16, Hex(line));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportKind {
    Input,
    Output,
    Feature,
}

impl ReportKind {
    fn name(self) -> &'static str {
        match self {
            ReportKind::Input => "input",
            ReportKind::Output => "output",
            ReportKind::Feature => "feature",
        }
    }
}

#[derive(Clone, Copy)]
struct ReportSize {
    id: u8,
    kind: ReportKind,
    bits: u32,
    /// Usage page in effect at the report's first main item.
    usage_page: u16,
}

#[derive(Clone, Copy, Default)]
struct Globals {
    usage_page: u16,
    report_size: u32,
    report_count: u32,
    report_id: u8,
}

/// Log application collections and the byte size of every (report ID, type) pair, and
/// return those pairs.
fn log_report_summary(desc: &[u8]) -> [Option<ReportSize>; MAX_REPORTS] {
    let mut reports: [Option<ReportSize>; MAX_REPORTS] = [None; MAX_REPORTS];
    let mut g = Globals::default();
    let mut stack = [Globals::default(); 4];
    let mut depth = 0usize;
    let mut usage: Option<u32> = None;

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xfe {
            // Long item (HID 1.11 §6.2.2.3): bDataSize, bLongItemTag, data.
            let size = desc.get(i + 1).copied().unwrap_or(0) as usize;
            i += 3 + size;
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            n => n as usize,
        };
        let Some(bytes) = desc.get(i + 1..i + 1 + size) else {
            log::warn!("  report descriptor truncated at {:#x}", i);
            break;
        };
        let data = bytes
            .iter()
            .rev()
            .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
        i += 1 + size;

        match (prefix >> 2) & 0x03 {
            // Main
            0 => {
                let kind = match prefix >> 4 {
                    0x8 => Some(ReportKind::Input),
                    0x9 => Some(ReportKind::Output),
                    0xb => Some(ReportKind::Feature),
                    0xa => {
                        if data == 0x01 {
                            log::info!(
                                "  application collection: usage page {:#06x}, usage {:#06x}",
                                g.usage_page,
                                usage.unwrap_or(0)
                            );
                        }
                        None
                    }
                    _ => None,
                };
                if let Some(kind) = kind {
                    let bits = g.report_size * g.report_count;
                    match reports
                        .iter_mut()
                        .flatten()
                        .find(|r| r.id == g.report_id && r.kind == kind)
                    {
                        Some(r) => r.bits += bits,
                        None => match reports.iter_mut().find(|r| r.is_none()) {
                            Some(slot) => {
                                *slot = Some(ReportSize {
                                    id: g.report_id,
                                    kind,
                                    bits,
                                    usage_page: g.usage_page,
                                })
                            }
                            None => log::warn!(
                                "  more than {} reports; summary incomplete",
                                MAX_REPORTS
                            ),
                        },
                    }
                }
                usage = None;
            }
            // Global
            1 => match prefix >> 4 {
                0x0 => g.usage_page = data as u16,
                0x7 => g.report_size = data,
                0x8 => g.report_id = data as u8,
                0x9 => g.report_count = data,
                0xa if depth < stack.len() => {
                    stack[depth] = g;
                    depth += 1;
                }
                0xb if depth > 0 => {
                    depth -= 1;
                    g = stack[depth];
                }
                _ => {}
            },
            // Local: remember the first Usage for the next collection line.
            2 if prefix >> 4 == 0x0 => {
                usage.get_or_insert(data);
            }
            _ => {}
        }
    }

    for r in reports.iter().flatten() {
        let id_byte = if r.id != 0 { " + 1 ID byte" } else { "" };
        log::info!(
            "  report id {:#04x} {}: {} bytes{}, usage page {:#06x}",
            r.id,
            r.kind.name(),
            r.bits.div_ceil(8),
            id_byte,
            r.usage_page
        );
    }
    reports
}

/// Space-separated lowercase hex bytes.
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}
