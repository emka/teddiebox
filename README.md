# teddiebox

Replacement firmware for a Toniebox, written in Rust for the ESP32-S3 board a
`TONIEBOX-ESP32` rev 1.6.C carries. It reads the same figures, plays the same
stories off the same SD card, and fetches missing ones from a local
[teddyCloud](https://github.com/toniebox-reverse-engineering/teddycloud).

`HARDWARE.md` covers the board, the wiring and how to get into download mode.

## How this box differs from a stock one

Deliberate departures, not omissions. Everything else aims to behave the way
the box a child already knows behaves.

### It remembers where every story got to

A stock box remembers where one story was: the last figure it played. Put that
figure back, even after standby, and it carries on. Put a different figure on
in between and the first one starts again from the beginning.

This box remembers the **exact spot for every story**, and keeps remembering
it after being switched off:

- **Put the last figure back** and the story carries on where it was, as stock
  does.
- **Play another figure in between** and the first one still carries on where
  it was. The place is written to the card when the box needs the memory for
  another figure, or when it shuts itself down.

A story played all the way to its end starts from the beginning next time. A
finished story is not a paused one.

The place is kept in a small text file called `<STORY>.POS` beside the story on
the card — one number, which you can read or delete on a laptop. Deleting it
just means that story starts from the beginning.

What is *not* remembered is an ending nobody chose: if the battery goes flat
mid-story, or the box is reset, the place falls back to whatever was last
written to the card. This is a deliberate trade — writing every few seconds to
survive it would wear the card for a case that leaves the box unusable anyway.

### The controls do more than volume

A stock box's ears do one thing between them: tap the larger for louder, the
smaller for quieter. This box keeps that and adds holding.

**Holding an ear changes the chapter.** Hold the larger — it is on the right —
and the story moves on; hold the smaller and it goes back. The chapter changes
after about half a second, while your finger is still on the ear, so you can
hear that the box heard you. Letting go does nothing more.

Two meanings on one ear cost something, and it is worth knowing which: until
that half second is up the box cannot tell a tap from a hold, so the volume
waits for you to let go.

**A slap on the side of the box also changes the chapter**: the right face
forward, the left face back. The accelerometer detects the knock itself, so it
catches one however busy the box is.

**If you would rather have stock's ears**, put `ears_skip = no` in `CONFIG.TXT`
on the card. A press can then only mean one thing, so the volume moves the
instant the ear goes down instead of waiting. Chapters are still reachable by
slapping, so nothing is lost.

### Headphones get their own, quieter volume

The socket on this box is not a switching one: plugging headphones in leaves
the speaker playing, on stock firmware and on this. So this box does it in
software — a jack goes in, the speaker goes quiet and the story carries on in
the headphones; a jack comes out and the speaker comes back. The story never
stops either way, and nothing has to be pressed.

**Headphones have their own volume.** The ears mean the same thing they always
did, but the six steps they move are about 12 dB quieter when something is
plugged in, and each output remembers where it was left: turning the
headphones down does not leave the speaker quiet when the plug comes out.

That 12 dB is an estimate, not tuned against real headphones. If it is wrong
for yours, both volume scales are written out step by step in
`crates/teddiebox-core/src/volume.rs`: change the levels in the `HEADPHONES`
table to what you want to hear, and the `HEADPHONE_OFFSET_DB` constant beside
it to the new difference between the two. A test checks that they still agree.

### Its settings can be fixed without a card reader

A box whose `CONFIG.TXT` is wrong or missing cannot reach the network, and on a
stock box there would be nothing to do about it but take the card out. This one
can be told its settings over the air:

1. **Hold both ears** and switch the box on, keeping them held until the light
   comes on. The box raises its own WiFi network instead of becoming a teddy
   bear.
2. **Join `teddiebox-setup`** from a phone or laptop. The passphrase is
   `teddiebox`.
3. **Open <http://192.168.4.1/>**. The page shows `CONFIG.TXT` exactly as it is
   on the card — comments and all — in one editable box.
4. **Edit it and press Save and restart.** The box checks the file before
   writing it, so a mistake comes back as a message above the box rather than
   as a card that has to be fixed on a laptop. The network then disappears and
   the box restarts.

The box gives up and restarts on its own after ten minutes, whether or not
anybody joined: an access point left beaconing overnight flattens the battery.

**Two things worth knowing before you use it.** The page shows your home WiFi
passphrase in clear, because that is the file being edited. And the passphrase
for `teddiebox-setup` is the one printed above — it is in this public
repository, so anyone in radio range who has read this page can also read
what is on the box's.

That second one goes further than joining. WPA2 gives each client its own key,
but that key is derived from the four-way handshake and the passphrase — so
somebody in range who already knows the passphrase and records the moment your
phone joins can read the whole session afterwards, without ever joining
themselves. The page is plain HTTP, and it cannot be anything else: a box with
no clock and a phone with no reason to trust it have nothing to build a
certificate check on. Set the box up somewhere you would be happy saying both
passphrases out loud.

**You can give the box its own setup passphrase.** Put `setup_password =` and
between 8 and 63 characters of your own in `CONFIG.TXT`, and the box asks for
that instead of `teddiebox`. Worth understanding before you do: the published
passphrase is what a box with no card, an unreadable card, or a `CONFIG.TXT`
too broken to parse falls back to — so it always gets you in to *those*.

If you forget one you have set, the way back is the serial console that setup
mode runs alongside the page: send `setup pw off` to put the card back to the
published passphrase, or `setup pw` and a new one to change it. Either rewrites
that one line of `CONFIG.TXT`, leaves every other line exactly as you wrote it,
and restarts the box. So a forgotten passphrase costs a USB serial cable rather
than a card reader.

### The indicator says more

A stock box does not show a charging colour while it sits idle. This one uses
the single RGB light to say what it is doing:

| colour | meaning |
|---|---|
| green | idle, or playing a story |
| blue | waiting on the server — fetching a story it does not have, or checking that the one it has is still the current one |
| orange | the battery is running low |
| red | a fault, or a battery about to give out |
| cyan | idle, and on the charger |
| magenta | the setup page is up |
| dark | standby |

It is steady rather than breathing, and deliberately dim: this sits in a
child's room.

## What it does not do yet

**Rewind and fast-forward are not implemented.** On a stock box, tilting it
to one side winds the story back or forward within a chapter. This box does
nothing when it is tilted: the only way to move through a story is a whole
chapter at a time, by holding an ear or slapping the side.

## What goes in `CONFIG.TXT`

One `key = value` per line, `#` starts a comment, blank lines are ignored.
Unknown keys are skipped, so a card written for a newer firmware still boots an
older one — but a key the box *does* know, given a value it cannot use, is
refused out loud rather than guessed at. `ears_skip = ture` is a typo about
what the ears do, and the box saying so beats the box deciding for you.

| key | | |
|---|---|---|
| `ssid` | required | your WiFi network |
| `password` | | its passphrase. Everything after the `=` is the password, `#` included — so a passphrase with a hash in it needs no escaping. Leave it empty for an open network |
| `server` | required | `host:port` of your teddyCloud |
| `ears_skip` | `yes` | whether holding an ear changes the chapter |
| `update_url` | | full `https://` URL of an update manifest. Checked when the card is read, but no firmware fetches an update yet |
| `setup_password` | | the box's own setup passphrase, 8 to 63 characters. See above |

**`server` is the line in this file carrying the weight.** The box checks the
server's certificate against `TCCA.DER` on the card and will not connect
without it — but it also identifies itself to whatever it reaches, sending its
own certificate and the placed figure's token, so that a request can be
relayed to the real teddyCloud. Point `server` somewhere you trust.

The box's own certificate and key are **not** on the card. They live in the
`cert` flash partition, written once per box with `just identity` — a private
key does not belong on a medium that comes out of the box and goes into other
machines. A box that has not been provisioned plays everything on its card and
cannot fetch; it says so at boot.

`CERT/` on the card only needs `TCCA.DER`. If the card also has `CLIENT.DER`
and `PRIVATE.DER` there, the firmware ignores them: once `just identity` has
been run and the box reports its identity from flash at boot, delete both
files from the card, so the private key is not left on it.

The box also keeps the WiFi key it derives from `ssid` and `password`, in the
`wifi` flash partition, so that joining the network takes a tenth of a second
rather than two. Change either line and the box derives a new key by itself,
after its first successful join with the new one. Whoever can read the box's
flash can therefore join your network — but whoever holds the box holds the
card, where the passphrase already sits in plain text. The partition arrives
with `just flash`; a box whose partition table predates it simply joins the
slower way.

## Building it

`just check` runs every gate the pipeline runs, in the same order, and is what
to run before committing. `just flash` puts the box into download mode, flashes
it, and starts it again — the order in `scripts/flash.sh` is not arbitrary, and
getting it wrong costs opening the case.

The box carries a serial console on `/dev/ttyUSB0` at 115200 and prints its own
command list at boot. Most of those commands exist to take a box apart rather
than to run one — poking codec registers, arming the other firmware slot,
reading a figure's memory — so a build meant to live on a shelf leaves them
out:

    TEDDIEBOX_RELEASE=1 just flash

That image answers `dl`, which reboots it for flashing, and nothing else. It is
also about 19 KB smaller. Everything the box does for a child works the same;
there is just no way to make it do anything else over the console.
