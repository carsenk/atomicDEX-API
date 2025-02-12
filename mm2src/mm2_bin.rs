#![feature(non_ascii_idents)]
#![feature(drain_filter)]
#![feature(async_await)]

#[macro_use] extern crate common;
#[macro_use] extern crate fomat_macros;
#[macro_use] extern crate gstuff;
//#[macro_use] extern crate lazy_static;
#[macro_use] extern crate serde_json;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate serialization_derive;
#[macro_use] extern crate unwrap;

#[path = "mm2.rs"]
mod mm2;

fn main() {
    #[cfg(feature = "native")] {
        mm2::mm2_main()
    }
}
