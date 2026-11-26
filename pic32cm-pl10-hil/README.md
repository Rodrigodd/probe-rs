# PIC32CM PL10 support in probe-rs

This document explains the code added to support the Microchip PIC32CM PL10 family (the
PIC32CMxxxxPL10xxx devices). It first explains the parts of probe-rs that this work touches,
then the device's peculiarities, then what was added and why, and finally the hardware tests
in this directory.

## Branches

The work is split into stacked branches on top of `master`:

| Branch                      | Contents                                                     | Upstream? |
|-----------------------------|--------------------------------------------------------------|-----------|
| `arm-chip-info-access-port` | `ArmChipInfo` carries the AP the ROM table was found on      | yes       |
| `pic32cm-pl10`              | DSU DID detection, target YAML, ROM table re-read, sequence  | yes       |
| `pic32cm-pl10-hil`          | This document and the hardware-in-the-loop scripts           | no        |
| `pic32cm-pl10-standalone`   | `pic32cm-pl10` rebased directly on `master`, using AP 0      | yes, alternative |
| `microchip-dsu-did-target-gen` | `target-gen` generating `MicrochipDsuDid` from ATDF files | maybe, later |

`pic32cm-pl10` is based on `arm-chip-info-access-port`, and `pic32cm-pl10-hil` on
`pic32cm-pl10`. `pic32cm-pl10-standalone` has the same commits as `pic32cm-pl10`, without the
access port change: Microchip detection uses the hard-coded AP 0 of `master`.
`microchip-dsu-did-target-gen` is based on the first commit of `pic32cm-pl10`, and adds one
commit with the `target-gen` code (see below).
The original, unsquashed history is kept in `pic32cm-pl-detection`.

Commits on `pic32cm-pl10`:

1. *Detect Microchip devices by the PNMID/PNDID DSU DID* (`probe-rs-target`,
   `vendor/microchip/mod.rs`)
2. *Add the Microchip PIC32CM PL10 target family* (`probe-rs/targets/PIC32CM_PL10.yaml`)
3. *Read the ROM table ID twice for ARM chip detection* (`architecture/arm`)
4. *Add the PIC32CM PL10 debug sequence* (`vendor/microchip/sequences/pic32cm_pl10.rs`, plus the
   retry of DSU DID reads of 0)
5. *Restore debug access of a PIC32CM PL10 during chip detection* (`vendor/microchip/mod.rs`).
   Kept separate so that it can be dropped if upstream does not want detection to reset the
   device.

---

## Part 1: the relevant parts of probe-rs

### Workspace layout

Only a few crates matter here:

- **`probe-rs-target`**: the *data model* of a target, deserialized from YAML. `ChipFamily`,
  `Chip` (a variant), memory regions, flash algorithms, and `ChipDetectionMethod`
  (`probe-rs-target/src/chip_detection.rs`), which tells auto-detection how to identify each
  variant of a family.
- **`probe-rs`**: the library. Probes, architectures (ARM, RISC-V, Xtensa), sessions,
  flashing, and vendor-specific code.
- **`target-gen`**: a tool that generates the target YAML files from CMSIS device family packs
  (`.pack` / DFP zip files).
- **`probe-rs-tools`**: the `probe-rs` CLI (`download`, `read`, `erase`, `info`, `reset`, ...),
  which the hardware tests drive.

### Targets and the registry

Every supported chip is described by a YAML file in `probe-rs/targets/`. Each file is one
`ChipFamily` with a list of `variants` (`Chip`): their cores, memory map, and flash algorithms.
Those files are built into the binary, and loaded into a `Registry`
(`probe-rs/src/config/registry.rs`), which can look up a target by name
(`get_target_by_name`) or by the JEP106 code/part number of an ARM ROM table
(`get_target_by_chip_info`).

A family can also have a `chip_detection` list. Each entry is a `ChipDetectionMethod`, an enum
with one variant per vendor-specific detection scheme (`AtsamDsu`, `NordicConfigId`,
`InfineonXmcScu`, ...). An entry holds the register address/values to compare, and a map from
an ID value to a variant name. The vendor code in `probe-rs` reads the register and looks the
value up in the registry.

`target-gen` creates these files from a CMSIS pack: it reads the `.pdsc`, generates one variant
per device, and turns the pack's flash algorithm ELF files into position-independent blobs
embedded in the YAML. It has almost no vendor-specific knowledge (only a table mapping pack
vendor names to JEP106 codes), and never fills in `chip_detection`: every detection entry in
`probe-rs/targets/` is written by hand.

### From probe to memory: the ARM debug interface

On ARM, the layers are:

```
Probe (CMSIS-DAP, J-Link, ST-Link, ...)
  └─ DebugPortWire / SwdSequence      raw SWD/JTAG pins and sequences
     └─ ArmDebugInterface              DP and AP register access
        └─ Debug Port (DP)            selected by a DpAddress (Default, or multidrop)
           └─ Access Port (AP)        FullyQualifiedApAddress = DP + AP index/address
              └─ ArmMemoryInterface   32-bit memory reads/writes through a MEM-AP
```

- `ArmDebugInterface` (`architecture/arm/communication_interface.rs`) owns the probe and the DP
  state. `memory_interface(&ap)` returns a `Box<dyn ArmMemoryInterface>` borrowing it, so only
  one can exist at a time. `reinitialize()` reconnects to the DP, for example after the target
  was reset and the DP went down with it.
- `FullyQualifiedApAddress` identifies an AP *on a specific DP*. Before this work, much vendor
  detection code built `FullyQualifiedApAddress::v1_with_default_dp(0)`, AP 0 on the default
  DP, regardless of where the chip was actually found.
- Writes can be buffered; `flush()` sends them. Code that must know a write took effect before
  the next step (the Boot ROM handshake below) flushes explicitly.

### Debug sequences

Many chips need non-standard steps to connect, reset, halt or erase. probe-rs models these as
**debug sequences**, after the CMSIS-Pack "debug sequence" concept. For ARM, it is the
`ArmDebugSequence` trait (`architecture/arm/sequences.rs`). Each method has a default that
follows the ARM/CMSIS default behaviour, and a vendor overrides the ones it needs. The ones this
work overrides:

| Method                     | When it runs                                                      |
|----------------------------|-------------------------------------------------------------------|
| `debug_device_unlock`      | Every ARM attach, after the DP is up, before cores are touched.   |
| `reset_hardware_assert`    | Attach *under reset*: drive nRESET before connecting.             |
| `reset_hardware_deassert`  | Attach under reset: release reset, after `reset_catch_set`.       |
| `reset_catch_set`/`_clear` | Before/after a reset that must halt the core at the reset vector. |
| `reset_system`             | `Core::reset()` and `Core::reset_and_halt()`.                     |
| `debug_core_stop`          | When the `Session` is dropped, to deconfigure core debug.         |
| `debug_erase_sequence`     | Returns a `DebugEraseSequence` that erases the chip without a flash algorithm. |

A sequence is an `Arc<dyn ArmDebugSequence>` shared by the session, so state kept between calls
(like "halt after the next reset") has to use interior mutability (`AtomicBool` here).

`reset_and_halt` on an ARMv6-M core (`architecture/arm/core/armv6m.rs`) is:
`reset_catch_set` → `reset_system` → wait for halt → `reset_catch_clear`. The default
`reset_catch_set` enables *vector catch* (`DEMCR.VC_CORERESET`), so the core halts on the first
instruction after reset.

Some errors have special meaning: `ArmError::ReAttachRequired` from `debug_device_unlock` or
from `DebugEraseSequence::erase_all` makes the session reconnect to the target, which is needed
after a sequence reset the chip.

### Vendors

`probe-rs/src/vendor/` has one module per vendor, each implementing the `Vendor` trait
(`vendor/mod.rs`):

- `try_create_debug_sequence(&Chip)`: return the vendor's sequence for a chip, usually by
  matching the chip name. This is how a target gets its `debug_sequence`.
- `try_detect_arm_chip(registry, interface, ArmChipInfo)`: identify the exact variant, given
  the JEP106 manufacturer and part number read from the ROM table.

### The ARM attach flow

`Session::new` (`probe-rs/src/session.rs`) first picks the `Target`:

- With `--chip NAME`, it is looked up in the registry.
- Without it, **auto-detection** runs (`vendor::auto_determine_target`). For ARM,
  `try_detect_arm_chip` opens an `ArmDebugInterface` with the **default** sequence (the chip is
  not known yet, so its sequence cannot be used), and calls `read_chip_info_from_rom_table`:
  for each AP on the DP, it reads the CoreSight component ID at the AP's base address, and if
  it is a class 1 ROM table with a JEP106 code, returns `ArmChipInfo { manufacturer, part }`.
  Each vendor's `try_detect_arm_chip` then gets a chance to identify the variant; otherwise the
  registry is searched by manufacturer/part.

Then `attach_arm_debug_interface` attaches with the target's sequence
(see also `doc/attach_flow_arm.md`): optionally `reset_hardware_assert`, connect to the DP,
`debug_device_unlock`, and, under reset, `reset_catch_set` + `reset_hardware_deassert`. The
`Permissions` passed in say whether the sequence may erase the chip to unlock it
(`permissions.erase_all()`, the CLI's `--allow-erase-all`).

When the `Session` is dropped, it clears hardware breakpoints and calls `debug_core_stop` on
each core, which by default clears `DHCSR.C_DEBUGEN` so the core runs freely.

### Flashing and erasing

Flashing uses the flash algorithms from the target YAML: probe-rs loads the algorithm into RAM
and calls it on the core. A full chip erase prefers the sequence's `DebugEraseSequence` if the
sequence provides one (`flashing/erase.rs`, `Session::sequence_erase_all`), since it can erase
things a flash algorithm can't, such as a locked device.

---

## Part 2: what makes the PIC32CM PL10 different

The datasheet calls the family **PIC32CM PL10** (DS40002667B, which covers the
PIC32CM6408PL10028/032/048/064); Microchip's DFP for it is named PIC32CM-PL. It is a Cortex-M0+
(ARMv6-M) with one MEM-AP (AHB-AP 0), the Program Flash Memory (PFM) at `0x0C00_0000`, the
configuration flash (ROMCFG at `0x0D00_0000`, SIGNATURE, BOOTCFG) after it, and SRAM at
`0x2000_0000`. Its debug-related peculiarities all come from its **Boot ROM** and its
**Device Service Unit (DSU)** at `0x4100_2000`. Section numbers below refer to the datasheet.

- **DSU DID layout** (18.6.8). Older Atmel SAM parts have a DSU DID register at a fixed address
  with PROCESSOR/FAMILY/SERIES/DEVSEL fields, which probe-rs already handles (`AtsamDsu`). The
  PIC32CM PL10 DID is at `0x4100_2120` (DSU offset `0x120`), with a different layout:
  `VER[31:28] PNMID[27:20] PNDID[19:12] MANID[11:1] FV[0]`. VER is the silicon revision (0 =
  A1). The 16-bit part number is `DID[27:12]`: PNMID (*Part Number Mask ID*) is its upper byte,
  `0xBA` for this family, and PNDID (*Part Number Device ID*) its lower byte, which selects the
  variant (6.5.3). MANID is the JEDEC code `0x29`, and FV is always 1, so the low 12 bits read
  `0x053`. The CoreSight ROM table has JEP106 `0x29` (Microchip, not Atmel's `0x1f`) and part
  number `0xCD0` (18.4.3.3.1). (18.4.3.3.2 still lists the SAM processor/family/series/devsel
  fields, which do not exist on this DID.)

- **A Boot ROM runs before the application, on every reset** (6.4.2). While it runs, the DAL is
  0 and the DAP security filter blocks debugger accesses; only when the boot completes
  (`STATUS_BOOTOK`) is the DAL set to its final value (Figure 6-3, Table 6-9, 18.4.3.2). It talks
  to a debugger through the DSU's two *Boot ROM Communication Channels* (6.5.6.2.2, 18.6.6): the
  debugger writes commands to `BCC0`, the Boot ROM answers on `BCC1`, and `STATUSB.BCCD0/1`
  (*dirty* bits, set on write, cleared on read) say whether a channel holds unread data.

- **Cold-plugging and reset extensions** (6.5.4.4, 18.4.5.2). Driving at least three SWCLK
  rising edges while RESET is asserted makes the DSU detect a debugger (*cold-plugging*) and
  set `STATUSA.CRSTEXT0` (*CPU0 Reset Extension*, bit 8) and `STATUSA.BREXT0` (*Boot ROM 0 Phase
  Extension*, bit 16). The CPU is held in reset until the debugger writes 1 to `CRSTEXT0`; the
  Boot ROM then initializes the device and waits for a command (5 ms is the recommended wait,
  6.5.6.3). If the debugger sends `CMD_EXIT` while `BREXT0` is set, the Boot ROM answers
  `STATUS_BOOTOK` and enters **Park mode**, where the core sleeps in the Boot ROM and the
  debugger can safely program the device (6.5.6.3, Table 6-7). Once `BREXT0` is cleared, the Boot
  ROM applies the user settings and starts the application (Figure 6-4). `CMD_IMODE` instead
  enters **Interactive mode (IMODE)**, where the Boot ROM executes commands such as `CMD_CE_ALL`
  (6.5.6.4). A boot error with a debugger connected also enters IMODE (6.5.6.1.2). Hot-plugging
  (connecting without a reset) is not available at DAL 0 (6.5.5.4.3, Table 18-1).

- **Vector catch does not work** (6.5.4.2). Reset vector catch cannot stop the CPU at the
  first instruction of the application (the *First Mutable Executable*, FME); at DAL 0 it halts
  the CPU on the first instruction of the Boot ROM. The datasheet uses Park mode instead. To
  halt at the application's reset handler, a breakpoint has to be placed there.

- **Debug Access Level (DAL)** (6.5.5.4.1, 18.6.9). The datasheet also calls it *Debugger*
  Access Level (DSU chapter) and *Device* Access Level (ROMCFG). The current level is in
  `DSU.DAL.CPU0[1:0]` (`0x4100_2124`): `0x0` SECURED or `0x2` FULL_DEBUG; `0x1` and `0x3` are
  reserved. At DAL 0 the debugger can only access the DSU external registers (offsets
  `0x100`–`0x1FF`) and the DSU CoreSight ROM table; other accesses are blocked, so the core's
  debug registers (`DHCSR`, ...) and memory fault. The configured level comes from two ROMCFG
  fuses (5.4, Table 5-2): DAL 2 is only in effect when `DAL2` (`0x0D00_0000`) is `0xFFFF_FF55`
  and `DAL0` (`0x0D00_0010`) is `0xFFFF_FFFF`; an erased `DAL2` fuse means DAL 0.

- **Observed on hardware, not in the datasheet** (see Part 6 for how it was measured):
  - *Reset loop without a valid application.* With an erased flash, the Boot ROM completes the
    boot (`STATUS_BOOTOK`), the CPU faults on the erased vector table, and the device resets,
    about every 72 ms. As the DAL is 0 while the Boot ROM runs, the configured DAL is only in
    effect for a short time on each boot, and the core is practically never accessible. Reads
    that land during a reset return 0. An invalid reset vector, or an application that faults,
    has the same effect.
  - *The DID reads PNDID 0 in that loop.* While the Boot ROM runs a normal boot, the DID reads
    `0x0BA00053` (PNDID 0), and sometimes 0. After a cold-plug, the DID reads correctly
    (`0x0BA02053` on a PIC32CM6408PL10048), even at DAL 0.
  - *Halting the core in Park mode, then resuming it, restarts the Boot ROM.* It posts a new
    `STATUS_INITCHECK_OK`, drops to DAL 0, and waits for a command, with `BREXT0` still set.
    Resuming the core without halting it first does not.
  - *`CHER` erases ROMCFG.* The `DAL2` fuse reads `0xFFFF_FFFF` after it, so the device is at
    DAL 0 after the next reset, unless the fuse is rewritten with `WROMCFG`.
  - *`CMD_IMODE` gets no answer in Interactive mode.* Sent a second time, it is consumed
    (`BCCD0` clears) but nothing is posted on `BCC1`.

- **Recovering.** A cold-plug reset into Park mode lets the Boot ROM complete the boot and
  apply the configured DAL (`STATUS_BOOTOK`). If the configured DAL is 0, the device is
  *locked*: only `CMD_CE_ALL` in Interactive mode unlocks it (6.4.3.1), unless that command was
  disabled through the `KEYVAL_CE_ALL` fuse (6.4.3.3, PDID). `CMD_CE_ALL` uses a
  challenge-response: the Boot ROM sends `STATUS_CHALLENGE`, 128 bits of challenge, and waits
  for 256 bits of response, which the PIC32CM PL10 does not check (6.5.6.4.3).

- **Erasing.** `CMD_CE_ALL` erases the PFM and ROMCFG (not SIGNATURE or BOOTCFG), clears the
  volatile memories, and programs DAL 2; a reset is needed afterwards (Table 6-16). At DAL 2 the
  NVM controller can also erase the PFM with its `CHER` command (`NVMCTRL.CTRLB` with the
  `0xA5` CMDEX key, 26.6). The datasheet recommends this, from Park mode, when `CMD_CE_ALL` is
  locked. ROMCFG can only be written with `WROMCFG` (5.4). The datasheet does not say that
  `CHER` also erases ROMCFG, but on hardware it does, so the sequence rewrites the `DAL2` fuse
  afterwards.

The sequence follows Microchip's MPLAB programming scripts in the DFP, which differ from the
CMSIS sequences in the same pack.

---

## Part 3: what was added

### Branch `arm-chip-info-access-port`

*Pass the ROM table's access port to vendor ARM chip detection.*

`ArmChipInfo` gained an `access_port: FullyQualifiedApAddress` field, set by
`read_chip_info_from_rom_table` to the AP whose ROM table identified the chip. It no longer
implements `Copy` (`FullyQualifiedApAddress` isn't `Copy`), hence the `.clone()` in
`vendor/mod.rs`.

The Microchip, Nordic, Holtek, Renesas, Infineon and TridentIoT detection code now use that AP
(and its DP, `access_port.dp()`, for `TARGETID` reads) instead of a hard-coded AP 0 on the
default DP, or `interface.current_debug_port()`. This removes several `FIXME: This is a bit
shaky` comments.

This is independent of the PIC32CM PL10, but the PIC32CM PL10 detection builds on it: its
`restore_access` needs the AP to reset and reconnect on.

### Branch `pic32cm-pl10`

#### 1. DSU DID detection (`MicrochipDsuDid`)

- **`probe-rs-target/src/chip_detection.rs`**: new `ChipDetectionMethod::MicrochipDsuDid`
  holding a `MicrochipDsuDidDetection { did_address, pnmid, variants: PNDID → name }`. The DID
  address is per family, so it is in the data, not in code.

- **`probe-rs/src/vendor/microchip/mod.rs`**: `try_detect_arm_chip` now also accepts ROM tables
  with the Microchip JEP106 code, and first tries every `MicrochipDsuDid` entry in the registry:
  it reads the DID at each distinct `did_address` (cached), checks `FV` and `PNMID`, and looks
  up `PNDID`. This runs *before* the existing ATSAM check, because the fixed ATSAM DID address
  may hold an unrelated register on these devices.

#### 2. The target family (`probe-rs/targets/PIC32CM_PL10.yaml`)

Generated with `target-gen` from `Microchip.PIC32CM-PL_DFP` 1.6.499, with the manufacturer set
to Microchip (`0x29`). 11 variants (32/64/128 KiB flash, 20 to 64 pins), each with the main
flash, BOOTCFG and ROMCFG flash algorithms. The `MicrochipDsuDid` detection entry was added by
hand, like the detection entries of other families.

#### Not in `pic32cm-pl10`: generating the detection entry (`microchip-dsu-did-target-gen`)

The branch `microchip-dsu-did-target-gen` has a `target-gen` change that generates the entry
instead. It was split out because it would be the first chip detection generated by
`target-gen`, adds Microchip-specific parsing and a `roxmltree` dependency, and only serves one
family for now.

In `target-gen/src/generate.rs`: Microchip packs ship an ATDF file per device
(`atdf/<DEVICE>.atdf`, XML), which the `.pdsc` doesn't reference. For each device,
`read_microchip_dsu_did` finds that file in the pack (`Kind::find_file_ending_with`, works for
zip archives and unpacked directories), and `parse_microchip_dsu_did` (using `roxmltree`)
extracts:
- the DSU base address from the `DSU` module instance,
- the DID register offset from the `DSU` module definition, only if the register has `PNMID`
  and `PNDID` bitfields (so ATSAM-style DIDs are skipped),
- the expected DID value from the `SIGNATURES/DSU_DID` property.

`add_microchip_dsu_did_detection` merges devices with the same DID address and PNMID into one
detection entry, and warns on duplicate PNDIDs. Two unit tests cover the parser. The entry in
`PIC32CM_PL10.yaml` was originally generated with it, from `Microchip.PIC32CM-PL_DFP` 1.6.499.

#### 3. Read the ROM table ID twice (`architecture/arm/communication_interface.rs`)

`read_chip_info_from_rom_table` used to parse the whole ROM table (`Component::try_parse`).
Now it only reads the component ID (`ComponentInformationReader::read_all`, made
`pub(crate)`), and only trusts it once two reads, 10 ms apart, agree (up to 10 attempts).
`ComponentId::is_rom_table` was added for the class check.

Reason: on a PIC32CM PL10 in the Boot ROM's reset loop, a read that lands during a reset returns
0, so a single read could return a garbage or partial ID and the chip would not be detected.
Reading only the ID also avoids walking a ROM table whose entries may be inaccessible at DAL 0.

This is a generic change that affects detection of every ARM chip (at least one extra 10 ms
read per AP).

It does not cover every case: in some boots most reads lose a word, and the same wrong ID can
come back twice in a row (PID0 read as 0 gives part `0xC00` instead of `0xCD0`; a cleared PIDR1
or PIDR2 gives the wrong JEP106 code, or none). About 1 in 40 power-on detections of an erased
device fail that way, with "The connected chip could not automatically be determined"; running
the command again works. More elaborate merging of reads fixed it on this device, but was not
kept, as it could not be shown to be safe for every device. Use `--chip` to avoid it.

#### 4. The debug sequence (`vendor/microchip/sequences/pic32cm_pl10.rs`)

`Pic32cmPl10` implements `ArmDebugSequence` and `DebugEraseSequence`. `Microchip::
try_create_debug_sequence` returns it for chip names `PIC32CMxxxxPL...` (`is_pic32cm_pl10`).
`SwdSequenceShim` in `atsam.rs` was made `pub(crate)` to reuse it.

Building blocks (private functions):

| Function                    | What it does                                                    |
|-----------------------------|-----------------------------------------------------------------|
| `reset_into_extension`      | Cold-plug: assert nRESET, drive 3 SWCLK pulses, release nRESET with SWCLK low → `CRSTEXT0`/`BREXT0` set. |
| `reset_and_reconnect`       | The above + `interface.reinitialize()` + a new memory interface.|
| `exit_reset_extension`      | Write 1 to `STATUSA.CRSTEXT0` so the Boot ROM starts, wait 5 ms, and return the first status it posts (`STATUS_INITCHECK_OK` or an error), as in Table 6-5. |
| `reset_to_park`             | Exit CRSTEXT0, fail on a boot error, send `CMD_EXIT`, wait for `STATUS_BOOTOK`, read DAL; if DAL ≠ 0 halt the core (`DHCSR.C_HALT`). Returns DAL. |
| `wait_boot_rom_status`      | Wait for an expected status on `BCC1`; fail on `0xEEEExxxx` errors and on `STATUS_ARG_INVALID` (a command locked by its ROMCFG key value). |
| `application_reset_handler` | Read the reset vector at `0x0C00_0004`; `None` if erased or not a Thumb address in flash. |
| `idle_in_sram`              | Write `b .; b .` at `0x2000_0000`, set PRIMASK, set PC there, release BREXT0. |
| `halt_in_flash`             | Set an FPB breakpoint on the reset handler, release BREXT0, run, wait up to 1 s for halt; else halt anywhere. Uses `idle_in_sram` if there is no valid application. |
| `boot_from_reset_extension` | `reset_to_park`, then `halt_in_flash` if a halt was requested, else run the application (or the idle loop). Fails if DAL is 0 (locked). |
| `is_core_accessible`        | DAL ≠ 0 *and* `DHCSR` can be read.                              |
| `restore_access`            | Reset to Park mode; if DAL ≠ 0, either release BREXT0 (valid app) or idle in SRAM (no app). Returns DAL. Also used by detection (commit 5). |
| `erase_with_nvmctrl`        | NVMCTRL `CHER` (needs DAL 2), then rewrite ROMCFG DAL 2 if it was erased. |
| `erase_with_boot_rom`       | Enter Interactive mode (`CMD_IMODE` → `STATUS_CMD_VALID`; after a boot error, no answer within 100 ms also means it is already in Interactive mode), send `CMD_CE_ALL`, wait for `STATUS_CHALLENGE`, read the 4 challenge words, send 8 zero response words (not checked by this family), wait for `STATUS_CMD_VALID` then `STATUS_OK`. |
| `chip_erase`                | Erase via NVMCTRL at DAL 2, else via the Boot ROM; then check DAL is 2. |

**Why the idle loop.** probe-rs halts the core in Park mode, and resuming it from there
(`resume`, or ending the session, which runs `debug_core_stop`) restarts the Boot ROM at DAL 0.
Releasing `BREXT0` instead starts the application, but without a valid application the device
then enters the DAL 0 reset loop. Either way, the next probe-rs command can't access the core,
which is why both are only done with a valid application. So instead of leaving
the core parked in the Boot ROM, the sequence points it at a two-instruction loop in SRAM with
interrupts masked, and releases BREXT0. The core can then be halted, resumed, have RAM written
and read, and a later reset (after flashing an application) starts the application normally.
The trade-off is that it overwrites the first word of SRAM.

Trait methods:

- **`debug_device_unlock`** (every attach):
  - core accessible and a valid application → nothing to do;
  - core accessible but no valid application → `restore_access` (reset to park + idle loop),
    so the session starts from a known, stable state;
  - core not accessible but `CRSTEXT0` set → we are attaching under reset; nothing to do here,
    `reset_hardware_deassert` will do the handshake;
  - core not accessible → `restore_access`, warning if there was an application (it probably
    faulted).
  If DAL is still 0 after that, the device is locked: with the `erase_all` permission it is
  chip-erased, otherwise a `MissingPermissions` error explains how to unlock it.

- **`reset_hardware_assert`**: `reset_into_extension`. nRESET is already released when it
  returns; the CPU stays in CRSTEXT0.
- **`reset_hardware_deassert`**: `boot_from_reset_extension`.
- **`reset_catch_set`**: only sets `halt_after_reset`, no vector catch.
- **`reset_catch_clear`**: clears the flag, and disables the FPB / clears comparator 0 that
  `halt_in_flash` used.
- **`reset_system`**: `reset_and_reconnect` + `boot_from_reset_extension`. This is a full
  nRESET, not `AIRCR.SYSRESETREQ`, because the Boot ROM handshake needs CRSTEXT0.
- **`debug_core_stop`**: if the default fails *and* DAL reads 0, ignore the error. When the
  core resumes after it was halted in Park mode, the Boot ROM restarts and the device can drop
  to DAL 0 at any moment, making the `DHCSR` write fail even though nothing is wrong.
- **`debug_erase_sequence` / `erase_all`**: `chip_erase`, then `ReAttachRequired`, since the
  chip was reset.

**Detection change (in `microchip/mod.rs`, same commit).** `detect_by_dsu_did` reads the DID
with `read_dsu_did`, which retries a read of 0 up to 5 times, 10 ms apart, as a read during
one of the reset loop's resets returns 0.

The idle loop's register writes use the existing definitions in
`architecture/arm/core/registers/cortex_m.rs`: `PC`, and the `EXTRA` register (CONTROL and
PRIMASK, by name from `CORTEX_M_CORE_REGISTERS`, as `stm32n6.rs` does).

#### 5. Restore debug access during detection (`vendor/microchip/mod.rs`)

The DID is in the DSU external space, so it is readable at DAL 0, but in the reset loop it
reads PNDID as 0, which would identify every looping device as variant `0x0`
(`PIC32CM6408PL10028`). So when `detect_by_dsu_did` finds a PIC32CM PL10,
`Microchip::try_detect_arm_chip` calls `restore_pic32cm_pl10_access`, which, if the core is
not accessible:

1. calls `Pic32cmPl10::restore_access` (made `pub(crate)` here, with `is_core_accessible`) to
   reset into Park mode, which restores the DAL;
2. runs `detect_by_dsu_did` again.

If the device is locked (DAL still 0) or the reset fails, it keeps the variant read at DAL 0
and logs a warning; detection does not fail.

Auto-detection runs with the *default* sequence, so this calls the PIC32CM PL10 code directly;
it is the only way to get a correct PNDID out of a device that is at DAL 0. Without this
commit, an erased or looping device is detected as `PIC32CM6408PL10028`.

---

## Part 4: review notes

Things worth knowing, or raising during upstream review:

- **Detection resets the device** (commit 5). Auto-detecting a PIC32CM PL10 whose core is not
  accessible resets it into Park mode. Other vendor detection code is read-only. This is
  needed to read a correct PNDID, but it is a side effect of e.g. `probe-rs info` without
  `--chip`. The commit can be dropped if upstream objects.
- **Locked devices are guessed.** If the device is locked (configured DAL 0), PNDID stays 0
  and detection returns `PIC32CM6408PL10028` (PNDID `0x0`), which may be the wrong size, with
  a warning. Erase works on any variant, so this only matters for flashing afterwards.
- **Detection of an erased device is flaky.** About 1 in 40 power-on detections fail in the
  Boot ROM's reset loop (see commit 3). The hardware test retries a failed detection.
- **Generic ROM table change.** Commit 3 changes ARM detection for every chip: it adds at
  least 10 ms per AP, and no longer parses the ROM table. It could be its own PR, stacked on
  `arm-chip-info-access-port`; it has no changelog fragment, since it mainly fixes detection of
  the (unreleased) PIC32CM PL10.
- **Hard-coded AP in `erase_all`.** `DebugEraseSequence::erase_all` uses
  `FullyQualifiedApAddress::v1_with_default_dp(0)`, as the trait doesn't pass an AP. It is
  correct for this family (one MEM-AP, AHB-AP 0, datasheet 6.5.5.6.3), but contrasts with the
  first branch.
- **SRAM clobbering.** `idle_in_sram` overwrites `0x2000_0000` when there is no application.
  If a user writes RAM there and then resets, the idle loop is rewritten.
- **Untested paths.** A locked `CMD_CE_ALL` (`STATUS_ARG_INVALID`) and a boot error with a
  debugger connected were not reproduced on hardware: locking `CMD_CE_ALL` at DAL 0 can make
  the device unrecoverable, and the boot errors that can be provoked (e.g. a bad CRC table with
  `CRCBOOT`) live in BOOTCFG, which a chip erase does not clear. The code handles both readings
  of the datasheet for the boot error case.
- **Halting in Park mode is outside the datasheet's flow.** The datasheet says the core never
  needs to be halted in Park mode, but probe-rs needs a halted core, and halting it is what
  makes a later resume restart the Boot ROM (see Part 2).
- **`answer_challenge` busy-polls** `STATUSB` without sleeping (bounded by a 1 s timeout).
- **`debug_core_stop`** returns the DAL read error, not the original error, if reading DAL also
  fails.
- **FPB comparator 0** is used for the reset-handler breakpoint and cleared afterwards. probe-rs
  marks hardware breakpoints as disabled after any reset, so this does not conflict with user
  breakpoints.

---

## Part 5: datasheet errors and omissions

Found while checking the code against the *PIC32CM6408PL10028/032/048/064 Complete Data Sheet*,
DS40002667B, and on hardware (a PIC32CM6408PL10048 on a board with an nEDBG probe, 2026-11-25,
using `bootrom_probe.py`). Section numbers refer to that revision.

### Contradicted by hardware

1. **Interactive mode entry status.** 6.5.6.3: "Once the Boot ROM reports STATUS_OK, the
   device is in Interactive mode." The Boot ROM answers `CMD_IMODE` with `STATUS_CMD_VALID`
   only, as Table 6-6 says; no `STATUS_OK` follows.
2. **`CHER` also erases ROMCFG.** 26.4.2.3.4 and the `CTRLB.CMD` table (26.6) say `CHER`
   "erases the Flash". On hardware, at DAL 2 in Park mode, it also erases the ROMCFG `DAL2`
   fuse (`0xFFFF_FF55` → `0xFFFF_FFFF`), so the device is at DAL 0 after the next reset. This
   matters because 6.5.6.4.5 (Command CE_ALL) recommends `CHER` from Park mode when `CMD_CE_ALL` is
   locked: without rewriting `DAL2` with `WROMCFG` first, the device then comes back at DAL 0.
   (Whether `CHER` also erases the `KEYVAL_*` fuses was not checked: they were already erased.)
3. **Unrecognized commands.** 6.5.6.4.5: "If, for any reason, the Boot ROM does not properly
   recognize a command, it will return STATUS_CMD_INVALID instead of STATUS_CMD_VALID."
   `STATUS_CMD_INVALID` has no code in Table 6-9, and on hardware a second `CMD_IMODE` sent in
   Interactive mode is consumed (`STATUSB.BCCD0` clears) without any answer on `BCC1`.

### Contradictions within the datasheet

4. **Boot error with a debugger connected.** 6.4.2.2.2 ("A boot failure occurs and a debugger
   is connected") and 6.5.6.4.1 ("the device will automatically enter Interactive debug mode")
   say the Boot ROM enters Interactive mode by itself. 6.5.6.1.2 says it waits for a command:
   `CMD_IMODE` enters Interactive mode and `CMD_EXIT` resets the device.
5. **Cold-plugging order.** 18.4.2.3 steps 3–4 release RESET and then clock SWCLK. 18.4.5.2.1
   (Figure 18-4), 6.5.4.4 and Table 6-5 step 2 drive the SWCLK pulses while RESET is asserted,
   which is what works on hardware.
6. **DSU external address range.** `0x100`–`0x1FF` in 6.4.3.3, 6.5.5.4.1 (Table 6-3) and
   Figure 18-2; `0x100`–`0xFFF` in 6.5.5.4.2; `0x100`–`0x1FFF` in 18.4.2.1.
7. **Chip identification fields.** 18.4.3.3.2 says the DID provides processor, family, series
   and device select identification (the SAM D layout). The DID of this family (18.6.8) has
   VER, PNMID, PNDID, MANID and FV.
8. **DAL 1.** Figure 18-2 says the internal range cannot be accessed "in DAL 0 or 1", but DAL 1
   is reserved (Table 6-3, 18.6.9).
9. **Name of the DAL.** *Debug* Access Level (6.5.5.4.1, 5.4.1.1), *Debugger* Access Level
   (6.4.3.1, 6.5.2, 18.6.9) and *Device* Access Level (5.4, Table 5-2).
10. **`STATUS_ARG_AUTH`.** Used in Table 6-16, Figure 6-8 and the other command definitions,
    but it has no code in Table 6-9.

### Undocumented behaviour

11. **Halting the core in Park mode.** 6.5.6.3 says the core never needs to be halted through
    DHCSR in Park mode. If a debugger halts it anyway (generic Arm tools do, to use the core
    registers), resuming it restarts the Boot ROM: it posts `STATUS_INITCHECK_OK`, the DAL
    drops to 0, and it waits for a command. Resuming without halting first does not.
12. **No valid application.** 6.4.2.1 step 4 does not say what happens when the vector table
    is erased or invalid. On hardware the Boot ROM completes the boot (`STATUS_BOOTOK`), the
    CPU faults, and the device resets about every 72 ms, at DAL 0 nearly all the time. Only a
    cold-plug gets the debugger in.
13. **PNDID during boot.** 18.6.8 gives the DID reset value as `0x0BAXX053`, but not when PNDID
    becomes valid. During a normal boot (e.g. in the loop above) it reads 0 (`0x0BA00053`), so
    a tool identifying the variant then picks the wrong part. After a cold-plug it reads
    correctly, even at DAL 0.
14. **First status.** Table 6-5 step 6 says to read "the response code" after releasing the
    CPU reset extension, without naming it. It is `STATUS_INITCHECK_OK`, and if the debugger
    sends `CMD_EXIT` before reading it, it is overwritten by `STATUS_BOOTOK`.

### Typos

15. 6.5.6.3: "clear the CRSTEXT bit in the DSU.STATUS register" (the register is `STATUSA`, the
    bit `CRSTEXT0`). 6.5.4.4: "STAUSA". 18.4.3.1: "DCCO". 6.5.2: "diabled".

---

## Part 6: hardware tests (this directory, not upstreamed)

- **`run.py`** drives the `probe-rs` CLI against a connected board through every scenario the
  sequence handles: erased flash, running application, an image with an invalid reset vector,
  power-on reset with and without `--chip`, `reset`, `reset --pin`... It builds two tiny images
  itself (a "good" one that increments a counter at `0x2000_0100`, and a "bad" one with a
  non-Thumb, out-of-flash reset vector), so no firmware project is needed. A test fails on a
  non-zero exit code or any unexpected `WARN`/`ERROR` line, except that a failed
  auto-detection is retried up to 3 times (see commit 3). **It erases the flash.**

  ```sh
  cargo build -p probe-rs-tools --bin probe-rs
  python3 pic32cm-pl10-hil/run.py [--chip PIC32CM6408PL10048] [-k FILTER] [-v] [--restore app.elf]
  ```

- **`dsu_peek.py`** talks to a CMSIS-DAP v1 (HID) probe directly through `/dev/hidraw`, and
  reads only the DSU registers (and `DHCSR` when accessible) without resetting or halting. It
  shows the state the device is really in, which probe-rs hides by resetting into Park mode as
  soon as it connects. Its nRESET pulse is also what `run.py` uses to emulate a power cycle.

- **`bootrom_probe.py`** runs the raw Boot ROM experiments behind Part 2 and Part 5, through
  `dsu_peek.py`'s probe access: `first_status`, `exit_immediately`, `imode`, `peek`,
  `park_resume` and `cher_romcfg`, given as arguments. **`cher_romcfg` erases the flash and the
  `DAL2` fuse, leaving the device at DAL 0 after the next reset**; recover it with
  `probe-rs erase --allow-erase-all`.

  ```sh
  python3 pic32cm-pl10-hil/bootrom_probe.py first_status imode park_resume
  ```
