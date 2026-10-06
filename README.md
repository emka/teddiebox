# teddiebox

Replacement firmware for the Toniebox with the ESP32 rev 1.6.C
board, written in Rust. It reads the same figures, and fetches audio from a local
[teddyCloud](https://github.com/toniebox-reverse-engineering/teddycloud).

> **Unofficial project.** Not affiliated with or endorsed by tonies or the
> maker of the Toniebox. "Toniebox" and "tonies" are their trademarks, used
> only to say what this firmware is compatible with.
> Flashing replaces the stock firmware, can brick the box and may void the
> warranty. Use is at your own risk. The firmware drives battery charging and
> the volume path of a children's product, so you are responsible for safe use.

## Motivation

teddyCloud replaces the manufacturer's cloud, but the box still runs closed
firmware that cannot be improved. This firmware is open, and
can be extended. It behaves like stock except where noted below.

## Features different to stock

### Configuration file

The firmware is configured by writing `config.txt` on the SD card.
Editing this file is also possible by booting the box in setup mode (see its own section below).
All options are listed under [Configuration](#configuration).

### Position per story

Stock firmware remembers the position of the last figure played, also across standby.
Playing a different figure in between resets it.

teddiebox remembers the position of every story, also across power-off.

- The position is written to the SD card when the figure's memory is needed for
  another figure, and at shutdown.
- A story played to its end starts from the beginning next time.
- The position is stored in `<story>.pos` next to the story on the SD card.
  Delete it to start that story from the beginning.
- A flat battery or a reset loses the position since the last write. Writing
  more often would wear the SD card.

### Chapter skip

- **Hold an ear** for about half a second: the larger (right) ear skips
  forward, the smaller one back.
- **Slap the side** of the box: right side forward, left side back.

Because a press could be a hold, volume changes when the ear is released rather
than when it is pressed. Set `ears_skip = no` in `config.txt` to get stock ear
behaviour: volume changes on press, and slapping still skips.

### Trusted CA on the SD card

Stock keeps the CA it trusts in flash, in the `assets` partition as
`CERT/CA.DER`. It is the manufacturer's CA. Pointing a stock box at teddyCloud
means patching that file in a flash dump and writing the partition back.

teddiebox reads the CA from the SD card instead.

- The file is `cert/tcca.der`, the teddyCloud CA. See
  [Requirements](#requirements).
- To change the CA, replace the file, or upload it in setup mode. No reflash.
- The `assets` partition is left as stock. teddiebox does not read the
  manufacturer's CA from it.
- Without the file the box plays what is on the SD card but cannot fetch, and
  says so at boot.
- Anyone who can write to the SD card can change which CA the box trusts. Stock
  needs access to the flash for that.

### Setup mode

The box can be configured over WiFi without removing the SD card. See
[Changing settings without an SD card reader](#changing-settings-without-an-sd-card-reader).

### Status light

teddiebox uses the light as follows:

| colour | meaning |
|---|---|
| green | idle or playing |
| blue | waiting on the server: fetching a story, or checking that the cached one is current |
| orange | battery low |
| red | fault, or battery about to run out |
| cyan | idle and charging |
| magenta | setup page running |
| off | standby |

The light is steady and dim.

## Missing compared to stock

- **Rewind and fast-forward by tilting.** Tilting does nothing. Only whole
  chapters can be skipped.
- **Telemetry.** Stock reports events to teddyCloud: figure placed or lifted,
  ear presses, slaps, tilts, playback start and stop, charger in or out.
  teddyCloud shows them as the box's live state and publishes them over MQTT.
  teddiebox sends none of these.
- **Settings from teddyCloud.** Stock gets its volume limit, slap setting and
  similar settings from the server. teddiebox reads settings only from
  `config.txt`.
- **Updates over the air.** See [Updates over the air](#updates-over-the-air).

## Configuration

Settings are in `config.txt` in the SD card's root. Example:

    ssid = home
    password = correct horse battery
    server = teddycloud.local:443
    ears_skip = yes
    # update_url = https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt
    # setup_password = my own passphrase

Format:

- One `key = value` per line. `#` starts a comment. Blank lines are ignored.
- Unknown keys are ignored, so newer cards work with older firmware.
- A known key with an invalid value is reported as an error, not guessed at.
- `password` and `setup_password` take everything after `=`, including `#`.
  For the other keys, a `#` that starts a word begins a comment.

Options:

| key | default | |
|---|---|---|
| `ssid` | required | WiFi network, up to 32 characters |
| `password` | empty | WiFi passphrase, up to 63 characters. Empty for an open network |
| `server` | required | `host:port` of your teddyCloud, up to 64 characters. Only letters, digits, `.`, `-`, `_` and `:` |
| `ears_skip` | `yes` | holding an ear skips a chapter. `yes`/`no`, `true`/`false` or `1`/`0`. `no` gives stock ear behaviour, see [Chapter skip](#chapter-skip) |
| `update_url` | none | `https://` URL of an update manifest, up to 128 characters. Without it the box never checks for updates, see [Updates over the air](#updates-over-the-air). An empty value is an error |
| `setup_password` | `teddiebox` | passphrase of the setup network, 8 to 63 characters, see [Setup mode](#changing-settings-without-an-sd-card-reader) |

A file without `ssid` or `server` is rejected.

**Trust `server`.** The box verifies the server's certificate against
`cert/tcca.der` on the SD card, and sends it its own certificate and the placed
figure's token.

**`update_url` is verified like `server`**, so its host must be teddyCloud
itself. See [Updates over the air](#updates-over-the-air).

**The WiFi key is stored in flash.** The box derives a key from `ssid` and
`password` and keeps it in the `wifi` partition, which cuts joining from about
2s to 0.1s. It is re-derived after changing either value. Anyone who can
read the flash can join your network, but the passphrase is on the SD card in
plain text anyway.

## Installation

### Requirements

- A Toniebox with the `TONIEBOX-ESP32` rev 1.6.C board. See `HARDWARE.md` for
  the board, the wiring and download mode.
- A USB serial adapter connected to the box's console.
- A [teddyCloud](https://github.com/toniebox-reverse-engineering/teddycloud)
  server on your network.
- A full flash dump of the box before it is flashed, kept safe and read-only.
  [Follow the instructions](https://tonies-wiki.revvox.de/docs/tools/teddycloud/setup/dump-certs/esp32/)
  to make one. `just flash` writes only the application and relies on the
  stock bootloader being on the box, and you need the dump to restore the stock
  firmware. Set `TEDDIEBOX_STOCK_DUMP` in `.envrc.local` to its path.
- The teddyCloud CA at `cert/tcca.der` on the SD card. Copy it there, or
  upload it in setup mode (below). teddyCloud serves it at
  `https://<host>:8443/api/getFile/ca.der`.

### Flashing

    just flash

This puts the box into download mode, flashes it and restarts it. Use it rather
than calling the tools directly: the order in `scripts/flash.sh` matters, and
getting it wrong means opening the case to recover.

For a box in daily use, build the release image:

    TEDDIEBOX_RELEASE=1 just flash

The release image drops the debug console commands. It answers only `dl`
(reboot for flashing). Playback and controls are the
same.

The flash layout is the stock one, so a dump of a stock box and
`partitions.csv` agree. The firmware goes in `ota_0`; updates alternate with
`ota_1`; `ota_2` keeps a stock image and is never written.

### The box's identity

The box authenticates to teddyCloud with the certificate and key stock put in
the `assets` flash partition (`CERT/client.der` and `CERT/private.der`). The
firmware only reads them: neither `just flash` nor an update writes `assets`.

If `assets` was overwritten, the box plays what is on the SD card but cannot
fetch, and says so at boot. Put it back from the dump with `BIN_FILE=<file>
BIN_ADDR=0xf000 ./scripts/flash.sh`, where the file is the dump's bytes from
`0xf000` to `0x16f000`.

### Changing settings without an SD card reader

1. Hold both ears while switching the box on, until the light turns on. The box
   starts a WiFi network instead of playing. A bench image also enters setup
   mode on the console command `setup`.
2. Join `teddiebox-setup` with passphrase `teddiebox`.
3. Open <http://192.168.4.1/>. It shows `config.txt` for editing.
4. Press **Write config.txt**. The file is checked first; errors are shown on
   the page and nothing is written.
5. Press **Restart** to leave setup mode with the new settings.

To put teddyCloud's CA on the SD card, choose `ca.der` under **certificate** and
press **Write certificate**. The page then shows its size. It is used from the
next restart.

Setup mode also ends by itself after ten minutes.

Security:

- The page shows your WiFi passphrase in plain text.
- Anyone on the setup network can replace the CA the box trusts, as they can
  replace `server`.
- The default setup passphrase is public. Anyone in range who knows it and
  records your device joining can decrypt the session, including your WiFi
  passphrase. The page cannot use HTTPS: the box has no clock and the phone
  has no reason to trust its certificate. Use setup mode only where that risk
  is acceptable.

Set `setup_password` in `config.txt` to use your own passphrase. A box with no
SD card, an unreadable SD card or an unparseable `config.txt` still uses the
default.

To reset a forgotten setup passphrase, use the serial console in setup mode:
`setup pw off` restores the default, `setup pw <new>` sets a new one. Both
change only that line of `config.txt` and restart the box.

### Updates over the air

With `update_url` set, the box checks for an update once per boot, after the
jingle, when no figure is placed on the box and the battery is not low. A boot that starts
with a figure on the box, or on a low battery, does not check.

If the manifest's version differs from the running one, the box downloads the
image into its spare firmware slot (the LED shows fetching, about 30 s) and
reboots into it. The new image is kept only after the SD card mounts and the
codec responds; otherwise the box goes back to the old one at the next boot.

The download stops, and the box keeps its current firmware, if:

- anything starts playing, such as a story or the low-battery warning, or a
  figure needs the network;
- the length, SHA-256 or version inside the image does not match the manifest.

To publish an update:

    TEDDIEBOX_RELEASE=1 just ota-image

This writes `teddiebox.bin` and `teddiebox.txt` to `target/ota/`. Upload both
to the directory `update_url` names, e.g. `/content/teddiebox/` on teddyCloud's
port 8443. `update_url` then points at `teddiebox.txt`:

    update_url = https://teddycloud.local:8443/content/teddiebox/teddiebox.txt

Uploads use teddyCloud's HTTP API, so curl has to authenticate the server.
That takes a one-time setup, below, and then one command per update:

    for f in teddiebox.bin teddiebox.txt; do
      curl -k --pinnedpubkey "sha256//$PIN" -F "file=@target/ota/$f" \
        "https://teddycloud.local:8443/api/fileUpload?path=/teddiebox&special=content"
    done

`-k` only switches off the host name check, which cannot pass: teddyCloud's
certificate has no subject alternative name. The pin authenticates the server
instead, and curl refuses any server whose key is not `PIN`. Check the result
with
`curl -k --pinnedpubkey "sha256//$PIN" https://teddycloud.local:8443/content/teddiebox/teddiebox.txt`.

#### One-time upload setup

1. Get teddyCloud's CA as `tcca.der`. It is the file the SD card carries as
   `cert/tcca.der`. If you have no copy, download it once, over a network you
   trust, since nothing authenticates this first download:

       curl -k -o tcca.der https://teddycloud.local:8443/api/getFile/ca.der

2. Derive the pin from the server's certificate, after checking that the CA
   signed it:

       openssl s_client -connect teddycloud.local:8443 </dev/null 2>/dev/null |
         openssl x509 -out leaf.pem
       openssl verify -CAfile <(openssl x509 -inform der -in tcca.der) leaf.pem
       PIN=$(openssl x509 -in leaf.pem -pubkey -noout |
         openssl pkey -pubin -outform der |
         openssl dgst -sha256 -binary | openssl base64)

   `verify` must print `leaf.pem: OK`. Keep the value of `PIN`. It changes
   only when teddyCloud gets a new certificate key.

3. Create the directory. The upload API refuses a directory that does not
   exist:

       curl -k --pinnedpubkey "sha256//$PIN" -X POST --data-raw teddiebox \
         "https://teddycloud.local:8443/api/dirCreate?special=content"

Alternatively, copy both files into the `teddiebox` folder of teddyCloud's
`content` data directory.

The box updates when the version differs, not only when it is newer, so
publishing an older image rolls every box back to it.

Updates are not signed. Anyone who can write to that directory decides what
the box runs. The box only ever talks to the host in `update_url`, and
verifies it against `cert/tcca.der` like every other request.

teddyCloud has no authentication of its own, so anyone who can reach its web
port (8443) can upload a different image and manifest, and every box that
checks for updates will run it. Keep teddyCloud on a trusted network and do
not expose port 8443 to the internet. To restrict it further, put a reverse
proxy with basic auth or client certificates in front of that port, as the
teddyCloud maintainers recommend.

## Development

- `just check` runs all CI gates in CI's order. Run it before committing.
- The serial console is running at 115200 baud. The box prints its
  command list at boot.
- `just console` opens an interactive session. Only one program can use the
  port, so `just flash` refuses while a console is open.
- `firmware/vendor/` holds patched copies of `mbedtls-rs`, `mbedtls-rs-sys` and
  `smoltcp`. It is gitignored. `just vendor` fetches and patches them, and every
  build recipe runs it. Plain `cargo` in `firmware/` needs it run once first.
  The patches and the reasons for them are in `scripts/vendor-*.sh`.
- `HARDWARE.md` covers the board, the wiring and download mode.

### Use of LLMs

This project is developed with LLM assistance.

Contributors may use any tool, on these terms:

- Read and understand the change before you submit it for others to review.
- Say how a change was verified. For firmware behaviour, that means
  on a real box.
