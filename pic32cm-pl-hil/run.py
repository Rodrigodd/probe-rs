#!/usr/bin/env python3
"""Hardware-in-the-loop tests for the PIC32CM-PL debug sequence.

Runs the probe-rs CLI against a connected PIC32CM-PL board and checks that it
stays accessible across erase, flash, reset and read, including with an erased
flash and with an image whose reset vector is invalid.

The test images are generated here, so no firmware project is needed:

- good: a loop that increments a counter at 0x20000100. The test checks that
  the counter changes to see that the application runs.
- bad: a vector table whose reset vector is not a Thumb address in flash, like
  an image built with its vector table stripped.

THE TESTS ERASE THE FLASH OF THE CONNECTED DEVICE.

Usage:
    cargo build -p probe-rs-tools --bin probe-rs
    python3 pic32cm-pl-hil/run.py [--chip NAME] [--probe-rs PATH]
                                  [--restore ELF] [-k FILTER] [-v]
"""

import argparse
import os
import re
import struct
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dsu_peek  # noqa: E402

FLASH = 0x0C00_0000
COUNTER = 0x2000_0100

# Warning printed when a connection finds the core inaccessible with an application in flash,
# which means the application faulted.
FAULT_WARNING = "the application may have faulted"
# Warning printed when the image in flash has an invalid reset vector.
BAD_VECTOR_WARNING = "is not a valid reset handler"
# Warnings expected while the device runs the bad image.
BAD_STATE = [FAULT_WARNING, BAD_VECTOR_WARNING]

ANSI = re.compile(r"\x1b\[[0-9;]*m")


def good_image() -> bytes:
    """Vector table, then: r0 = COUNTER; r1 = 0; loop { r1 += 1; *r0 = r1 }."""
    code = struct.pack(
        "<6HI",
        0x4802,  # 0x40: ldr r0, [pc, #8]   (literal at 0x4C)
        0x2100,  # 0x42: movs r1, #0
        0x3101,  # 0x44: adds r1, #1
        0x6001,  # 0x46: str r1, [r0]
        0xE7FC,  # 0x48: b 0x44
        0xE7FE,  # 0x4A: b .   (fault handler)
        COUNTER,  # 0x4C: literal
    )
    reset = FLASH + 0x40 + 1
    fault = FLASH + 0x4A + 1
    vectors = [0x2000_2000, reset] + [fault] * 14
    return struct.pack("<16I", *vectors) + code


def bad_image() -> bytes:
    """Reset vector 0x490A2000: bit 0 clear and outside of flash."""
    return struct.pack("<4I", 0x2000_2000, 0x490A_2000, 0xD202_4291, 0xD202_4291)


class Failure(Exception):
    pass


class Harness:
    def __init__(self, args, tmp):
        self.probe_rs = args.probe_rs
        self.chip = args.chip
        self.verbose = args.verbose
        self.images = {}
        for name, data in [("good", good_image()), ("bad", bad_image())]:
            path = os.path.join(tmp, f"{name}.bin")
            with open(path, "wb") as f:
                f.write(data)
            self.images[name] = path

    def run(self, *cmd, chip=True, timeout=120):
        argv = [self.probe_rs, *cmd]
        if chip:
            argv += ["--chip", self.chip]
        env = dict(os.environ, NO_COLOR="1")
        env.pop("RUST_LOG", None)
        proc = subprocess.run(argv, capture_output=True, text=True, env=env, timeout=timeout)
        out = ANSI.sub("", proc.stdout + proc.stderr)
        if self.verbose:
            print(f"    $ {' '.join(argv[1:])}  -> {proc.returncode}")
            for line in out.splitlines():
                print(f"      | {line}")
        return proc.returncode, out

    def check(self, *cmd, allow=(), chip=True):
        """Run a command that must succeed without warnings or errors, except those allowed."""
        code, out = self.run(*cmd, chip=chip)
        problems = [
            line
            for line in out.splitlines()
            if re.search(r"\b(WARN|ERROR)\b|^Error", line)
            and not any(a in line for a in allow)
        ]
        if code != 0 or problems:
            raise Failure(
                f"`probe-rs {' '.join(cmd)}` exited with {code}:\n"
                + "\n".join("      " + line for line in out.splitlines())
            )
        return out

    def read_words(self, address, count, allow=(), chip=True):
        out = self.check("read", "b32", hex(address), str(count), allow=allow, chip=chip)
        words = []
        for line in out.splitlines():
            m = re.match(r"^\s*[0-9a-f]+:\s+((?:[0-9a-f]{8}\s*)+)$", line.strip())
            if m:
                words += [int(w, 16) for w in m.group(1).split()]
        if len(words) != count:
            raise Failure(f"could not parse read output:\n{out}")
        return words

    # Device setup

    def power_on_reset(self):
        """Reset the device without the debugger, like a power cycle."""
        dap = dsu_peek.Dap(dsu_peek.find_hidraw())
        try:
            dap.pulse_reset()
        finally:
            dap.close()

    def erase(self):
        # The device may still run the bad image of an earlier test.
        self.check("erase", "--allow-erase-all", allow=BAD_STATE)

    def download(self, image, *extra, allow=()):
        self.check(
            "download",
            "--binary-format",
            "bin",
            "--base-address",
            hex(FLASH),
            self.images[image],
            *extra,
            allow=allow,
        )

    def flash_bad_image(self):
        """Flash the bad image. When the session ends, the boot ROM runs it, and it faults."""
        self.erase()
        self.download("bad", allow=[BAD_VECTOR_WARNING])

    # Assertions

    def assert_erased(self, allow=()):
        words = self.read_words(FLASH, 4, allow=allow)
        if words != [0xFFFF_FFFF] * 4:
            raise Failure(f"flash is not erased: {[hex(w) for w in words]}")

    def assert_good_image(self, allow=()):
        words = self.read_words(FLASH, 2, allow=allow)
        if words != [0x2000_2000, FLASH + 0x41]:
            raise Failure(f"good image not in flash: {[hex(w) for w in words]}")

    def assert_app_running(self, allow=()):
        """The good image's counter must change between reads."""
        first = self.read_words(COUNTER, 1, allow=allow)[0]
        time.sleep(0.05)
        second = self.read_words(COUNTER, 1)[0]
        if first == second:
            raise Failure(f"application is not running, counter stuck at {first:#x}")

    def assert_detected(self):
        out = self.check("info", chip=False)
        m = re.search(r"Detected chip: (\S+)", out)
        if not m or m.group(1) != self.chip:
            raise Failure(f"expected chip {self.chip}, got:\n{out}")


# Tests. Each one sets up the device state it needs.


def test_erase_from_running_app(h):
    h.erase()
    h.download("good", "--start")
    h.erase()
    h.assert_erased()


def test_erased_read_twice(h):
    h.erase()
    h.assert_erased()
    h.assert_erased()


def test_erased_info(h):
    h.erase()
    h.assert_detected()
    h.assert_detected()


def test_erased_reset(h):
    h.erase()
    h.check("reset")
    h.assert_erased()


def test_erased_reset_pin(h):
    h.erase()
    h.check("reset", "--method", "pin")
    h.assert_erased()


def test_erased_power_on_autodetect(h):
    # After a power cycle with an erased flash, the boot ROM resets the device in a loop, and
    # reads during each reset return 0. Detection and connection must still work, silently.
    h.erase()
    for _ in range(5):
        h.power_on_reset()
        out = h.check("read", "b32", hex(FLASH), "1", chip=False)
        if "ffffffff" not in out:
            raise Failure(f"unexpected read:\n{out}")
        h.assert_detected()


def test_erased_power_on_with_chip(h):
    h.erase()
    for _ in range(3):
        h.power_on_reset()
        h.assert_erased()


def test_running_app_power_on_autodetect(h):
    # Detection must not reset a running application.
    h.erase()
    h.download("good", "--start")
    h.power_on_reset()
    before = h.read_words(COUNTER, 1)[0]
    after = h.read_words(COUNTER, 1, chip=False)[0]
    if after <= before:
        raise Failure(f"the application was reset, counter {before:#x} -> {after:#x}")
    h.assert_app_running()


def test_erased_write_and_read_ram(h):
    h.erase()
    h.check("write", "b32", hex(0x2000_0400), "0x12345678")
    if h.read_words(0x2000_0400, 1) != [0x1234_5678]:
        raise Failure("RAM write did not read back")


def test_download_on_erased_runs_app(h):
    h.erase()
    h.download("good")
    h.assert_good_image()
    h.assert_app_running()


def test_download_start_runs_app(h):
    h.erase()
    h.download("good", "--start")
    h.assert_app_running()
    h.assert_app_running()


def test_running_app_info(h):
    h.erase()
    h.download("good", "--start")
    h.assert_detected()
    h.assert_app_running()


def test_running_app_reset(h):
    h.erase()
    h.download("good", "--start")
    h.check("reset")
    h.assert_app_running()


def test_running_app_reset_pin(h):
    h.erase()
    h.download("good", "--start")
    h.check("reset", "--method", "pin")
    h.assert_good_image()
    h.assert_app_running()


def test_redownload_over_running_app(h):
    h.erase()
    h.download("good", "--start")
    h.download("good", "--start")
    h.assert_app_running()


def test_bad_image_is_readable(h):
    h.flash_bad_image()
    words = h.read_words(FLASH, 2, allow=BAD_STATE)
    if words[1] != 0x490A_2000:
        raise Failure(f"bad image not in flash: {[hex(w) for w in words]}")
    # The first connection moved the core to the idle loop, the device stays accessible.
    h.read_words(FLASH, 2, allow=[BAD_VECTOR_WARNING])
    h.read_words(FLASH, 2, allow=[BAD_VECTOR_WARNING])


def test_bad_image_first_connect_repeated(h):
    # The faulting image makes the boot ROM restart in a loop, with the debug access level
    # briefly restored on each boot. Connecting must work wherever in that loop it lands.
    for _ in range(5):
        h.flash_bad_image()
        h.read_words(FLASH, 2, allow=BAD_STATE)


def test_bad_image_reflash_without_erase(h):
    h.flash_bad_image()
    h.download("good", "--start", allow=BAD_STATE)
    h.assert_good_image()
    h.assert_app_running()


def test_bad_image_erase(h):
    h.flash_bad_image()
    h.erase()
    h.assert_erased()
    h.download("good", "--start")
    h.assert_app_running()


TESTS = [v for k, v in list(globals().items()) if k.startswith("test_")]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--chip", default="PIC32CM6408PL10048")
    parser.add_argument("--probe-rs", default="target/debug/probe-rs", help="probe-rs binary to test")
    parser.add_argument("--restore", metavar="ELF", help="flash this ELF after the tests")
    parser.add_argument("-k", dest="filter", help="only run tests whose name contains this")
    parser.add_argument("-v", "--verbose", action="store_true", help="print every command and its output")
    args = parser.parse_args()

    tests = [t for t in TESTS if not args.filter or args.filter in t.__name__]
    failed = []
    with tempfile.TemporaryDirectory() as tmp:
        h = Harness(args, tmp)
        for test in tests:
            name = test.__name__[len("test_"):]
            start = time.monotonic()
            try:
                test(h)
                print(f"PASS {name} ({time.monotonic() - start:.1f}s)")
            except (Failure, subprocess.TimeoutExpired) as e:
                print(f"FAIL {name} ({time.monotonic() - start:.1f}s)\n    {e}")
                failed.append(name)

        if args.restore:
            code, out = h.run("download", "--start", args.restore)
            print(f"restored {args.restore}" if code == 0 else f"restore failed:\n{out}")

    print(f"\n{len(tests) - len(failed)}/{len(tests)} passed")
    if failed:
        print("failed: " + ", ".join(failed))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
