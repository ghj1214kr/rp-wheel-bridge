//! PS4/PS5 peripheral authentication relay: IF0 feature reports F0-F3, answered to the
//! console from a cache that the host side fills from a device that can sign (now
//! DriveHub; later the licensed auth pad).
//!
//! Report layout (GP2040-CE / jfedor2 wheel-adapter, also used by the earlier
//! ps5-wheel-passthrough project):
//! - F3 (GET, 7 bytes + ID): auth reset / sizes, e.g. `00 38 38 00 00 00 00`.
//! - F0 (SET, 63 + ID) ×5: `F0 <nonce id> <page 0..4> 00 <56 bytes> <4 bytes>`.
//! - F2 (GET, 15 + ID): `F2 <nonce id> <0x10 signing | 0x00 ready> 00 ...`.
//! - F1 (GET, 63 + ID) ×19: `F1 <nonce id> <page 0..18> 00 <56 bytes> ...`.
//!
//! Signers: DriveHub in the relay role ([`crate::relay`] drives [`handle`]), or in the
//! wheel role any other device behind the bridge that answers F3 ([`try_signer`], then
//! [`serve`]) — the licensed auth pad.
//!
//! The console's GET_REPORTs must be answered on the spot (embassy-usb control handlers
//! are synchronous), so the relay leans on the console polling F2: nonce pages go to
//! the signer as they arrive, and until all signature pages are fetched from it the
//! console is told "signing". Only that busy F2 is made up here; everything else is the
//! signer's own bytes.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb_driver::host::UsbPipe;
use embassy_usb_host::control::SetupPacket;

use crate::device::Packet;
use crate::hid::Hex;
use crate::usb_host::ControlPipe;

pub const ID_NONCE: u8 = 0xf0;
pub const ID_SIGNATURE: u8 = 0xf1;
pub const ID_STATE: u8 = 0xf2;
pub const ID_RESET: u8 = 0xf3;

/// Report lengths, report ID included.
const SIGNATURE_LEN: usize = 64;
const STATE_LEN: usize = 16;
const RESET_LEN: usize = 8;

const NONCE_LEN: usize = 64;
const LAST_NONCE_PAGE: u8 = 4;
const NONCE_PAGES: usize = LAST_NONCE_PAGE as usize + 1;
const SIGNATURE_PAGES: usize = 19;

/// F2 byte 2.
const STATE_SIGNING: u8 = 0x10;
const STATE_READY: u8 = 0x00;

/// How often the signer's F2 is polled, and for how long.
const SIGNER_POLL: Duration = Duration::from_millis(20);
const SIGNER_TIMEOUT: Duration = Duration::from_secs(10);
/// Signing runs per console nonce. The HORI OCTA steps its F1 page on every GET it
/// takes, also one whose reply is lost (500 ms timeout) or a SETUP resent after a
/// garbled ACK (a page skipped), so a lost page can only be had again by signing the
/// same nonce once more. Errors come in bursts of a second or two. Kept low: after
/// many signings in a row (10 attempts per round) the OCTA once stayed "signing" for
/// good, round after round.
const SIGN_ATTEMPTS: u32 = 4;
/// Pauses when signing again. Attempts fired back to back (three in 0.3 s) ended with
/// the OCTA refusing a nonce page, and twice the wheel on the same hub dropped out of
/// force feedback half a second after such a burst.
const RESIGN_PAUSE: Duration = Duration::from_millis(300);
const RESET_SETTLE: Duration = Duration::from_millis(50);
const NONCE_PAGE_GAP: Duration = Duration::from_millis(10);
/// A "ready" F2 right after a nonce can be the previous signing's; it is believed
/// only once "signing" has been seen, or after this long.
const STALE_READY_GRACE: Duration = Duration::from_millis(500);

/// F3 as DriveHub (both captures) answers it; given to the console until a signer's
/// own F3 has been read.
const DEFAULT_RESET: [u8; RESET_LEN] = [0xf3, 0x00, 0x38, 0x38, 0x00, 0x00, 0x00, 0x00];

const HID_GET_REPORT: u8 = 0x01;
const HID_SET_REPORT: u8 = 0x09;
const REPORT_TYPE_FEATURE: u16 = 0x03;

type CS = CriticalSectionRawMutex;

struct State {
    /// The signer's F3 answer.
    reset: [u8; RESET_LEN],
    nonce_id: u8,
    /// Signature pages fetched and the signer's final F2 cached.
    ready: bool,
    ready_state: [u8; STATE_LEN],
    signature: [[u8; SIGNATURE_LEN]; SIGNATURE_PAGES],
    /// Next F1 page to hand to the console.
    next_page: usize,
}

static STATE: Mutex<CS, RefCell<State>> = Mutex::new(RefCell::new(State {
    reset: DEFAULT_RESET,
    nonce_id: 0,
    ready: false,
    ready_state: [0; STATE_LEN],
    signature: [[0; SIGNATURE_LEN]; SIGNATURE_PAGES],
    next_page: 0,
}));

/// Console-side events for the host side to act on.
pub enum Cmd {
    /// The console read F3: reset the signer too.
    Reset,
    /// One nonce page (F0 report, ID included).
    Nonce(Packet),
}

pub static CMD: Channel<CS, Cmd, 8> = Channel::new();

// ---- console side (core 0, inside the synchronous control handler) ----

/// GET_REPORT(Feature) for F1/F2/F3 into `buf`. `None`: not an auth report, or nothing
/// to answer with (the request is then rejected).
pub fn get_report(id: u8, buf: &mut [u8]) -> Option<usize> {
    STATE.lock(|s| {
        let mut s = s.borrow_mut();
        match id {
            ID_RESET => {
                s.ready = false;
                s.next_page = 0;
                send(Cmd::Reset);
                let reset = s.reset;
                buf[..RESET_LEN].copy_from_slice(&reset);
                log::info!("auth: console GET F3 -> {}", Hex(&reset));
                Some(RESET_LEN)
            }
            ID_STATE => {
                if s.ready {
                    buf[..STATE_LEN].copy_from_slice(&s.ready_state);
                } else {
                    buf[..STATE_LEN].fill(0);
                    buf[0] = ID_STATE;
                    buf[1] = s.nonce_id;
                    buf[2] = STATE_SIGNING;
                }
                log::debug!("auth: console GET F2 -> {}", Hex(&buf[..3]));
                Some(STATE_LEN)
            }
            ID_SIGNATURE => {
                if !s.ready || s.next_page >= SIGNATURE_PAGES {
                    log::warn!(
                        "auth: console GET F1 with no signature page (ready {}, page {})",
                        s.ready,
                        s.next_page
                    );
                    return None;
                }
                let page = s.next_page;
                buf[..SIGNATURE_LEN].copy_from_slice(&s.signature[page]);
                s.next_page += 1;
                log::debug!("auth: console GET F1 page {}", page);
                Some(SIGNATURE_LEN)
            }
            _ => None,
        }
    })
}

/// SET_REPORT(Feature) F0 (report ID included). Returns false for other reports.
pub fn set_report(id: u8, data: &[u8]) -> bool {
    if id != ID_NONCE {
        return false;
    }
    STATE.lock(|s| {
        let mut s = s.borrow_mut();
        s.ready = false;
        s.next_page = 0;
        s.nonce_id = data.get(1).copied().unwrap_or(0);
    });
    log::info!(
        "auth: console SET F0 nonce id {} page {}",
        data.get(1).copied().unwrap_or(0),
        data.get(2).copied().unwrap_or(0)
    );
    send(Cmd::Nonce(Packet::new(data)));
    true
}

fn send(cmd: Cmd) {
    if CMD.try_send(cmd).is_err() {
        log::warn!("auth: command queue full; dropped");
    }
}

// ---- signer side (core 1, the host's control pipe to the signing device) ----

/// Read the signer's F3 (on HID interface `iface`) and answer the console's F3 with it
/// from now on. Returns whether the device answered, i.e. can sign.
pub async fn try_signer(ep0: &mut ControlPipe, iface: u16) -> bool {
    let mut buf = [0u8; RESET_LEN];
    match get_feature(ep0, iface, ID_RESET, &mut buf).await {
        Some(n) => {
            log::info!("auth: signer F3 = {}", Hex(&buf[..n]));
            STATE.lock(|s| s.borrow_mut().reset = buf);
            true
        }
        None => false,
    }
}

/// Wheel role: act on the console's auth events with this signer until a transfer
/// fails badly enough to stop. Events queued while there was no signer are dropped
/// (the console starts over).
pub async fn serve(ep0: &mut ControlPipe, iface: u16) -> ! {
    CMD.clear();
    log::info!("auth: signer ready (IF{})", iface);
    loop {
        let cmd = CMD.receive().await;
        handle(cmd, ep0, iface).await;
    }
}

/// The signer refused a nonce page of the current nonce.
static NONCE_REFUSED: AtomicBool = AtomicBool::new(false);

/// The console's nonce pages, kept by the signer side to sign again.
static NONCE: Mutex<CS, RefCell<[[u8; NONCE_LEN]; NONCE_PAGES]>> =
    Mutex::new(RefCell::new([[0; NONCE_LEN]; NONCE_PAGES]));

/// Act on one console event, with the signer's auth reports on HID interface `iface`.
pub async fn handle(cmd: Cmd, ep0: &mut ControlPipe, iface: u16) {
    match cmd {
        Cmd::Reset => reset_signer(ep0, iface).await,
        Cmd::Nonce(page) => {
            let data = page.bytes();
            if let Some(&n) = data.get(2)
                && usize::from(n) < NONCE_PAGES
                && data.len() == NONCE_LEN
            {
                NONCE.lock(|p| p.borrow_mut()[usize::from(n)].copy_from_slice(data));
            }
            // A page the signer refused is resent with all the others once the last
            // one is in (the console goes on at its pace regardless).
            if !set_nonce(ep0, iface, data).await {
                NONCE_REFUSED.store(true, Ordering::Relaxed);
            }
            if data.get(2) == Some(&LAST_NONCE_PAGE) {
                let resend = NONCE_REFUSED.swap(false, Ordering::Relaxed);
                sign(ep0, iface, resend).await;
            } else if data.get(2) == Some(&0) {
                NONCE_REFUSED.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// SET_REPORT(Feature) F0 to the signer.
async fn set_nonce(ep0: &mut ControlPipe, iface: u16, data: &[u8]) -> bool {
    let setup = SetupPacket::class_interface_out(
        HID_SET_REPORT,
        REPORT_TYPE_FEATURE << 8 | u16::from(ID_NONCE),
        iface,
        data.len() as u16,
    );
    if let Err(e) = ep0.control_out(&setup.to_bytes(), data).await {
        log::warn!("auth: signer SET F0 failed: {:?}", e);
        return false;
    }
    log::debug!("auth: signer SET F0 [{}]: {}", data.len(), Hex(data));
    true
}

/// Get the whole signature for the nonce just sent, signing it again (all nonce pages
/// resent) whenever a page is lost. `resend`: the signer refused a nonce page, so
/// resend them all before the first attempt too.
async fn sign(ep0: &mut ControlPipe, iface: u16, resend: bool) {
    for attempt in 1..=SIGN_ATTEMPTS {
        if attempt > 1 || resend {
            log::info!("auth: sending the nonce again (attempt {})", attempt);
            Timer::after(RESIGN_PAUSE).await;
            // Start over as the console does: F3 first.
            reset_signer(ep0, iface).await;
            Timer::after(RESET_SETTLE).await;
            let pages = NONCE.lock(|p| *p.borrow());
            for page in &pages {
                if !set_nonce(ep0, iface, page).await {
                    return;
                }
                Timer::after(NONCE_PAGE_GAP).await;
            }
        }
        match fetch_signature(ep0, iface).await {
            Fetch::Done => return,
            Fetch::PageLost => {}
            Fetch::NotReady => {
                reset_signer(ep0, iface).await;
                return;
            }
        }
    }
    log::warn!("auth: no complete signature after {} attempts", SIGN_ATTEMPTS);
    reset_signer(ep0, iface).await;
}

/// GET F3 on the signer, which resets its auth state.
async fn reset_signer(ep0: &mut ControlPipe, iface: u16) {
    let mut buf = [0u8; RESET_LEN];
    if let Some(n) = get_feature(ep0, iface, ID_RESET, &mut buf).await {
        log::info!("auth: signer F3 (reset) = {}", Hex(&buf[..n]));
    }
}

enum Fetch {
    Done,
    /// A signature page was not received (or one was skipped); sign again.
    PageLost,
    /// The signer never became ready.
    NotReady,
}

/// Poll the signer's F2 until it is ready, then fetch every F1 page and publish them.
async fn fetch_signature(ep0: &mut ControlPipe, iface: u16) -> Fetch {
    let start = Instant::now();
    let mut state = [0u8; STATE_LEN];
    let mut last = [0xffu8; 3];
    let mut seen_signing = false;
    loop {
        if start.elapsed() >= SIGNER_TIMEOUT {
            log::warn!(
                "auth: signer not ready after {} s",
                SIGNER_TIMEOUT.as_secs()
            );
            return Fetch::NotReady;
        }
        if get_feature(ep0, iface, ID_STATE, &mut state)
            .await
            .is_some()
        {
            if state[..3] != last {
                log::info!(
                    "auth: signer F2 = {} ({} ms)",
                    Hex(&state),
                    start.elapsed().as_millis()
                );
                last.copy_from_slice(&state[..3]);
            }
            seen_signing |= state[2] == STATE_SIGNING;
            if state[2] == STATE_READY && (seen_signing || start.elapsed() >= STALE_READY_GRACE)
            {
                break;
            }
        }
        Timer::after(SIGNER_POLL).await;
    }

    let mut pages = [[0u8; SIGNATURE_LEN]; SIGNATURE_PAGES];
    for (i, page) in pages.iter_mut().enumerate() {
        if get_feature(ep0, iface, ID_SIGNATURE, page).await.is_none() {
            log::warn!("auth: signer F1 page {} lost", i);
            return Fetch::PageLost;
        }
        // Byte 2 is the page number; the signer steps it on every GET.
        if usize::from(page[2]) != i {
            log::warn!("auth: signer F1 page {} came as page {}", i, page[2]);
            return Fetch::PageLost;
        }
        log::debug!("auth: signer F1 [{}]: {}", i, Hex(page));
    }
    STATE.lock(|s| {
        let mut s = s.borrow_mut();
        // Answer with the console's nonce id: DriveHub echoes it, the OCTA uses its own
        // counter. F1/F2 carry no checksum (their last 4 bytes are zero).
        let nonce_id = s.nonce_id;
        if state[1] != nonce_id {
            log::info!(
                "auth: signer nonce id {} rewritten to the console's {}",
                state[1],
                nonce_id
            );
        }
        state[1] = nonce_id;
        for page in pages.iter_mut() {
            page[1] = nonce_id;
        }
        s.signature = pages;
        s.ready_state = state;
        s.next_page = 0;
        s.ready = true;
    });
    log::info!(
        "auth: signature cached ({} pages, {} ms)",
        SIGNATURE_PAGES,
        start.elapsed().as_millis()
    );
    Fetch::Done
}

/// GET_REPORT(Feature, `id`) on HID interface `iface` into `buf`; `None` on failure
/// (logged).
async fn get_feature(ep0: &mut ControlPipe, iface: u16, id: u8, buf: &mut [u8]) -> Option<usize> {
    let setup = SetupPacket::class_interface_in(
        HID_GET_REPORT,
        REPORT_TYPE_FEATURE << 8 | u16::from(id),
        iface,
        buf.len() as u16,
    );
    match ep0.control_in(&setup.to_bytes(), buf).await {
        Ok(n) => Some(n),
        Err(e) => {
            log::warn!("auth: signer GET {:#04x} failed: {:?}", id, e);
            None
        }
    }
}
