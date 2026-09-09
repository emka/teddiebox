#!/usr/bin/env python3
"""Record one charge-to-cutoff run of the box's pack, unattended.

Arms the firmware's own `batlog` and writes every line it prints to a CSV,
across the whole run: charging, the rest after the charger comes off, and the
discharge down to the point the box stops talking. One file, one timeline.

It is meant to be started and left. The only things it asks a person to do are
plug the charger in and take it out again, and it says when.

    scripts/battery-run.py --out pack-2026-09-09.csv

`--self-test` runs the logic below without a box attached.
"""
import argparse
import os
import re
import select
import sys
import time

# The charger channel is uncalibrated but separates the two states cleanly:
# raw ~1930-1957 with nothing connected, raw 4095 (railed) on the charger.
# Anything in between has never been seen, so the midpoint is a safe line.
CHARGER_PRESENT_RAW = 3000

BATLOG_HEADER = "batlog,ms,raw,mv,playing,charger_raw"


def charger_present(charger_raw):
    """Whether the charger is plugged in, from the raw ADC count."""
    return charger_raw >= CHARGER_PRESENT_RAW


def parse_batlog(line):
    """One `batlog` CSV line as a dict, or None if this is not one.

    The header the firmware prints when the log is armed is rejected by the
    same numeric parse that rejects any other malformed line — its fields are
    the column names. An explicit test for it lives in `self_test` so that
    stays true if the header ever changes.
    """
    line = line.strip()
    if not line.startswith("batlog,"):
        return None
    parts = line.split(",")
    if len(parts) != 6:
        return None
    try:
        return {
            "ms": int(parts[1]),
            "raw": int(parts[2]),
            "mv": int(parts[3]),
            "playing": int(parts[4]),
            "charger_raw": int(parts[5]),
        }
    except ValueError:
        return None


class FullCharge:
    """Decides when a charging NiMH pack has had enough.

    Two signals, either of which ends the charge:

    * **-dV.** A NiMH pack's voltage falls once it is full — a few millivolts
      per cell, so `drop_mv` across three. This is the real signal and the one
      every dedicated charger uses.
    * **A plateau.** If the pack has not set a new peak for `plateau_s`, the
      charge has stopped going anywhere. This is the fallback for a charger
      that tapers instead of driving hard enough to produce a clear -dV.

    `min_charge_s` guards both: a pack that is already near full still needs
    long enough for the readings to mean something, and the first minutes
    after a charger is connected are noise.
    """

    def __init__(self, drop_mv=15, plateau_s=1800, min_charge_s=600):
        self.drop_mv = drop_mv
        self.plateau_s = plateau_s
        self.min_charge_s = min_charge_s
        self.started_at = None
        self.peak_mv = None
        self.peak_at = None
        self.reason = None

    def update(self, t_s, mv):
        """Feed one reading. True once the pack should be called full."""
        if self.started_at is None:
            self.started_at = t_s
        if self.peak_mv is None or mv > self.peak_mv:
            self.peak_mv = mv
            self.peak_at = t_s

        if t_s - self.started_at < self.min_charge_s:
            return False

        if mv <= self.peak_mv - self.drop_mv:
            self.reason = f"-dV: {mv} mV is {self.peak_mv - mv} below the {self.peak_mv} mV peak"
            return True

        if t_s - self.peak_at >= self.plateau_s:
            self.reason = (
                f"plateau: no new peak above {self.peak_mv} mV "
                f"for {int(t_s - self.peak_at)} s"
            )
            return True

        return False


def next_phase(phase, on_charge, full):
    """The phase after one sample, from the charger line and the full verdict.

    `waiting` -> `charge` -> `full` -> `discharge`, and back to `charge` if the
    charger reappears. The charger line wins over `full`: a pack whose charger
    came off is discharging whatever the detector just decided.

    A discharge-only run is this same machine entered at `charge` with `full`
    wired to False, so it can only ever fall through to `discharge`.
    """
    if phase == "waiting":
        return "charge" if on_charge else "waiting"
    if phase in ("charge", "full"):
        if not on_charge:
            return "discharge"
        return "full" if (phase == "full" or full) else "charge"
    if phase == "discharge":
        return "charge" if on_charge else "discharge"
    return phase


def self_test():
    assert charger_present(4095), "railed means plugged in"
    assert charger_present(3000), "the threshold itself counts as present"
    assert not charger_present(1957), "nothing connected"
    assert not charger_present(1930), "nothing connected"

    assert parse_batlog(BATLOG_HEADER) is None, "the header is not a sample"
    assert parse_batlog("teddiebox: alive 7") is None, "not a batlog line"
    assert parse_batlog("batlog,1,2,3") is None, "too few fields"
    assert parse_batlog("batlog,a,b,c,d,e") is None, "non-numeric"
    assert (
        parse_batlog("teddiebox: 1,2,3,4,5,6") is None
    ), "six numeric fields are not enough — it has to be a batlog line"
    sample = parse_batlog("batlog,123456,3055,3751,0,1957")
    assert sample == {
        "ms": 123456,
        "raw": 3055,
        "mv": 3751,
        "playing": 0,
        "charger_raw": 1957,
    }, sample

    # Rising and early: not full, however the numbers move.
    d = FullCharge(drop_mv=15, plateau_s=1800, min_charge_s=600)
    assert not d.update(0, 3800)
    assert not d.update(60, 3820)
    assert not d.update(120, 3700), "a drop inside the guard window is noise"

    # A clear -dV after the guard window ends the charge.
    d = FullCharge(drop_mv=15, plateau_s=1800, min_charge_s=600)
    d.update(0, 3800)
    d.update(700, 3900)
    assert d.update(800, 3884), "16 mV below the peak is -dV"
    assert "-dV" in d.reason, d.reason

    # A pack that simply stops rising ends on the plateau instead.
    d = FullCharge(drop_mv=15, plateau_s=1800, min_charge_s=600)
    d.update(0, 3900)
    assert not d.update(1000, 3900), "still inside the plateau window"
    assert d.update(1801, 3900), "no new peak for the whole window"
    assert "plateau" in d.reason, d.reason

    # A pack still climbing keeps the plateau window open.
    d = FullCharge(drop_mv=15, plateau_s=1800, min_charge_s=600)
    d.update(0, 3800)
    for t in range(600, 5000, 300):
        assert not d.update(t, 3800 + t // 10), "a new peak resets the window"

    # The whole phase table, written out. A full run starts in `waiting`; a
    # discharge-only run starts in `charge` and never sees a `full` verdict,
    # so the rows with full=False are that run's whole life.
    for phase, on_charge, full, expected in [
        ("waiting", False, False, "waiting"),
        ("waiting", True, False, "charge"),
        ("charge", True, False, "charge"),
        ("charge", True, True, "full"),
        ("charge", False, False, "discharge"),
        ("charge", False, True, "discharge"),
        ("full", True, True, "full"),
        ("full", False, True, "discharge"),
        ("discharge", False, False, "discharge"),
        ("discharge", True, False, "charge"),
    ]:
        got = next_phase(phase, on_charge, full)
        assert got == expected, f"{phase}/{on_charge}/{full} gave {got}"

    print("self-test: all assertions passed")


class Console:
    """The serial port, line by line, with the writes this run needs."""

    def __init__(self, port):
        self.port = port
        self.fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        self.buf = b""

    def send(self, line):
        os.write(self.fd, (line + "\r").encode())

    def lines(self, timeout):
        """Yield whole lines for up to `timeout` seconds."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            ready, _, _ = select.select([self.fd], [], [], 0.5)
            if not ready:
                continue
            try:
                chunk = os.read(self.fd, 4096)
            except (BlockingIOError, OSError):
                continue
            if not chunk:
                continue
            self.buf += chunk
            while b"\n" in self.buf:
                raw, self.buf = self.buf.split(b"\n", 1)
                yield raw.decode("utf-8", "replace").strip()

    def close(self):
        os.close(self.fd)


def say(message):
    print(f"\n=== {message}\n", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--port", default="/dev/ttyUSB0")
    ap.add_argument("--out", help="CSV to write (required unless --self-test)")
    ap.add_argument(
        "--every",
        type=int,
        default=30,
        help="seconds between samples, 1-255 (default 30)",
    )
    ap.add_argument("--drop-mv", type=int, default=15, help="-dV threshold (default 15)")
    ap.add_argument(
        "--plateau-min",
        type=int,
        default=30,
        help="minutes without a new peak that count as full (default 30)",
    )
    ap.add_argument(
        "--min-charge-min",
        type=int,
        default=10,
        help="minutes before a full-charge call is believed (default 10)",
    )
    ap.add_argument(
        "--max-charge-h", type=float, default=10.0, help="give up charging after this"
    )
    ap.add_argument(
        "--quiet-end-min",
        type=float,
        default=5.0,
        help="minutes of silence that mean the box has died (default 5)",
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        self_test()
        return 0
    if not args.out:
        ap.error("--out is required")
    if not 1 <= args.every <= 255:
        ap.error("--every must be 1-255 (the firmware takes one byte)")

    con = Console(args.port)
    out = open(args.out, "w", buffering=1)
    out.write("# teddiebox pack run\n")
    out.write(f"# started {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
    out.write(f"# interval {args.every} s\n")
    out.write("phase,wall_s,ms,raw,mv,playing,charger_raw\n")

    started = time.time()
    phase = "waiting"
    detector = FullCharge(
        drop_mv=args.drop_mv,
        plateau_s=args.plateau_min * 60,
        min_charge_s=args.min_charge_min * 60,
    )
    last_sample_at = time.time()
    last_report = 0.0
    samples = 0

    def arm():
        # `awake on` because a run is hours of the box deliberately doing
        # nothing, which is what the idle timeout exists to end. An armed
        # batlog also counts as use, so this is belt and braces — but the
        # firmware forgets both on a reset, and a reset mid-run is exactly
        # when it would matter.
        con.send("awake on")
        time.sleep(0.3)
        con.send(f"batlog {args.every:02X}")

    say("Arming the box. Do not unplug the serial adapter until this finishes.")
    arm()

    say("PLUG THE CHARGER IN NOW. Waiting for it...")

    try:
        while True:
            got_line = False
            for line in con.lines(timeout=5):
                got_line = True

                # A reset loses `awake on` and the armed log, and a run that
                # quietly stopped recording looks exactly like a flat pack.
                if "painting the stack" in line:
                    say("The box reset. Re-arming.")
                    time.sleep(2)
                    arm()
                    continue

                sample = parse_batlog(line)
                if sample is None:
                    continue

                last_sample_at = time.time()
                wall = last_sample_at - started
                on_charge = charger_present(sample["charger_raw"])

                full = (
                    phase == "charge"
                    and on_charge
                    and detector.update(wall, sample["mv"])
                )
                moved_to = next_phase(phase, on_charge, full)
                if moved_to != phase:
                    if moved_to == "charge":
                        say(
                            "Charger detected. Charging — this will take hours."
                            if phase == "waiting"
                            else "Charger is back on — the discharge is contaminated."
                        )
                    elif moved_to == "full":
                        say(
                            f"Pack looks full ({detector.reason}).\n"
                            "    UNPLUG THE CHARGER NOW. Recording continues."
                        )
                    elif phase == "full":
                        say("Charger removed. Recording the discharge to cutoff.")
                    else:
                        say(
                            "Charger removed before I called it full — "
                            "recording the discharge from here."
                        )
                    phase = moved_to

                out.write(
                    f"{phase},{wall:.1f},{sample['ms']},{sample['raw']},"
                    f"{sample['mv']},{sample['playing']},{sample['charger_raw']}\n"
                )
                samples += 1

                if time.time() - last_report > 60:
                    last_report = time.time()
                    print(
                        f"[{time.strftime('%H:%M:%S')}] {phase}: "
                        f"{sample['mv']} mV, {samples} samples",
                        flush=True,
                    )

            quiet_for = time.time() - last_sample_at
            if phase == "discharge" and quiet_for > args.quiet_end_min * 60:
                say(
                    f"The box has said nothing for {quiet_for / 60:.1f} minutes. "
                    "Taking that as the end of the discharge."
                )
                break
            if phase in ("charge", "full") and (
                time.time() - started > args.max_charge_h * 3600
            ):
                say("Charging has run past --max-charge-h. Stopping.")
                break
            if not got_line and quiet_for > args.quiet_end_min * 60:
                say("No output at all. Is the box on, and the adapter plugged in?")
                break
    except KeyboardInterrupt:
        say("Interrupted. The file holds everything recorded so far.")
    finally:
        out.close()
        con.close()

    print(f"\nWrote {samples} samples to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
