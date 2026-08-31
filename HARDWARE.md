# Serial console on the board

How to get a serial console onto the Toniebox. Everything
here is about board **`TONIEBOX-ESP32` rev 1.6.C (2022-04-27)**; Toniebox pinouts
are revision-specific, so check the silkscreen before trusting any of it.

The ESP32-S3 on this board has **no USB**: GPIO19/20, the S3's native D−/D+, are
used for the red LED and the left ear. There is no USB-Serial-JTAG. UART0 on
J103 is the debug channel.

## J103 — UART0

Three bare pads in a row, not a populated header, near D100/R149 just below
J102. The order along the row is:

```
TxD   RxD   GND
```

Ground is an end pad, so **RxD is the middle pad whichever way round the board
is turned**. That is the orientation-proof way to read it, and the check that
catches a mis-crimped cable: if the wire you believe is ground came off the
middle pad, stop and ring it out against the SD socket shell before connecting
anything.


## Sources

- [RevvoX ESP32 pinout table](https://tonies-wiki.revvox.de/docs/wiki/esp32/pinout/)
- [teddyCloud ESP32 certificate dump, with the J103 photo](https://tonies-wiki.revvox.de/docs/tools/teddycloud/setup/dump-certs/esp32/)
