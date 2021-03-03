#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

#[path = "../example_common.rs"]
mod example_common;
use example_common::*;

use cortex_m_rt::entry;
use defmt::panic;

use embassy::executor::{task, Executor};
use embassy::io::{AsyncBufReadExt, AsyncWriteExt};
use embassy::util::Forever;

#[entry]
fn main() -> ! {
    info!("Hello World!");
    loop {
        cortex_m::asm::nop();
    }
}
