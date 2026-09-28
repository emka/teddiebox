//! A request from any device on the setup portal's open access point.
//!
//! `is_complete` decides when the portal stops reading, so it must agree
//! with what `parse` makes of the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_portal::{http, MAX_BODY};

fuzz_target!(|data: &[u8]| {
    let complete = http::is_complete(data);
    match http::parse(data) {
        Ok(request) => {
            assert!(request.header_len <= data.len());
            assert!(data[..request.header_len].ends_with(b"\r\n\r\n"));
            assert!(request.content_length <= MAX_BODY);
            let arrived = data.len() - request.header_len;
            assert_eq!(complete, Ok(arrived >= request.content_length));
        }
        Err(http::RequestError::Incomplete) => assert_eq!(complete, Ok(false)),
        Err(refused) => assert_eq!(complete, Err(refused)),
    }
});
