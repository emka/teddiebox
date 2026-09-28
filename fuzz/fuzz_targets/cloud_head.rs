//! A response head from teddyCloud, or from something pretending to be it.
//! The body is written to the card from where the head says it starts.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_cloud::parse_head;

fuzz_target!(|data: &[u8]| {
    if let Ok((_, body_at)) = parse_head(data) {
        assert!(body_at <= data.len(), "body at {body_at} of {}", data.len());
        assert!(data[..body_at].ends_with(b"\r\n\r\n"), "body at {body_at}");
    }
});
