#![no_std]

use gstd::debug;

#[no_mangle]
pub unsafe extern "C" fn handle() {
    debug!("Starting panicker handle");
    panic!("I just panic every time")
}
