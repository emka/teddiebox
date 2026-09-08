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

### The indicator says more

A stock box does not show a charging colour while it sits idle. This one uses
the single RGB light to say what it is doing:

| colour | meaning |
|---|---|
| green | idle, or playing a story |
| blue | fetching a story it does not have |
| orange | the battery is running low |
| red | a fault, or a battery about to give out |
| cyan | idle, and on the charger |
| dark | standby |

It is steady rather than breathing, and deliberately dim: this sits in a
child's room.
