# PIC32CM PL10 support in probe-rs

This document explains the code added to support the Microchip PIC32CM PL10 family (the
PIC32CMxxxxPL10xxx devices). It first explains the parts of probe-rs that this work touches, then the
device's peculiarities, then what was added and why, and finally the hardware tests in this
directory.

## Branches

The work is split into three stacked branches on top of `master`:

| Branch                       | Contents                                                     | Upstream? |
|------------------------------|--------------------------------------------------------------|-----------|
| `arm-chip-info-access-port`  | `ArmChipInfo` carries the AP the ROM table was found on      | yes       |
| `pic32cm-pl10`                 | DSU DID detection, target YAML, ROM table re-read, sequence  | yes       |
| `pic32cm-pl10-hil`             | This document and the hardware-in-the-loop scripts           | no        |

`pic32cm-pl10` is based on `arm-chip-info-access-port`, and `pic32cm-pl10-hil` on `pic32cm-pl10`.
The original, unsquashed history is kept in `pic32cm-pl-detection-backup`.

Commits on `pic32cm-pl10`:

1. *Detect Microchip devices by the PNMID/PNDID DSU DID* (`probe-rs-target`, `target-gen`,
   `vendor/microchip/mod.rs`)
2. *Add the Microchip PIC32CM PL10 target family* (`probe-rs/targets/PIC32CM_PL10.yaml`)
3. *Read the ROM table ID twice for ARM chip detection* (`architecture/arm`)
4. *Add the PIC32CM PL10 debug sequence* (`vendor/microchip/sequences/pic32cm_pl10.rs`, plus the
   detection changes that need the sequence)

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
embedded in the YAML. It has a little vendor-specific knowledge, for example it fills in the
`AtsamDsu` detection info for Atmel parts.

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
  the next step (the boot ROM handshake below) flushes explicitly.

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

The PIC32CM PL10 is a Cortex-M0+ (ARMv6-M) with flash at `0x0C00_0000` and SRAM at
`0x2000_0000`. Its debug-related peculiarities all come from its **boot ROM** and its
**Device Service Unit (DSU)**:

- **DSU DID layout.** Older Atmel SAM parts have a DSU DID register at a fixed address with
  PROCESSOR/FAMILY/SERIES/DEVSEL fields, which probe-rs already handles (`AtsamDsu`). The
  PIC32CM PL10 has its DSU elsewhere (DID at `0x4100_2120`), with a different layout:
  `VER[31:28] PNMID[27:20] PNDID[19:12] MANID[11:1] FV[0]`. PNMID identifies the family/mask,
  PNDID the variant. Its ROM table is designed by Microchip (JEP106 `0x29`), not Atmel (`0x1f`).

- **A boot ROM runs before the application, on every reset.** It talks to a debugger through
  the DSU's two *Boot Communication Channels*: the debugger writes commands to `BCC0`, the boot
  ROM answers on `BCC1`, and `STATUSB.BCCD0/1` say whether a channel holds unread data.

- **Reset extensions.** If SWCLK is held low while nRESET is released, the CPU stays in *CPU
  reset extension* (`STATUSA.CRSTEXT`) until the debugger clears it. The boot ROM then runs.
  If the debugger sends `EXIT`, the boot ROM answers `BOOT_OK` and enters **park mode**, where
  the core can be halted. It then waits for the debugger to release the *boot ROM reset
  extension* (`STATUSA.BREXT`) before it validates and starts the application.

- **Vector catch does not work.** The "first instruction after reset" is in the boot ROM,
  which the debugger can't stop in a useful place. To halt at the application's reset handler,
  a breakpoint has to be placed there instead.

- **Debug Access Level (DAL).** `DSU.DAL` is 0, 1 or 2. At DAL 2 everything is accessible. At
  DAL 0 only the DSU is; the core's debug registers (`DHCSR`, ...) and memory fault. The
  configured level is stored in flash (ROMCFG); the boot ROM applies it.

- **The device drops to DAL 0 when there is no valid application.** When the boot ROM can't
  start the application (erased flash, invalid reset vector), or the application faults, the
  device ends up at DAL 0. With an erased flash, the boot ROM resets the device **in a loop**;
  each reset lasts a few milliseconds, during which reads return 0, and the access level is
  only briefly restored on each boot. Running the core while it is parked with BREXT still set
  also restarts the boot ROM, with the same result.

- **Recovering.** Resetting into CPU reset extension and going to park mode restores the
  configured DAL. If the configured DAL is 0, the device is *locked*: only a chip erase
  (the boot ROM's `CE_ALL` command, answered with a challenge/response) unlocks it.

- **Erasing.** At DAL 2 the NVM controller can erase the flash (`CHER` command). A chip erase
  can clear the ROMCFG word holding the DAL 2 setting, which then has to be rewritten.

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

- **`target-gen/src/generate.rs`**: Microchip packs ship an ATDF file per device
  (`atdf/<DEVICE>.atdf`, XML), which the `.pdsc` doesn't reference. For each device,
  `read_microchip_dsu_did` finds that file in the pack (`Kind::find_file_ending_with`, works for
  zip archives and unpacked directories), and `parse_microchip_dsu_did` (using `roxmltree`)
  extracts:
  - the DSU base address from the `DSU` module instance,
  - the DID register offset from the `DSU` module definition, only if the register has `PNMID`
    and `PNDID` bitfields (so ATSAM-style DIDs are skipped),
  - the expected DID value from the `SIGNATURES/DSU_DID` property.

  `add_microchip_dsu_did_detection` merges devices with the same DID address and PNMID into one
  detection entry, and warns on duplicate PNDIDs. Two unit tests cover the parser.

- **`probe-rs/src/vendor/microchip/mod.rs`**: `try_detect_arm_chip` now also accepts ROM tables
  with the Microchip JEP106 code, and first tries every `MicrochipDsuDid` entry in the registry:
  it reads the DID at each distinct `did_address` (cached), checks `FV` and `PNMID`, and looks
  up `PNDID`. This runs *before* the existing ATSAM check, because the fixed ATSAM DID address
  may hold an unrelated register on these devices.

#### 2. The target family (`probe-rs/targets/PIC32CM_PL10.yaml`)

Generated with `target-gen` from `Microchip.PIC32CM-PL_DFP` 1.6.499, with the manufacturer set
to Microchip (`0x29`). 11 variants (32/64/128 KiB flash, 20 to 64 pins), each with the main
flash, BOOTCFG and ROMCFG flash algorithms, and the `MicrochipDsuDid` detection entry generated
by the code above.

#### 3. Read the ROM table ID twice (`architecture/arm/communication_interface.rs`)

`read_chip_info_from_rom_table` used to parse the whole ROM table (`Component::try_parse`).
Now it only reads the component ID (`ComponentInformationReader::read_all`, made
`pub(crate)`), and only trusts it once two reads, 10 ms apart, agree (up to 10 attempts).
`ComponentId::is_rom_table` was added for the class check.

Reason: on a PIC32CM PL10 in the boot ROM's reset loop, a read that lands during a reset returns
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
| `reset_into_extension`      | Pulse nRESET while clocking SWCLK and leaving it low → CRSTEXT. |
| `reset_and_reconnect`       | The above + `interface.reinitialize()` + a new memory interface.|
| `exit_reset_extension`      | Clear `STATUSA.CRSTEXT`, so the boot ROM starts.                |
| `reset_to_park`             | Exit CRSTEXT, send `EXIT`, wait `BOOT_OK`, read DAL; if DAL ≠ 0 halt the core (`DHCSR.C_HALT`). Returns DAL. |
| `wait_boot_rom_status`      | Poll `BCC1` for an expected status, fail on `0xEEEExxxx` errors. |
| `application_reset_handler` | Read the reset vector at `0x0C00_0004`; `None` if erased or not a Thumb address in flash. |
| `idle_in_sram`              | Write `b .; b .` at `0x2000_0000`, set PRIMASK, set PC there, release BREXT. |
| `halt_in_flash`             | Set an FPB breakpoint on the reset handler, release BREXT, run, wait up to 10 s for halt; else halt anywhere. Uses `idle_in_sram` if there is no valid application. |
| `boot_from_reset_extension` | `reset_to_park`, then `halt_in_flash` if a halt was requested, else run the application (or the idle loop). Fails if DAL is 0 (locked). |
| `is_core_accessible`        | DAL ≠ 0 *and* `DHCSR` can be read.                              |
| `restore_access`            | Reset to park mode; if DAL ≠ 0, either release BREXT (valid app) or idle in SRAM (no app). Returns DAL. Also used by detection. |
| `erase_with_nvmctrl`        | NVMCTRL `CHER` (needs DAL 2), then rewrite ROMCFG DAL 2 if it was erased. |
| `erase_with_boot_rom`       | Boot ROM interactive mode (`IMODE`), `CE_ALL`, answer the challenge with zeros (no keys set), wait for `OK`. |
| `chip_erase`                | Erase via NVMCTRL at DAL 2, else via the boot ROM; then check DAL is 2. |

**Why the idle loop.** Without a valid application, leaving park mode (resuming the core, or
ending the session, which runs `debug_core_stop`) makes the boot ROM reset the device into the
DAL 0 reset loop, and the next probe-rs command can't access the core. So instead of leaving
the core parked in the boot ROM, the sequence points it at a two-instruction loop in SRAM with
interrupts masked, and releases BREXT. The core can then be halted, resumed, have RAM written
and read, and a later reset (after flashing an application) starts the application normally.
The trade-off is that it overwrites the first word of SRAM.

Trait methods:

- **`debug_device_unlock`** (every attach):
  - core accessible and a valid application → nothing to do;
  - core accessible but no valid application → `restore_access` (reset to park + idle loop),
    so the session starts from a known, stable state;
  - core not accessible but `CRSTEXT` set → we are attaching under reset; nothing to do here,
    `reset_hardware_deassert` will do the handshake;
  - core not accessible → `restore_access`, warning if there was an application (it probably
    faulted).
  If DAL is still 0 after that, the device is locked: with the `erase_all` permission it is
  chip-erased, otherwise a `MissingPermissions` error explains how to unlock it.

- **`reset_hardware_assert`**: `reset_into_extension`. nRESET is already released when it
  returns; the CPU stays in CRSTEXT.
- **`reset_hardware_deassert`**: `boot_from_reset_extension`.
- **`reset_catch_set`**: only sets `halt_after_reset`, no vector catch.
- **`reset_catch_clear`**: clears the flag, and disables the FPB / clears comparator 0 that
  `halt_in_flash` used.
- **`reset_system`**: `reset_and_reconnect` + `boot_from_reset_extension`. This is a full
  nRESET, not `AIRCR.SYSRESETREQ`, because the boot ROM handshake needs CRSTEXT.
- **`debug_core_stop`**: if the default fails *and* DAL reads 0, ignore the error. When the
  core resumes from park mode, the boot ROM restarts and the device can drop to DAL 0 at any
  moment, making the `DHCSR` write fail even though nothing is wrong.
- **`debug_erase_sequence` / `erase_all`**: `chip_erase`, then `ReAttachRequired`, since the
  chip was reset.

**Detection changes (in `microchip/mod.rs`, same commit).** At DAL 0 the DSU DID reads PNDID as
0, which would identify every locked or looping device as variant `0x0`
(`PIC32CM6408PL10028`). So `detect_by_dsu_did`:

1. reads the DID with `read_dsu_did`, which retries a read of 0 up to 5 times, 10 ms apart
   (the reset loop);
2. if the match is a PIC32CM PL10 and its core is not accessible, calls
   `Pic32cmPl10::restore_access` to reset into park mode (which restores the DAL), then retries
   detection once with `restore_access = false`.

Note that auto-detection runs with the *default* sequence, so this calls the PIC32CM PL10 code
directly; it is the only way to get a correct PNDID out of a device that is at DAL 0.

---

## Part 4: review notes

Things worth knowing, or raising during upstream review:

- **Detection resets the device.** Auto-detecting a PIC32CM PL10 whose core is not accessible
  resets it into park mode. Other vendor detection code is read-only. This is needed to read a
  correct PNDID, but it is a side effect of e.g. `probe-rs info` without `--chip`.
- **Locked devices are guessed.** If the device is locked (configured DAL 0), PNDID stays 0
  and detection returns `PIC32CM6408PL10028` (PNDID `0x0`), which may be the wrong size.
  Erase works on any variant, so this only matters for flashing afterwards.
- **Detection of an erased device is flaky.** About 1 in 40 power-on detections fail in the
  Boot ROM's reset loop (see commit 3). The hardware test retries a failed detection.
- **Generic ROM table change.** Commit 3 changes ARM detection for every chip: it adds at
  least 10 ms per AP, and no longer parses the ROM table. It could be its own PR, stacked on
  `arm-chip-info-access-port`; it has no changelog fragment, since it mainly fixes detection of
  the (unreleased) PIC32CM PL10.
- **Hard-coded AP in `erase_all`.** `DebugEraseSequence::erase_all` uses
  `FullyQualifiedApAddress::v1_with_default_dp(0)`, as the trait doesn't pass an AP. It is
  correct for this family (single AP 0), but contrasts with the first branch.
- **SRAM clobbering.** `idle_in_sram` overwrites `0x2000_0000` when there is no application.
  If a user writes RAM there and then resets, the idle loop is rewritten.
- **`answer_challenge` busy-polls** `STATUSB` without sleeping (bounded by a 1 s timeout).
- **`debug_core_stop`** returns the DAL read error, not the original error, if reading DAL also
  fails.
- **FPB comparator 0** is used for the reset-handler breakpoint and cleared afterwards. probe-rs
  marks hardware breakpoints as disabled after any reset, so this does not conflict with user
  breakpoints.

---

## Part 5: hardware tests (this directory, not upstreamed)

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
  shows the state the device is really in, which probe-rs hides by resetting into park mode as
  soon as it connects. Its nRESET pulse is also what `run.py` uses to emulate a power cycle.
