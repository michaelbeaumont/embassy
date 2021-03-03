#![no_std]
#![no_main]
#![feature(asm)]
#![feature(type_alias_impl_trait)]

use core::sync::atomic::{AtomicUsize, Ordering};
use cortex_m_rt::entry;
use defmt::*;
use defmt_rtt as _;
use embassy::executor::{task, Executor};
use embassy::util::Forever;
use panic_probe as _;
use rp2040_pac2 as pac;

mod pll;
mod resets;

#[link_section = ".boot2"]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER;

#[defmt::timestamp]
fn timestamp() -> u64 {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    // NOTE(no-CAS) `timestamps` runs with interrupts disabled
    let n = COUNT.load(Ordering::Relaxed);
    COUNT.store(n + 1, Ordering::Relaxed);
    n as u64
}

unsafe fn init() {
    // Now reset all the peripherals, except QSPI and XIP (we're using those
    // to execute from external flash!)

    let resets = resets::Resets::new();

    // Reset everything except:
    // - QSPI (we're using it to run this code!)
    // - PLLs (it may be suicide if that's what's clocking us)
    let mut peris = resets::ALL_PERIPHERALS;
    peris.set_io_qspi(false);
    peris.set_pads_qspi(false);
    peris.set_pll_sys(false);
    peris.set_pll_usb(false);
    resets.reset(peris);

    let mut peris = resets::ALL_PERIPHERALS;
    peris.set_adc(false);
    peris.set_rtc(false);
    peris.set_spi0(false);
    peris.set_spi1(false);
    peris.set_uart0(false);
    peris.set_uart1(false);
    peris.set_usbctrl(false);
    resets.unreset_wait(peris);

    // xosc 12 mhz
    pac::WATCHDOG.tick().write(|w| {
        w.set_cycles(XOSC_MHZ as u16);
        w.set_enable(true);
    });

    pac::CLOCKS
        .clk_sys_resus_ctrl()
        .write_value(pac::clocks::regs::ClkSysResusCtrl::from_bits(0));

    // Enable XOSC
    // TODO extract to HAL module
    const XOSC_MHZ: u32 = 12;
    pac::XOSC
        .ctrl()
        .write(|w| w.set_freq_range(pac::xosc::vals::CtrlFreqRange::_1_15MHZ));

    let startup_delay = (((XOSC_MHZ * 1_000_000) / 1000) + 128) / 256;
    pac::XOSC
        .startup()
        .write(|w| w.set_delay(startup_delay as u16));
    pac::XOSC.ctrl().write(|w| {
        w.set_freq_range(pac::xosc::vals::CtrlFreqRange::_1_15MHZ);
        w.set_enable(pac::xosc::vals::CtrlEnable::ENABLE);
    });
    while !pac::XOSC.status().read().stable() {}

    // Before we touch PLLs, switch sys and ref cleanly away from their aux sources.
    pac::CLOCKS
        .clk_sys_ctrl()
        .modify(|w| w.set_src(pac::clocks::vals::ClkSysCtrlSrc::CLK_REF));
    while pac::CLOCKS.clk_sys_selected().read() != 1 {}
    pac::CLOCKS
        .clk_ref_ctrl()
        .modify(|w| w.set_src(pac::clocks::vals::ClkRefCtrlSrc::ROSC_CLKSRC_PH));
    while pac::CLOCKS.clk_ref_selected().read() != 1 {}

    let mut peris = resets::Peripherals::from_bits(0);
    peris.set_pll_sys(true);
    peris.set_pll_usb(true);
    resets.reset(peris);
    resets.unreset_wait(peris);

    pll::PLL::new(pll::PllSys).configure(1, 1500_000_000, 6, 2);
    pll::PLL::new(pll::PllUsb).configure(1, 480_000_000, 5, 2);
}

#[task]
async fn run() {
    unsafe {
        loop {
            info!("task on!");
            pac::IO_BANK0.gpio25_ctrl().write(|w| {
                w.set_oeover(pac::io::vals::Oeover::ENABLE);
                w.set_outover(pac::io::vals::Outover::HIGH);
            });

            cortex_m::asm::delay(1_000_000);

            info!("task off!");
            pac::IO_BANK0.gpio25_ctrl().write(|w| {
                w.set_oeover(pac::io::vals::Oeover::ENABLE);
                w.set_outover(pac::io::vals::Outover::LOW);
            });

            cortex_m::asm::delay(1_000_000);
        }
    }
}

static EXECUTOR: Forever<Executor> = Forever::new();

#[entry]
fn main() -> ! {
    info!("Hello World!");

    unsafe { init() };

    let executor = EXECUTOR.put(Executor::new());
    executor.run(|spawner| {
        unwrap!(spawner.spawn(run()));
    });
}
