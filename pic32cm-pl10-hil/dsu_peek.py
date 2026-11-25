#!/usr/bin/env python3
"""Read the PIC32CM PL10 DSU registers without resetting or halting the device.

probe-rs resets the device into the boot ROM as soon as it connects at debug access level 0,
which hides the state the device was in. This script talks to a CMSIS-DAP v1 (HID) probe
through /dev/hidraw directly, and only reads registers that are accessible at any debug access
level, plus DHCSR when it is accessible.

Usage:
    python3 pic32cm-pl10-hil/dsu_peek.py [--hidraw /dev/hidrawN] [--samples N]
"""

import argparse
import glob
import os
import time

DSU_STATUSA = 0x4100_2104
DSU_STATUSB = 0x4100_2108
DSU_BCC1 = 0x4100_2114
DSU_DAL = 0x4100_2124
DHCSR = 0xE000_EDF0

ACK = {1: "OK", 2: "WAIT", 4: "FAULT"}


def find_hidraw():
    for path in sorted(glob.glob("/sys/class/hidraw/hidraw*")):
        with open(os.path.join(path, "device", "uevent")) as f:
            if "CMSIS-DAP" in f.read():
                return "/dev/" + os.path.basename(path)
    raise SystemExit("no CMSIS-DAP HID probe found")


class Dap:
    def __init__(self, path):
        self.fd = os.open(path, os.O_RDWR)

    def cmd(self, *data):
        os.write(self.fd, bytes([0]) + bytes(data) + bytes(64 - len(data)))
        resp = os.read(self.fd, 64)
        if resp[0] != data[0]:
            raise RuntimeError(f"bad response to command {data[0]:#x}: {resp[:4].hex()}")
        return resp

    def transfer(self, request, value=0):
        """One SWD transfer. Returns (ack, data)."""
        resp = self.cmd(0x05, 0, 1, request, *value.to_bytes(4, "little"))
        ack = resp[2] & 0x7
        data = int.from_bytes(resp[3:7], "little") if request & 0x2 and ack == 1 else None
        return ack, data

    def connect(self):
        self.cmd(0x02, 1)  # DAP_Connect: SWD
        self.cmd(0x11, *(1_000_000).to_bytes(4, "little"))  # DAP_SWJ_Clock: 1 MHz
        self.cmd(0x04, 0, 100, 0, 0, 0)  # DAP_TransferConfigure
        self.cmd(0x13, 0)  # DAP_SWD_Configure
        # Line reset, JTAG-to-SWD, line reset, idle. The device may not answer while the
        # boot ROM resets it, so retry for a while.
        seq = [0xFF] * 7 + [0x9E, 0xE7] + [0xFF] * 7 + [0x00]
        deadline = time.monotonic() + 2
        while True:
            self.cmd(0x12, len(seq) * 8, *seq)
            ack, idcode = self.transfer(0x02)  # DP IDCODE
            if ack == 1:
                break
            if time.monotonic() > deadline:
                raise RuntimeError(f"no answer from the target: {ACK.get(ack, f'ack {ack}')}")
            time.sleep(0.001)
        self.transfer(0x00, 0x1E)  # DP ABORT: clear errors
        self.transfer(0x04, 0x5000_0000)  # DP CTRL/STAT: power up
        self.transfer(0x08, 0)  # DP SELECT: AP 0, bank 0
        self.transfer(0x01, 0x2300_0002)  # AP CSW: 32-bit
        return idcode

    def read32(self, address):
        self.transfer(0x05, address)  # AP TAR
        ack, data = self.transfer(0x0F)  # AP DRW
        if ack != 1:
            self.transfer(0x00, 0x1E)
            return ACK.get(ack, f"ack {ack}")
        return data

    def pulse_reset(self):
        """Pulse nRESET with SWCLK high, so the device boots without the debugger, as after a
        power cycle."""
        self.cmd(0x02, 1)  # DAP_Connect: SWD
        self.cmd(0x10, 0x81, 0x81, 0, 0, 0, 0)  # DAP_SWJ_Pins: SWCLK and nRESET high
        self.cmd(0x10, 0x01, 0x81, 0, 0, 0, 0)  # nRESET low
        time.sleep(0.02)
        self.cmd(0x10, 0x81, 0x81, 0, 0, 0, 0)  # nRESET high
        time.sleep(0.2)

    def close(self):
        self.cmd(0x03)  # DAP_Disconnect
        os.close(self.fd)


def fmt(value):
    return f"{value:#010x}" if isinstance(value, int) else value


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--hidraw", help="probe device, found automatically by default")
    parser.add_argument("--samples", type=int, default=5, help="how many times to read")
    args = parser.parse_args()

    dap = Dap(args.hidraw or find_hidraw())
    try:
        print(f"DP IDCODE {dap.connect():#010x}")
        for _ in range(args.samples):
            statusa = dap.read32(DSU_STATUSA)
            statusb = dap.read32(DSU_STATUSB)
            # Reading BCC1 acknowledges it, so only read it when the boot ROM posted a value.
            bcc1 = dap.read32(DSU_BCC1) if isinstance(statusb, int) and statusb & 0x2 else "-"
            dal = dap.read32(DSU_DAL)
            dhcsr = dap.read32(DHCSR)
            print(
                f"DAL {fmt(dal)}  STATUSA {fmt(statusa)}  STATUSB {fmt(statusb)}  "
                f"BCC1 {fmt(bcc1)}  DHCSR {fmt(dhcsr)}"
            )
            time.sleep(0.2)
    finally:
        dap.close()


if __name__ == "__main__":
    main()
