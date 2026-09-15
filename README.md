# teddiebox

Replacement firmware for a Toniebox, written in Rust for the ESP32-S3 board a
`TONIEBOX-ESP32` rev 1.6.C carries. It reads the same figures, plays the same
stories off the same SD card, and fetches missing ones from a local
[teddyCloud](https://github.com/toniebox-reverse-engineering/teddycloud).

`HARDWARE.md` covers the board, the wiring and how to get into download mode.

## How this box differs from a stock one

Deliberate departures, not omissions. Everything else aims to behave the way
the box a child already knows behaves.

### It remembers where a story got to, even after being switched off

A stock box remembers where a story was only until it goes into standby. Put a
figure back the next day and the story starts again from the beginning.

This box remembers the **exact spot**, and keeps remembering it after being
switched off:

- **Lift a figure and put it straight back** and the story carries on where it
  was, as stock does.
- **Put it back tomorrow** and it still does. The place is written to the card
  when the box needs the memory for another figure, or when it shuts itself
  down.

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
what is on the box's. Set the box up somewhere you would be happy saying the
passphrase out loud.

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
