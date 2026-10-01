# Serial console on the board

How to get a serial console onto the Toniebox. Everything
here is about board **`TONIEBOX-ESP32` rev 1.6.C (2022-04-27)**; Toniebox pinouts
are revision-specific, so check the silkscreen before trusting any of it.

The ESP32-S3 on this board has **no USB**: GPIO19/20, the S3's native D−/D+, are
used for the red LED and the larger ear (on the box's right). There is no
USB-Serial-JTAG. UART0 on J103 is the debug channel.

## J103: UART0

Three bare pads in a row, not a populated header, near D100/R149 just below
J102. The order along the row is:

```
TxD   RxD   GND
```

For a stable connection, solder a 3-pin JST PH 2.0 header (2.0 mm pitch) to
the pads. The pads tear off easily, so heat them briefly.

With the manufacturer's cable colours: white is GND, black is RxD (middle),
red is TxD.

## JTAG

Not needed. The USB-Serial-JTAG peripheral is unavailable (see above). The
ESP32-S3's pad JTAG (MTCK, MTDO, MTDI, MTMS) is routed to J102, just above
J103, and nothing here uses it. Flashing and debugging both go over UART0 on
J103.

## Getting in and out of download mode

Download mode is where a flashing tool can talk to the box. Three ways in, in
descending order of convenience:

| Route | Needs | When |
|---|---|---|
| Type `dl` + Enter on the console | firmware running and responsive | normal development |
| `esptool --before no-reset --after watchdog-reset run` | box already in download mode | to leave download mode |
| Short **J100**, then apply power cold | nothing but the board | always works; the recovery floor |

The first two need the firmware or the ROM to cooperate. **J100 with a cold
power-on is the one that cannot fail**, because the chip samples GPIO0 in mask
ROM before it reads a byte of flash. Whatever else you do, that route stays
open — which is what makes flashing experimental firmware safe.

Note that the DTR and RTS lines are not wired to anything on this board, so
esptool's and espflash's default auto-reset cannot work. Every invocation needs
`--before no-reset`, and getting the chip to reset itself means either the
watchdog route above or removing power.

**Flash before you probe.** `espflash` must be the first tool to touch the port
after the box enters download mode. Running `esptool` first — even `flash-id` —
leaves esptool's stub loader resident, and `espflash` then fails with `Timeout
while running MemData command`, or simply cannot connect. Recovering from that
costs a cold boot with J100 shorted, so the order is: enter download mode,
flash, and only then probe.

**A cold power-on is not the same as a reset.** Only power-on clears the RTC
domain, and the `FORCE_DOWNLOAD_BOOT` bit lives there. If the box ever comes up
in download mode when you did not ask it to, pull power rather than resetting.

## Sources

- [RevvoX ESP32 pinout table](https://tonies-wiki.revvox.de/docs/wiki/esp32/pinout/)
- [teddyCloud ESP32 certificate dump, with the J103 photo](https://tonies-wiki.revvox.de/docs/tools/teddycloud/setup/dump-certs/esp32/)
