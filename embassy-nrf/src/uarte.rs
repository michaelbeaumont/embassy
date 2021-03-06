//! Async low power UARTE.
//!
//! The peripheral is automatically enabled and disabled as required to save power.
//! Lowest power consumption can only be guaranteed if the send receive futures
//! are dropped correctly (e.g. not using `mem::forget()`).

use core::future::Future;
use core::ops::Deref;
use core::sync::atomic::{compiler_fence, Ordering};
use core::task::{Context, Poll};

use embassy::interrupt::InterruptExt;
use embassy::util::Signal;

use crate::fmt::{assert, *};
use crate::hal::pac;
use crate::hal::prelude::*;
use crate::hal::target_constants::EASY_DMA_SIZE;
use crate::interrupt::Interrupt;
use crate::{interrupt, util};

pub use crate::hal::uarte::Pins;
// Re-export SVD variants to allow user to directly set values.
pub use pac::uarte0::{baudrate::BAUDRATE_A as Baudrate, config::PARITY_A as Parity};

/// Interface to the UARTE peripheral
pub struct Uarte<T>
where
    T: Instance,
{
    instance: T,
    irq: T::Interrupt,
    pins: Pins,
}

pub struct State {
    tx_done: Signal<()>,
    rx_done: Signal<u32>,
}

impl<T> Uarte<T>
where
    T: Instance,
{
    /// Creates the interface to a UARTE instance.
    /// Sets the baud rate, parity and assigns the pins to the UARTE peripheral.
    ///
    /// # Unsafe
    ///
    /// The returned API is safe unless you use `mem::forget` (or similar safe mechanisms)
    /// on stack allocated buffers which which have been passed to [`send()`](Uarte::send)
    /// or [`receive`](Uarte::receive).
    #[allow(unused_unsafe)]
    pub unsafe fn new(
        uarte: T,
        irq: T::Interrupt,
        mut pins: Pins,
        parity: Parity,
        baudrate: Baudrate,
    ) -> Self {
        assert!(uarte.enable.read().enable().is_disabled());

        uarte.psel.rxd.write(|w| {
            unsafe { w.bits(pins.rxd.psel_bits()) };
            w.connect().connected()
        });

        pins.txd.set_high().unwrap();
        uarte.psel.txd.write(|w| {
            unsafe { w.bits(pins.txd.psel_bits()) };
            w.connect().connected()
        });

        // Optional pins
        uarte.psel.cts.write(|w| {
            if let Some(ref pin) = pins.cts {
                unsafe { w.bits(pin.psel_bits()) };
                w.connect().connected()
            } else {
                w.connect().disconnected()
            }
        });

        uarte.psel.rts.write(|w| {
            if let Some(ref pin) = pins.rts {
                unsafe { w.bits(pin.psel_bits()) };
                w.connect().connected()
            } else {
                w.connect().disconnected()
            }
        });

        uarte.baudrate.write(|w| w.baudrate().variant(baudrate));
        uarte.config.write(|w| w.parity().variant(parity));

        // Enable interrupts
        uarte.events_endtx.reset();
        uarte.events_endrx.reset();
        uarte
            .intenset
            .write(|w| w.endtx().set().txstopped().set().endrx().set().rxto().set());

        // Register ISR
        irq.set_handler(Self::on_irq);
        irq.unpend();
        irq.enable();

        Uarte {
            instance: uarte,
            irq,
            pins,
        }
    }

    pub fn free(self) -> (T, T::Interrupt, Pins) {
        // Wait for the peripheral to be disabled from the ISR.
        while self.instance.enable.read().enable().is_enabled() {}
        (self.instance, self.irq, self.pins)
    }

    fn enable(&mut self) {
        trace!("enable");
        self.instance.enable.write(|w| w.enable().enabled());
    }

    fn tx_started(&self) -> bool {
        self.instance.events_txstarted.read().bits() != 0
    }

    fn rx_started(&self) -> bool {
        self.instance.events_rxstarted.read().bits() != 0
    }

    unsafe fn on_irq(_ctx: *mut ()) {
        let uarte = &*pac::UARTE0::ptr();

        let mut try_disable = false;

        if uarte.events_endtx.read().bits() != 0 {
            uarte.events_endtx.reset();
            trace!("endtx");
            compiler_fence(Ordering::SeqCst);

            if uarte.events_txstarted.read().bits() != 0 {
                // The ENDTX was signal triggered because DMA has finished.
                uarte.events_txstarted.reset();
                try_disable = true;
            }

            T::state().tx_done.signal(());
        }

        if uarte.events_txstopped.read().bits() != 0 {
            uarte.events_txstopped.reset();
            trace!("txstopped");
            try_disable = true;
        }

        if uarte.events_endrx.read().bits() != 0 {
            uarte.events_endrx.reset();
            trace!("endrx");
            let len = uarte.rxd.amount.read().bits();
            compiler_fence(Ordering::SeqCst);

            if uarte.events_rxstarted.read().bits() != 0 {
                // The ENDRX was signal triggered because DMA buffer is full.
                uarte.events_rxstarted.reset();
                try_disable = true;
            }

            T::state().rx_done.signal(len);
        }

        if uarte.events_rxto.read().bits() != 0 {
            uarte.events_rxto.reset();
            trace!("rxto");
            try_disable = true;
        }

        // Disable the peripheral if not active.
        if try_disable
            && uarte.events_txstarted.read().bits() == 0
            && uarte.events_rxstarted.read().bits() == 0
        {
            trace!("disable");
            uarte.enable.write(|w| w.enable().disabled());
        }
    }
}

impl<T: Instance> embassy::traits::uart::Uart for Uarte<T> {
    type ReceiveFuture<'a> = ReceiveFuture<'a, T>;
    type SendFuture<'a> = SendFuture<'a, T>;

    /// Sends serial data.
    ///
    /// `tx_buffer` is marked as static as per `embedded-dma` requirements.
    /// It it safe to use a buffer with a non static lifetime if memory is not
    /// reused until the future has finished.
    fn send<'a>(&'a mut self, tx_buffer: &'a [u8]) -> SendFuture<'a, T> {
        // Panic if TX is running which can happen if the user has called
        // `mem::forget()` on a previous future after polling it once.
        assert!(!self.tx_started());

        T::state().tx_done.reset();

        SendFuture {
            uarte: self,
            buf: tx_buffer,
        }
    }

    /// Receives serial data.
    ///
    /// The future is pending until the buffer is completely filled.
    /// A common pattern is to use [`stop()`](ReceiveFuture::stop) to cancel
    /// unfinished transfers after a timeout to prevent lockup when no more data
    /// is incoming.
    ///
    /// `rx_buffer` is marked as static as per `embedded-dma` requirements.
    /// It it safe to use a buffer with a non static lifetime if memory is not
    /// reused until the future has finished.
    fn receive<'a>(&'a mut self, rx_buffer: &'a mut [u8]) -> ReceiveFuture<'a, T> {
        // Panic if RX is running which can happen if the user has called
        // `mem::forget()` on a previous future after polling it once.
        assert!(!self.rx_started());

        T::state().rx_done.reset();

        ReceiveFuture {
            uarte: self,
            buf: rx_buffer,
        }
    }
}

/// Future for the [`Uarte::send()`] method.
pub struct SendFuture<'a, T>
where
    T: Instance,
{
    uarte: &'a mut Uarte<T>,
    buf: &'a [u8],
}

impl<'a, T> Drop for SendFuture<'a, T>
where
    T: Instance,
{
    fn drop(self: &mut Self) {
        if self.uarte.tx_started() {
            trace!("stoptx");

            // Stop the transmitter to minimize the current consumption.
            self.uarte.instance.events_txstarted.reset();
            self.uarte
                .instance
                .tasks_stoptx
                .write(|w| unsafe { w.bits(1) });

            // TX is stopped almost instantly, spinning is fine.
            while !T::state().tx_done.signaled() {}
        }
    }
}

impl<'a, T> Future for SendFuture<'a, T>
where
    T: Instance,
{
    type Output = Result<(), embassy::traits::uart::Error>;

    fn poll(self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self { uarte, buf } = unsafe { self.get_unchecked_mut() };

        if T::state().tx_done.poll_wait(cx).is_pending() {
            let ptr = buf.as_ptr();
            let len = buf.len();
            assert!(len <= EASY_DMA_SIZE);
            // TODO: panic if buffer is not in SRAM

            uarte.enable();

            compiler_fence(Ordering::SeqCst);
            uarte
                .instance
                .txd
                .ptr
                .write(|w| unsafe { w.ptr().bits(ptr as u32) });
            uarte
                .instance
                .txd
                .maxcnt
                .write(|w| unsafe { w.maxcnt().bits(len as _) });

            trace!("starttx");
            uarte.instance.tasks_starttx.write(|w| unsafe { w.bits(1) });
            while !uarte.tx_started() {} // Make sure transmission has started

            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

/// Future for the [`Uarte::receive()`] method.
pub struct ReceiveFuture<'a, T>
where
    T: Instance,
{
    uarte: &'a mut Uarte<T>,
    buf: &'a mut [u8],
}

impl<'a, T> Drop for ReceiveFuture<'a, T>
where
    T: Instance,
{
    fn drop(self: &mut Self) {
        if self.uarte.rx_started() {
            trace!("stoprx (drop)");

            self.uarte.instance.events_rxstarted.reset();
            self.uarte
                .instance
                .tasks_stoprx
                .write(|w| unsafe { w.bits(1) });

            util::low_power_wait_until(|| T::state().rx_done.signaled())
        }
    }
}

impl<'a, T> Future for ReceiveFuture<'a, T>
where
    T: Instance,
{
    type Output = Result<(), embassy::traits::uart::Error>;

    fn poll(self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self { uarte, buf } = unsafe { self.get_unchecked_mut() };

        match T::state().rx_done.poll_wait(cx) {
            Poll::Pending if !uarte.rx_started() => {
                let ptr = buf.as_ptr();
                let len = buf.len();
                assert!(len <= EASY_DMA_SIZE);

                uarte.enable();

                compiler_fence(Ordering::SeqCst);
                uarte
                    .instance
                    .rxd
                    .ptr
                    .write(|w| unsafe { w.ptr().bits(ptr as u32) });
                uarte
                    .instance
                    .rxd
                    .maxcnt
                    .write(|w| unsafe { w.maxcnt().bits(len as _) });

                trace!("startrx");
                uarte.instance.tasks_startrx.write(|w| unsafe { w.bits(1) });
                while !uarte.rx_started() {} // Make sure reception has started

                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(Ok(())),
        }
    }
}

/// Future for the [`receive()`] method.
impl<'a, T> ReceiveFuture<'a, T>
where
    T: Instance,
{
    /// Stops the ongoing reception and returns the number of bytes received.
    pub async fn stop(self) -> usize {
        let len = if self.uarte.rx_started() {
            trace!("stoprx (stop)");

            self.uarte.instance.events_rxstarted.reset();
            self.uarte
                .instance
                .tasks_stoprx
                .write(|w| unsafe { w.bits(1) });
            T::state().rx_done.wait().await
        } else {
            // Transfer was stopped before it even started. No bytes were sent.
            0
        };
        len as _
    }
}

mod private {
    pub trait Sealed {}
}

pub trait Instance:
    Deref<Target = pac::uarte0::RegisterBlock> + Sized + private::Sealed + 'static
{
    type Interrupt: Interrupt;

    #[doc(hidden)]
    fn state() -> &'static State;
}

static UARTE0_STATE: State = State {
    tx_done: Signal::new(),
    rx_done: Signal::new(),
};
impl private::Sealed for pac::UARTE0 {}
impl Instance for pac::UARTE0 {
    type Interrupt = interrupt::UARTE0_UART0;

    fn state() -> &'static State {
        &UARTE0_STATE
    }
}

#[cfg(any(feature = "52833", feature = "52840", feature = "9160"))]
static UARTE1_STATE: State = State {
    tx_done: Signal::new(),
    rx_done: Signal::new(),
};
#[cfg(any(feature = "52833", feature = "52840", feature = "9160"))]
impl private::Sealed for pac::UARTE1 {}
#[cfg(any(feature = "52833", feature = "52840", feature = "9160"))]
impl Instance for pac::UARTE1 {
    type Interrupt = interrupt::UARTE1;

    fn state() -> &'static State {
        &UARTE1_STATE
    }
}
