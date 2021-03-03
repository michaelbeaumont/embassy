use core::ops::Deref;
use defmt::{assert, *};

use crate::pac;

const XOSC_MHZ: u32 = 12;

pub struct PLL<T: Instance> {
    inner: T,
}

impl<T: Instance> PLL<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    pub fn configure(&mut self, refdiv: u32, vco_freq: u32, post_div1: u8, post_div2: u8) {
        unsafe {
            let p = self.inner.regs();
            // Power off in case it's already running
            p.pwr().reset();
            p.fbdiv_int().reset();

            let ref_mhz = XOSC_MHZ / refdiv;
            p.cs().write(|w| unsafe { w.set_refdiv(ref_mhz as _) });

            let fbdiv = vco_freq / (ref_mhz * 1_000_000);
            assert!(fbdiv >= 16 && fbdiv <= 520);
            assert!((post_div1 >= 1 && post_div1 <= 7) && (post_div2 >= 1 && post_div2 <= 7));
            assert!(post_div2 <= post_div1);
            assert!(ref_mhz <= (vco_freq / 16));

            p.fbdiv_int().write(|w| w.set_fbdiv_int(fbdiv as _));

            p.pwr().modify(|w| {
                w.set_pd(false);
                w.set_vcopd(false);
            });

            while !p.cs().read().lock() {}

            p.prim().write(|w| {
                w.set_postdiv1(post_div1);
                w.set_postdiv2(post_div2);
            });

            p.pwr().modify(|w| w.set_postdivpd(false));
        }
    }
}

mod sealed {
    use crate::pac;

    pub trait Instance {}
    impl Instance for super::PllSys {}
    impl Instance for super::PllUsb {}
}

// todo make owned
pub struct PllSys;
pub struct PllUsb;

pub trait Instance {
    fn regs(&self) -> pac::pll_sys::PllSys;
}
impl Instance for PllSys {
    fn regs(&self) -> pac::pll_sys::PllSys {
        pac::PLL_SYS
    }
}
impl Instance for PllUsb {
    fn regs(&self) -> pac::pll_sys::PllSys {
        pac::PLL_USB
    }
}
