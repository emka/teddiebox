//! The head of a downloaded image, whose descriptor names its version.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_ota::image_version;

fuzz_target!(|data: &[u8]| {
    if let Ok(version) = image_version(data) {
        assert!(!version.contains('\0'), "version {version:?}");
    }
});
