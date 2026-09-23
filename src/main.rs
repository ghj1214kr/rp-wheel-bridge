#![no_std]
#![no_main]

mod hid;
mod uart_log;
mod usb_host;

use core::ptr::addr_of_mut;

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::clocks::{ClockConfig, CoreVoltage};
use embassy_rp::executor::Executor;
use embassy_rp::multicore::{Stack, spawn_core1};
use embassy_rp::peripherals::{PIO0, UART0};
use embassy_rp::{bind_interrupts, interrupt};
use embassy_time::Timer;
use panic_probe as _;
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    UART0_IRQ => embassy_rp::uart::BufferedInterruptHandler<UART0>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
});

/// System clock. rp-pio-usb-host clocks its TX state machine at 48 MHz and its RX
/// edge detector at 96 MHz; 192 MHz makes both dividers integers (4 / 2), so PIO steps
/// do not jitter by a system clock, and leaves headroom for the receive loop. RP2350 is
/// specified up to 200 MHz; the core voltage is raised one step for it, as the Pico SDK
/// does above 150 MHz.
const SYS_CLOCK_HZ: u32 = 192_000_000;
const CORE_VOLTAGE: CoreVoltage = CoreVoltage::V1_15;

static mut CORE1_STACK: Stack<16384> = Stack::new();
static EXECUTOR1: StaticCell<Executor> = StaticCell::new();

/// USB host frame timer (TIMER0 alarm 1), enabled on core 1 by `usb_host::init`.
#[interrupt]
fn TIMER0_IRQ_1() {
    usb_host::on_frame_timer_irq();
}

/// Core 0: UART logger (native USB: PS5 device later).
/// Core 1: PIO USB host only, so its timing-critical transactions never share an
/// executor or interrupts with the native USB stack.
#[embassy_executor::main(
    executor = "embassy_rp::executor::Executor",
    entry = "cortex_m_rt::entry"
)]
async fn main(spawner: Spawner) {
    let mut clocks =
        ClockConfig::system_freq(SYS_CLOCK_HZ).expect("SYS_CLOCK_HZ is reachable from 12 MHz");
    clocks.core_voltage = CORE_VOLTAGE;
    let p = embassy_rp::init(embassy_rp::config::Config::new(clocks));

    let log_uart = uart_log::init(p.UART0, p.PIN_0, p.PIN_1, Irqs);
    spawner.spawn(uart_log::task(log_uart).unwrap());

    log::info!(
        "rp-wheel-bridge boot (clk_sys {} Hz)",
        embassy_rp::clocks::clk_sys_freq()
    );
    log::info!(
        "UART logger initialized (UART0, TX=GPIO0, RX=GPIO1, {} 8N1)",
        uart_log::BAUDRATE
    );

    let (pio, dp, dm) = (p.PIO0, p.PIN_12, p.PIN_13);
    let (sof_pwm, sof_dma) = (p.PWM_SLICE7, p.DMA_CH10);
    spawn_core1(
        p.CORE1,
        // SAFETY: CORE1_STACK is only handed to core 1, once.
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| {
                // Created on core 1 so the PIO and frame-timer interrupts are enabled on
                // core 1's NVIC.
                let host_bus = usb_host::init(pio, dp, dm, Irqs);
                // SOFs from hardware: PWM slice 7 wraps every 1 ms and paces DMA channel
                // 10, which starts the PIO state machine holding the next SOF.
                let hw_sof = host_bus.enable_hw_sof(sof_pwm, sof_dma);
                log::info!(
                    "PIO USB host initialized (PIO0, D+=GPIO12, D-=GPIO13, hardware SOF {})",
                    hw_sof
                );

                spawner.spawn(usb_host::idle_task(host_bus).unwrap());
                spawner.spawn(usb_host::host_task(host_bus).unwrap());
            })
        },
    );

    let mut counter: u32 = 0;

    loop {
        Timer::after_secs(10).await;

        log::info!("rp-wheel-bridge alive: {}", counter);

        counter = counter.wrapping_add(1);
    }
}
