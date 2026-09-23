#![no_std]
#![no_main]

mod hid;
mod usb_host;

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_time::Timer;
use panic_probe as _;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
});

/// Time for the PC to re-enumerate the CDC logger after a reset, so the serial monitor
/// can reattach before the USB host starts logging.
const BOOT_LOG_DELAY_MS: u64 = 2000;

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    // Larger than the usual 2048: the descriptor dump is bursty and is buffered
    // until the serial monitor is attached.
    embassy_usb_logger::run!(4096, log::LevelFilter::Debug, driver);
}

#[embassy_executor::main(
    executor = "embassy_rp::executor::Executor",
    entry = "cortex_m_rt::entry"
)]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let usb_driver = Driver::new(p.USB, Irqs);

    spawner.spawn(logger_task(usb_driver).unwrap());

    // The logger is installed when logger_task first runs; anything logged before
    // this await would be dropped.
    Timer::after_millis(BOOT_LOG_DELAY_MS).await;

    log::info!("rp-wheel-bridge boot");
    log::info!("native USB logger initialized");

    let host_bus = usb_host::init(p.PIO0, p.PIN_12, p.PIN_13, Irqs);
    log::info!("PIO USB host initialized (PIO0, D+=GPIO12, D-=GPIO13)");

    spawner.spawn(usb_host::idle_task(host_bus).unwrap());
    spawner.spawn(usb_host::host_task(host_bus).unwrap());

    let mut counter: u32 = 0;

    loop {
        Timer::after_secs(10).await;

        log::info!("rp-wheel-bridge alive: {}", counter);

        counter = counter.wrapping_add(1);
    }
}
