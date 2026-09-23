#![no_std]
#![no_main]

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_time::Timer;
use panic_probe as _;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(2048, log::LevelFilter::Debug, driver);
}

#[embassy_executor::main(
    executor = "embassy_rp::executor::Executor",
    entry = "cortex_m_rt::entry"
)]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let usb_driver = Driver::new(p.USB, Irqs);

    spawner.spawn(logger_task(usb_driver).unwrap());

    let mut counter: u32 = 0;

    loop {
        log::info!("rp-wheel-bridge alive: {}", counter);

        counter = counter.wrapping_add(1);

        Timer::after_secs(1).await;
    }
}
