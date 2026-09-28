//! A form body posted to the setup portal. Only a config that parses may be
//! written to the card.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_portal::form;
use teddiebox_portal::submission::{examine, Submission};

fuzz_target!(|data: &[u8]| {
    let _: Result<teddiebox_core::heapless::Vec<u8, 64>, _> = form::field(data, "config");
    if let Submission::Write(bytes) = examine(data) {
        let text = core::str::from_utf8(&bytes).expect("wrote bytes that are not text");
        assert!(
            teddiebox_config::Config::parse(text).is_ok(),
            "wrote a config that does not parse"
        );
    }
});
