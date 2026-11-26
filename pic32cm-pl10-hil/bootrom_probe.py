#!/usr/bin/env python3
"""Raw experiments on the PIC32CM PL10 Boot ROM handshake, through CMSIS-DAP HID."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dsu_peek
from dsu_peek import Dap

STATUSA, STATUSB, BCC0, BCC1, DID, DAL = 0x41002104, 0x41002108, 0x41002110, 0x41002114, 0x41002120, 0x41002124
DHCSR = 0xE000EDF0
CMD_EXIT, CMD_IMODE, CMD_CE_ALL, CMD_READCFM = 0x444247AA, 0x44424755, 0x444247E3, 0x4442474C
NAMES = {1: "CRC_FAIL", 2: "CRC_OK", 3: "INITCHECK_OK", 4: "BOOTOK", 5: "CMD_VALID", 6: "ARG_INVALID",
         8: "DATA_VALID", 9: "OK", 0xB: "CHALLENGE"}


def name(v):
    if isinstance(v, int):
        return NAMES.get(v, f"{v:#010x}")
    return v


class D(Dap):
    def write32(self, address, value):
        self.transfer(0x05, address)
        ack, _ = self.transfer(0x0D, value)
        if ack != 1:
            self.transfer(0x00, 0x1E)
        return ack

    def pins(self, out, sel):
        self.cmd(0x10, out, sel, 0, 0, 0, 0)

    def cold_plug(self):
        self.cmd(0x02, 1)
        self.pins(0x00, 0x80)  # nRESET low
        time.sleep(0.05)
        for _ in range(3):
            self.pins(0x00, 0x01)
            self.pins(0x01, 0x01)
        self.pins(0x00, 0x01)  # SWCLK low
        self.pins(0x80, 0x80)  # nRESET high
        time.sleep(0.05)
        return self.connect()

    def statuses(self, timeout=0.5):
        """Collect every BCC1 value posted within the timeout."""
        out = []
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            sb = self.read32(STATUSB)
            if isinstance(sb, int) and sb & 2:
                out.append(self.read32(BCC1))
            time.sleep(0.001)
        return out

    def regs(self):
        return {k: (lambda v: f"{v:#010x}" if isinstance(v, int) else v)(self.read32(a))
                for k, a in [("STATUSA", STATUSA), ("STATUSB", STATUSB), ("DAL", DAL), ("DID", DID), ("DHCSR", DHCSR)]}


def release_crstext(d):
    d.write32(STATUSA, 1 << 8)


def exp_first_status(d):
    print("== E1: status posted after releasing CRSTEXT0, before any command")
    d.cold_plug()
    print("  after cold plug:", d.regs())
    release_crstext(d)
    t0 = time.monotonic()
    st = d.statuses(0.5)
    print(f"  BCC1 values within 500 ms: {[name(s) for s in st]}")
    print("  regs:", d.regs())
    print("== E2: CMD_EXIT after that")
    d.write32(BCC0, CMD_EXIT)
    print(f"  BCC1 values: {[name(s) for s in d.statuses(0.5)]}")
    print("  regs:", d.regs())


def exp_exit_immediately(d):
    print("== E1b: CMD_EXIT written immediately after releasing CRSTEXT0 (what the code does)")
    d.cold_plug()
    release_crstext(d)
    time.sleep(0.005)
    d.write32(BCC0, CMD_EXIT)
    print(f"  BCC1 values: {[name(s) for s in d.statuses(0.5)]}")
    print("  regs:", d.regs())


def exp_imode(d):
    print("== E3: CMD_IMODE, then CMD_IMODE again, then CMD_READCFM-less CMD_EXIT")
    d.cold_plug()
    release_crstext(d)
    time.sleep(0.005)
    print(f"  initial: {[name(s) for s in d.statuses(0.2)]}")
    d.write32(BCC0, CMD_IMODE)
    print(f"  after CMD_IMODE: {[name(s) for s in d.statuses(0.5)]}")
    d.write32(BCC0, CMD_IMODE)
    print(f"  after 2nd CMD_IMODE: {[name(s) for s in d.statuses(0.5)]}")
    print("  regs:", d.regs())
    d.write32(BCC0, CMD_EXIT)
    print(f"  after CMD_EXIT in IMODE: {[name(s) for s in d.statuses(0.5)]}")
    time.sleep(0.2)
    try:
        d.connect()
    except Exception as e:
        print("  reconnect:", e)
    print("  regs:", d.regs())


def exp_peek(d, samples=10, period=0.05):
    print(f"== peek without reset ({samples} samples)")
    d.connect()
    for _ in range(samples):
        print("  ", d.regs())
        time.sleep(period)


def exp_park_resume(d):
    print("== E5: Park mode with BREXT0 set, then resume the core")
    d.cold_plug()
    release_crstext(d)
    time.sleep(0.005)
    d.write32(BCC0, CMD_EXIT)
    print(f"  {[name(s) for s in d.statuses(0.3)]}")
    print("  parked:", d.regs())
    d.write32(DHCSR, 0xA05F0001)  # C_DEBUGEN, not halted
    time.sleep(0.01)
    for _ in range(8):
        print("  after resume:", d.regs())
        time.sleep(0.05)


def exp_cher_romcfg(d):
    print("== E6: CHER in Park mode at DAL 2: is ROMCFG erased?")
    d.cold_plug()
    release_crstext(d)
    time.sleep(0.005)
    d.write32(BCC0, CMD_EXIT)
    print(f"  {[name(s) for s in d.statuses(0.3)]}  DAL {d.read32(DAL)}")
    d.write32(DHCSR, 0xA05F0003)
    rom = lambda: [f"{d.read32(0x0D000000 + o):#010x}" for o in (0x00, 0x10, 0x18, 0x20, 0x28)]
    print("  ROMCFG DAL2/DAL0/KEYVAL_CRC/KEYVAL_CE_ALL/KEYVAL_SDAL0 before:", rom())
    print("  flash[0..2] before:", [f"{d.read32(0x0C000000 + o):#010x}" for o in (0, 4)])
    d.write32(0x41004014, 2)  # clear INTFLAG.ERROR
    d.write32(0x41004004, 0xA521)  # CHER
    end = time.monotonic() + 5
    while not (d.read32(0x41004014) & 1) and time.monotonic() < end:
        time.sleep(0.005)
    print(f"  INTFLAG {d.read32(0x41004014):#x} STATUS {d.read32(0x4100401C):#x}")
    d.write32(0x41004004, 0xA501)
    print("  ROMCFG after:", rom())
    print("  flash[0..2] after:", [f"{d.read32(0x0C000000 + o):#010x}" for o in (0, 4)])


EXPS = {f.__name__[4:]: f for f in [exp_first_status, exp_exit_immediately, exp_imode, exp_peek,
                                    exp_park_resume, exp_cher_romcfg]}

if __name__ == "__main__":
    d = D(dsu_peek.find_hidraw())
    try:
        for e in sys.argv[1:]:
            EXPS[e](d)
    finally:
        d.close()
