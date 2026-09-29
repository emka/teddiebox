# teddiebox

Replacement firmware for the Toniebox with the ESP32 rev 1.6.C
board, written in Rust. It reads the same figures, and fetches audio from a local
[teddyCloud](https://github.com/toniebox-reverse-engineering/teddycloud).

## Motivation

teddyCloud replaces the manufacturer's cloud, but the box still runs closed
firmware that cannot be improved. This firmware is open, and
can be extended. It behaves like stock except where noted below.

## Features different to stock

### Configuration file

The firmware is configured by writing `config.txt` on the SD card.
Editing this file is also possible by booting the box in setup mode (see its own section below).

### Position per story

Stock firmware remembers the position of the last figure played, also across standby.
Playing a different figure in between resets it.

teddiebox remembers the position of every story, also across power-off.

- The position is written to the card when the figure's memory is needed for
  another figure, and at shutdown.
- A story played to its end starts from the beginning next time.
- The position is stored in `<STORY>.POS` next to the story on the card. Delete
  it to start that story from the beginning.
- A flat battery or a reset loses the position since the last write. Writing
  more often would wear the card.

### Chapter skip

- **Hold an ear** for about half a second: the larger (right) ear skips
  forward, the smaller one back.
- **Slap the side** of the box: right side forward, left side back.

Because a press could be a hold, volume changes when the ear is released rather
than when it is pressed. Set `ears_skip = no` in `CONFIG.TXT` to get stock ear
behaviour: volume changes on press, and slapping still skips.

### Setup mode

If `CONFIG.TXT` is wrong or missing, the box can be configured over WiFi
without removing the card. See
[Changing settings without a card reader](#changing-settings-without-a-card-reader).

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
  `CONFIG.TXT`.
- **Updates over the air.** See [Updates over the air](#updates-over-the-air).

## Installation

### Requirements

- A Toniebox with the `TONIEBOX-ESP32` rev 1.6.C board. See `HARDWARE.md` for
  the board, the wiring and download mode.
- A USB serial adapter connected to the box's console.
- A [teddyCloud](https://github.com/toniebox-reverse-engineering/teddycloud)
  server on your network.
- The box's certificate files `CLIENT.DER` and `PRIVATE.DER` on the host.
- The teddyCloud CA on the SD card at `cert/tcca.der`.

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

### Flashing identity once

The box authenticates to teddyCloud with its own certificate and key. They are
stored in the `cert` flash partition, not on the card.

1. Set `TEDDIEBOX_IDENTITY_DIR` in `.envrc.local` to the directory holding
   `CLIENT.DER` and `PRIVATE.DER`.
2. Run `just identity`.
3. Check that the box reports its identity from flash at boot.

This is needed once per box; flashing firmware does not touch the `cert`
partition. Without it the box plays what is on the card but cannot fetch, and
says so at boot.

### Configuration

Settings are in `CONFIG.TXT` in the card's root:

- One `key = value` per line. `#` starts a comment. Blank lines are ignored.
- Unknown keys are ignored, so newer cards work with older firmware.
- A known key with an invalid value is reported as an error, not guessed at.

| key | default | |
|---|---|---|
| `ssid` | required | WiFi network |
| `password` | empty | WiFi passphrase. Everything after `=` is used, including `#`. Empty for an open network |
| `server` | required | `host:port` of your teddyCloud |
| `ears_skip` | `yes` | holding an ear skips a chapter |
| `update_url` | none | `https://` URL of an update manifest. Parsed, but not used yet |
| `setup_password` | `teddiebox` | passphrase of the setup network, 8 to 63 characters |

**Trust `server`.** The box verifies the server's certificate against
`TCCA.DER` on the card, and sends it its own certificate and the placed
figure's token.

**The WiFi key is stored in flash.** The box derives a key from `ssid` and
`password` and keeps it in the `wifi` partition, which cuts joining from about
2s to 0.1s. It is re-derived after changing either value. Anyone who can
read the flash can join your network, but the passphrase is on the card in
plain text anyway.

### Changing settings without a card reader

1. Hold both ears while switching the box on, until the light turns on. The box
   starts a WiFi network instead of playing.
2. Join `teddiebox-setup` with passphrase `teddiebox`.
3. Open <http://192.168.4.1/>. It shows `CONFIG.TXT` for editing.
4. Press **Save and restart**. The file is checked first; errors are shown on
   the page. The box then restarts.

Setup mode ends by itself after ten minutes, or when saving.

Security:

- The page shows your WiFi passphrase in plain text.
- The default setup passphrase is public. Anyone in range who knows it and
  records your device joining can decrypt the session, including your WiFi
  passphrase. The page cannot use HTTPS: the box has no clock and the phone
  has no reason to trust its certificate. Use setup mode only where that risk
  is acceptable.

Set `setup_password` in `CONFIG.TXT` to use your own passphrase. A box with no
card, an unreadable card or an unparseable `CONFIG.TXT` still uses the default.

To reset a forgotten setup passphrase, use the serial console in setup mode:
`setup pw off` restores the default, `setup pw <new>` sets a new one. Both
change only that line of `CONFIG.TXT` and restart the box.

### Updates over the air

Not implemented yet. Every update is a `just flash`.

What exists: the flash has two firmware slots. A new image is kept only after
the card mounts and the codec responds; otherwise it is reverted at the next
boot. Downloading a manifest and an image is not implemented.

## Development

- `just check` runs all CI gates in CI's order. Run it before committing.
- The serial console is running at 115200 baud. The box prints its
  command list at boot.
- `just console` opens an interactive session. Only one program can use the
  port, so `just flash` refuses while a console is open.
- `HARDWARE.md` covers the board, the wiring and download mode.
