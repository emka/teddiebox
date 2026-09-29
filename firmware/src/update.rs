//! Asks the server named by the card's `update_url` whether a different
//! image exists, and installs it.
//!
//! The decisions are `teddiebox_ota`'s, tested on the host. This supplies the
//! socket, the flash and the reboot. The image goes into the slot that is not
//! running, and `otadata` is switched only after [`teddiebox_ota::verify`]
//! passes, so a failure at any earlier point leaves the box running as it
//! was.
//!
//! The server is always the host in `update_url`, never one a manifest
//! names: see the trust model in `teddiebox_ota`.

use core::sync::atomic::Ordering;

use embassy_net::Stack;
use embassy_time::Instant;
use mbedtls_rs::sys::{
    mbedtls_sha256_context, mbedtls_sha256_finish, mbedtls_sha256_free, mbedtls_sha256_init,
    mbedtls_sha256_starts, mbedtls_sha256_update,
};
use teddiebox_core::LedState;
use teddiebox_ota::{Decision, ImageWriter, Manifest, MAX_MANIFEST};

use crate::ota::{SlotWriter, SpareSlot};
use crate::tls;

/// Bytes per flash write.
///
/// A multiple of four, as `write_nor` requires. Small, because the staging
/// buffer lives on the stack while the download runs.
const STAGE: usize = 512;

/// How much of the image's start [`teddiebox_ota::verify`] needs: the ESP-IDF
/// application descriptor ends at offset 0x50.
const HEAD: usize = 0x50;

/// Checks for an update and, if there is one, installs it and reboots.
///
/// Returns only when nothing was installed: up to date, no server, a
/// malformed manifest, or a download that failed, did not verify, or was
/// stopped because a story started or a figure needs the network. Each case
/// prints why.
pub async fn check(client: &tls::Client, stack: &Stack<'_>, update_url: &str) {
    let url = match teddiebox_ota::split(update_url) {
        Ok(url) => url,
        Err(trouble) => {
            esp_println::println!("teddiebox: update url unusable — {trouble:?}");
            return;
        }
    };
    let Some(slot) = crate::ota::spare_slot() else {
        return;
    };
    let Some(manifest) = fetch_manifest(client, stack, &url.host, &url.path).await else {
        return;
    };

    let ours = env!("TEDDIEBOX_VERSION");
    match teddiebox_ota::decide(&manifest, ours, slot.len) {
        Decision::UpToDate => {
            esp_println::println!("teddiebox: update none — {ours} is current");
            return;
        }
        Decision::Refuse(why) => {
            esp_println::println!("teddiebox: update refused — {why:?}");
            return;
        }
        Decision::Update { .. } => {}
    }

    let path = match teddiebox_ota::resolve_image(&url.path, &manifest.image) {
        Ok(path) => path,
        Err(trouble) => {
            esp_println::println!("teddiebox: update image path unusable — {trouble:?}");
            return;
        }
    };
    esp_println::println!(
        "teddiebox: update {ours} -> {}, {} bytes from {path}",
        manifest.version,
        manifest.length
    );

    let shown = crate::LED_REQUEST.swap(LedState::Fetching.code(), Ordering::Relaxed);
    let installed = install(client, stack, &url.host, &path, &manifest, &slot).await;
    crate::LED_REQUEST.store(shown, Ordering::Relaxed);
    if installed && crate::ota::select_next_boot(slot.target) {
        esp_println::println!(
            "teddiebox: update installed — rebooting into {}",
            manifest.version
        );
        crate::drain_console();
        esp_hal::system::software_reset();
    }
}

/// Fetches and parses the manifest, printing why if it could not.
async fn fetch_manifest(
    client: &tls::Client,
    stack: &Stack<'_>,
    host: &str,
    path: &str,
) -> Option<Manifest> {
    let mut raw = teddiebox_core::heapless::Vec::<u8, MAX_MANIFEST>::new();
    // A body longer than the buffer stops at a full buffer, which
    // `parse_read` refuses as cut short.
    let fetched = tls::get_path(client, stack, host, path, &mut |bytes| {
        let room = raw.capacity() - raw.len();
        let _ = raw.extend_from_slice(&bytes[..bytes.len().min(room)]);
        raw.len() < raw.capacity()
    })
    .await;
    match fetched {
        Ok(_) | Err(tls::Error::Abandoned) => {}
        Err(tls::Error::NoContent) => {
            esp_println::println!("teddiebox: update no manifest at {host}{path}");
            return None;
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: update could not fetch the manifest — {trouble:?}");
            return None;
        }
    }
    match Manifest::parse_read(&raw, MAX_MANIFEST) {
        Ok(manifest) => Some(manifest),
        Err(trouble) => {
            esp_println::println!("teddiebox: update manifest malformed — {trouble:?}");
            None
        }
    }
}

/// Streams the image into `slot`, hashing it on the way, and says whether it
/// is the image the manifest promised.
///
/// Stops early, and returns `false`, when a story starts or another network
/// request is queued: erasing flash blocks interrupts, which would make a
/// story stutter, and a figure waiting on the network must not wait for this.
async fn install(
    client: &tls::Client,
    stack: &Stack<'_>,
    host: &str,
    path: &str,
    manifest: &Manifest,
    slot: &SpareSlot,
) -> bool {
    let started = Instant::now();
    let mut writer = ImageWriter::<STAGE>::new(slot.len);
    let mut digest = Sha256::new();
    let mut head = [0u8; HEAD];
    let mut head_len = 0;
    let mut refused = None;

    let fetched = tls::get_path(client, stack, host, path, &mut |bytes| {
        if crate::PLAYING.load(Ordering::Relaxed)
            || crate::NET_REQUEST.load(Ordering::Relaxed) != crate::REQUEST_NONE
        {
            return false;
        }
        digest.update(bytes);
        let take = (HEAD - head_len).min(bytes.len());
        head[head_len..head_len + take].copy_from_slice(&bytes[..take]);
        head_len += take;
        // Borrowed per chunk: the handle must never be held across an
        // `.await`, and the download awaits between chunks.
        let mut flash = crate::flash::flash();
        let mut region = SlotWriter {
            flash: &mut flash,
            offset: slot.offset,
        };
        match writer.push(&mut region, bytes) {
            Ok(()) => true,
            Err(trouble) => {
                refused = Some(trouble);
                false
            }
        }
    })
    .await;

    if let Some(trouble) = refused {
        esp_println::println!("teddiebox: update write failed — {trouble:?}");
        return false;
    }
    match fetched {
        Ok(_) => {}
        Err(tls::Error::Abandoned) => {
            esp_println::println!("teddiebox: update stopped — the box is needed for a story");
            return false;
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: update download failed — {trouble:?}");
            return false;
        }
    }

    let length = {
        let mut flash = crate::flash::flash();
        let mut region = SlotWriter {
            flash: &mut flash,
            offset: slot.offset,
        };
        match writer.finish(&mut region) {
            Ok(length) => length,
            Err(trouble) => {
                esp_println::println!("teddiebox: update write failed — {trouble:?}");
                return false;
            }
        }
    };
    let digest = digest.finish();
    esp_println::println!(
        "teddiebox: update {length} bytes in {} ms",
        started.elapsed().as_millis()
    );
    match teddiebox_ota::verify(manifest, length, &digest, &head[..head_len]) {
        Ok(()) => true,
        Err(trouble) => {
            esp_println::println!("teddiebox: update does not verify — {trouble:?}");
            false
        }
    }
}

/// A running SHA-256, from the mbedtls the box already carries for TLS.
struct Sha256(mbedtls_sha256_context);

impl Sha256 {
    fn new() -> Self {
        let mut context = mbedtls_sha256_context::default();
        // SAFETY: `context` is a valid, exclusively borrowed context; `init`
        // and `starts` only write to it. `starts` cannot fail for SHA-256
        // (`is224 = 0`) with SHA-256 compiled in.
        unsafe {
            mbedtls_sha256_init(&mut context);
            mbedtls_sha256_starts(&mut context, 0);
        }
        Self(context)
    }

    fn update(&mut self, bytes: &[u8]) {
        // SAFETY: the context was started in `new`, and `bytes` is a valid
        // slice for its length.
        unsafe {
            mbedtls_sha256_update(&mut self.0, bytes.as_ptr(), bytes.len());
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: the context was started in `new`, and `out` has the 32
        // bytes SHA-256 writes.
        unsafe {
            mbedtls_sha256_finish(&mut self.0, out.as_mut_ptr());
        }
        out
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        // SAFETY: the context was initialised in `new` and is freed once.
        unsafe { mbedtls_sha256_free(&mut self.0) }
    }
}
