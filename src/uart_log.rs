//! `log` output on UART0: GPIO0 TX, GPIO1 RX, 921600 8N1.
//!
//! Both cores log. Each record is formatted on the logging core and put into a pipe
//! whole or not at all, so lines from the two cores never interleave; [`task`] (core 0)
//! moves the pipe into the UART's interrupt-driven TX buffer. Logging never blocks:
//! when the pipe is full the record is dropped, and the number of dropped records is
//! reported in front of the next one that fits.
//!
//! RX is wired but not read yet.

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_rp::Peri;
use embassy_rp::interrupt::typelevel::{Binding, UART0_IRQ};
use embassy_rp::peripherals::{PIN_0, PIN_1, UART0};
use embassy_rp::uart::{self, BufferedInterruptHandler, BufferedUart};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;
use embedded_io_async::Write as _;
use log::{LevelFilter, Log, Metadata, Record};
use static_cell::StaticCell;

pub const BAUDRATE: u32 = 921_600;

const LEVEL: LevelFilter = LevelFilter::Debug;

/// Log bytes buffered between the logging cores and the UART. Sized for the bursty
/// descriptor dump.
const PIPE_LEN: usize = 4096;

/// Longest formatted record; longer ones are truncated. Fits a 64-byte report in hex
/// with its prefix.
const LINE_LEN: usize = 320;

const TX_BUF_LEN: usize = 256;
const RX_BUF_LEN: usize = 64;

static PIPE: Pipe<CriticalSectionRawMutex, PIPE_LEN> = Pipe::new();

/// Records dropped since the last one that made it into the pipe.
static DROPPED: AtomicU32 = AtomicU32::new(0);

static LOGGER: UartLogger = UartLogger;

/// Configure UART0 and install the logger. Records logged from here on are buffered
/// until [`task`] runs.
pub fn init(
    uart: Peri<'static, UART0>,
    tx: Peri<'static, PIN_0>,
    rx: Peri<'static, PIN_1>,
    irq: impl Binding<UART0_IRQ, BufferedInterruptHandler<UART0>>,
) -> BufferedUart {
    static TX_BUF: StaticCell<[u8; TX_BUF_LEN]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; RX_BUF_LEN]> = StaticCell::new();

    let mut config = uart::Config::default();
    config.baudrate = BAUDRATE;
    let uart = BufferedUart::new(
        uart,
        tx,
        rx,
        irq,
        TX_BUF.init([0; TX_BUF_LEN]),
        RX_BUF.init([0; RX_BUF_LEN]),
        config,
    );

    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LEVEL);
    }
    uart
}

#[embassy_executor::task]
pub async fn task(uart: BufferedUart) -> ! {
    // RX stays owned (and its interrupt enabled) for the lifetime of the task.
    let (mut tx, _rx) = uart.split();
    let mut chunk = [0u8; 64];
    loop {
        let n = PIPE.read(&mut chunk).await;
        let _ = tx.write_all(&chunk[..n]).await;
    }
}

struct UartLogger;

impl Log for UartLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let mut line = LineBuf::<LINE_LEN>::new();
        let _ = write!(line, "{}", record.args());
        line.end_line();

        // One critical section (both cores) for the capacity check and the writes.
        critical_section::with(|_| {
            let dropped = DROPPED.load(Ordering::Relaxed);
            let mut notice = LineBuf::<40>::new();
            if dropped != 0 {
                let _ = write!(notice, "[{} log records dropped]", dropped);
                notice.end_line();
            }
            if PIPE.free_capacity() < notice.len + line.len {
                DROPPED.store(dropped.saturating_add(1), Ordering::Relaxed);
                return;
            }
            push(notice.bytes());
            push(line.bytes());
            DROPPED.store(0, Ordering::Relaxed);
        });
    }

    fn flush(&self) {}
}

/// Write all of `bytes`; the caller has checked the free capacity. The pipe does not
/// write across its wraparound in one call, hence the loop.
fn push(mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match PIPE.try_write(bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(_) => return,
        }
    }
}

/// Fixed-size line; text past the end is cut off, leaving room for the line ending.
struct LineBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> LineBuf<N> {
    const EOL: &[u8] = b"\r\n";

    fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    fn end_line(&mut self) {
        self.buf[self.len..self.len + Self::EOL.len()].copy_from_slice(Self::EOL);
        self.len += Self::EOL.len();
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> fmt::Write for LineBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = N - Self::EOL.len() - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}
