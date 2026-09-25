#!/usr/bin/env python3
"""Walk the deep-sleep measurements with a meter in the hand and a box asleep.

Three numbers decide whether the power story works, and none of them can be
taken without a person: the current the box draws asleep, the level GPIO47
floats to while the digital domain is down, and whether an ear press brings it
back. This script does the parts a person should not have to remember — the
port, the console lines, what each answer means, and writing the record — and
asks for the parts only a meter can answer.

    scripts/sleep-check.py --out sleep-2026-09-14.md

It sets the line itself, so it needs no `stty` after a flashing tool. It does
need the port to itself: kill any capture **by PID** first.

`--self-test` runs the parsing below without a box attached.
"""
import argparse
import os
import re
import select
import sys
import termios
import time

# What the ROM prints on the first line after a reset. 0x5 is the wake this
# measurement is trying to cause; 0x1 is a power-on — and on this board a
# brownout reports as one too, so a low pack looks exactly like a clean boot.
RESET_REASONS = {
    "0x5": "a wake from deep sleep — what this measurement wants",
    "0x1": "a power-on, or a brownout: this board does not tell them apart",
    "0x3": "a software reset, so something rebooted rather than woke",
    "0x7": "a watchdog reset",
    "0x8": "a watchdog reset",
}

# The line the firmware prints immediately before it stops clocking the UART.
SLEEPING = re.compile(r"teddiebox: sleeping")
# The line it prints instead when the wake source would not arm.
NOT_ARMED = re.compile(r"teddiebox: sleep not armed — (.+)")
# The ROM's own first line after any reset.
RESET_LINE = re.compile(r"rst:(0x[0-9a-fA-F]+)")
# The console banner, printed once the firmware is up and listening.
BANNER = re.compile(r"teddiebox: dl rb")

MICROAMPS_PER_MILLIAMP = 1000.0


def reset_meaning(line):
    """What a ROM reset line says happened, or None if this is not one."""
    found = RESET_LINE.search(line)
    if not found:
        return None
    return RESET_REASONS.get(found.group(1).lower(), "an unrecognised reset reason")


def parse_current_ua(answer):
    """A meter reading as microamps, from whatever unit it was typed in.

    Accepts `40u`, `40 uA`, `40 µA`, `0.04 mA`, `0.04m`, and a bare number,
    which is read as microamps because that is the unit the answer is expected
    in. Returns None for anything it cannot read, so the caller can ask again
    rather than record a number nobody typed.
    """
    text = answer.strip().lower().replace("μ", "u").replace("µ", "u")
    found = re.fullmatch(r"([0-9]*\.?[0-9]+)\s*(ua|u|ma|m|a)?", text)
    if not found:
        return None
    value = float(found.group(1))
    unit = found.group(2) or "u"
    if unit in ("ma", "m"):
        return value * MICROAMPS_PER_MILLIAMP
    if unit == "a":
        return value * MICROAMPS_PER_MILLIAMP * MICROAMPS_PER_MILLIAMP
    return value


def verdict_for(sleep_ua):
    """What a sleep current means, as a one-line verdict."""
    if sleep_ua is None:
        return "not measured"
    if sleep_ua < 500:
        return "tens of microamps — what good looks like"
    if sleep_ua < 5_000:
        return "hundreds of microamps: better than a park, worse than expected"
    return (
        "milliamps — a domain stayed powered and the sleep saved nothing. "
        "Stop here: find out what stays powered before relying on sleep"
    )


def open_port(path):
    """The port, at 115200 raw, set up by us so no `stty` is needed first."""
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    attrs = termios.tcgetattr(fd)
    attrs[0] = attrs[1] = attrs[3] = 0
    attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    attrs[4] = attrs[5] = termios.B115200
    attrs[6][termios.VMIN] = 0
    attrs[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, attrs)
    return fd


def read_lines(fd, seconds, log, stop=None):
    """Everything the box says for `seconds`, logged, as a list of lines.

    Returns early when `stop` matches — but only ever on a line that comes
    *before* what is being measured, never on the event itself: releasing the
    port on the thing you came to see throws away everything printed after it.
    """
    deadline = time.time() + seconds
    buffered = b""
    lines = []
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if ready:
            buffered += os.read(fd, 4096)
            while b"\n" in buffered:
                raw, buffered = buffered.split(b"\n", 1)
                line = raw.decode("utf-8", "replace").rstrip("\r")
                log.write(line + "\n")
                log.flush()
                print(f"    {line}")
                lines.append(line)
                if stop and stop.search(line):
                    return lines
    return lines


def ask(prompt):
    """One answer from the person holding the meter."""
    return input(f"\n>>> {prompt}\n>>> ").strip()


def ask_current(prompt):
    """A meter reading, asked again until it is one or is refused outright."""
    while True:
        answer = ask(prompt + " (unit optional, microamps assumed; blank to skip)")
        if not answer:
            return None
        microamps = parse_current_ua(answer)
        if microamps is not None:
            return microamps
        print("    Not a reading I can read. Try `40u`, `0.04 mA`, or blank.")


def self_test():
    assert reset_meaning("rst:0x5 (DSLEEP),boot:0x28").startswith("a wake")
    assert "brownout" in reset_meaning("rst:0x1 (POWERON),boot:0x8")
    assert reset_meaning("teddiebox: alive 3") is None
    assert reset_meaning("rst:0xff (NOPE)") == "an unrecognised reset reason"

    assert parse_current_ua("40") == 40.0
    assert parse_current_ua("40u") == 40.0
    assert parse_current_ua(" 40 µA ") == 40.0
    assert parse_current_ua("0.04 mA") == 40.0
    assert parse_current_ua("12m") == 12_000.0
    assert parse_current_ua("") is None
    assert parse_current_ua("about forty") is None

    assert "what good looks like" in verdict_for(40)
    assert "Stop here" in verdict_for(12_000)
    assert verdict_for(None) == "not measured"

    assert SLEEPING.search("teddiebox: sleeping — press an ear to wake")
    assert NOT_ARMED.search("teddiebox: sleep not armed — the wake line was never handed over")
    assert BANNER.search("teddiebox: dl rb | t wav taf play")
    print("self-test ok")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default="/dev/ttyUSB0")
    ap.add_argument("--out", help="where to write the record, for pasting into the handover")
    ap.add_argument("--log", default="/tmp/sleep-check.log", help="raw console capture")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        self_test()
        return 0
    if not args.out:
        print("--out is required outside --self-test", file=sys.stderr)
        return 2

    print(__doc__.split("\n\n")[0])
    print(
        "\nBefore anything: the box must be flashed with a firmware that has "
        "`sleep`, the\nport must belong to nobody else, and the meter must be "
        "in series with the pack."
    )

    fd = open_port(args.port)
    record = {}
    try:
        with open(args.log, "w") as log:
            print("\n--- listening. Nothing should be needed here yet.")
            read_lines(fd, 2.0, log)

            record["awake_ua"] = ask_current("The current with the box awake and idle")

            print("\n--- sending `sleep`.")
            os.write(fd, b"sleep\r")
            said = read_lines(fd, 15.0, log)
            refused = next((found for found in map(NOT_ARMED.search, said) if found), None)
            if refused:
                print(f"\n!!! The box refused to sleep: {refused.group(1)}")
                print("!!! Nothing below is worth measuring until that is fixed.")
                record["refused"] = refused.group(1)
            elif not any(SLEEPING.search(line) for line in said):
                print("\n!!! The box never said it was sleeping. Is this firmware current?")
                record["refused"] = "the box never said it was sleeping"
            else:
                record["sleep_ua"] = ask_current("The current with the box asleep")
                print(f"    {verdict_for(record['sleep_ua'])}")
                record["gpio47_mv"] = ask(
                    "GPIO47 while asleep, in millivolts — it is active low, so a "
                    "float low\n    brings the storage rail back up under a "
                    "sleeping box (blank to skip)"
                )

            print("\n--- press an ear now. Waiting up to 60 s for the box to come back.")
            woke = read_lines(fd, 60.0, log)
            reasons = [meaning for meaning in map(reset_meaning, woke) if meaning]
            record["reset"] = reasons[0] if reasons else "the box said nothing"
            record["banner"] = any(BANNER.search(line) for line in woke)
    finally:
        os.close(fd)

    lines = [
        "## Deep sleep, measured",
        "",
        f"Taken {time.strftime('%Y-%m-%d %H:%M')}, on `{os.popen('git rev-parse --short HEAD').read().strip()}`.",
        "",
        f"- Awake and idle: {record.get('awake_ua', 'not measured')} µA",
    ]
    if "refused" in record:
        lines.append(f"- **The box refused to sleep**: {record['refused']}")
    else:
        lines += [
            f"- Asleep: {record.get('sleep_ua', 'not measured')} µA — {verdict_for(record.get('sleep_ua'))}",
            f"- GPIO47 while asleep: {record.get('gpio47_mv') or 'not measured'} mV",
        ]
    lines += [
        f"- The wake: {record['reset']}",
        f"- The box came back up: {'yes' if record.get('banner') else 'no banner seen'}",
        "",
        f"Raw console capture: `{args.log}`.",
    ]
    text = "\n".join(lines) + "\n"
    with open(args.out, "w") as out:
        out.write(text)
    print("\n" + text)
    print(f"--- written to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
