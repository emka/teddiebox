# Changelog

## 0.1.0 (2026-10-06)

Replacement firmware for the Toniebox (ESP32 rev 1.6.C), written in Rust. It reads the same figures and fetches audio from a local teddyCloud.

### Different to stock

* Position per story, kept across power-off
* Chapter skip: hold an ear, or slap the side of the box
* Trusted CA read from the SD card
* Setup mode: configure over WiFi without removing the card
* Status light
* Settings in `config.txt` on the SD card
* Updates over the air with `update_url`

### Missing compared to stock

* Rewind and fast-forward by tilting
* Telemetry to teddyCloud
* Settings from teddyCloud

Unofficial project. Flashing replaces the stock firmware and can brick the box. See the README before you start.

