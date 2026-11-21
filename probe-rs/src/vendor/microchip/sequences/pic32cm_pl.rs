//! Sequences for the PIC32CM-PL target family.
//!
//! These devices run a boot ROM after every reset. When a debugger is present, the boot ROM
//! talks to it through the DSU Boot Communication Channels (BCC). The debugger resets the device
//! into CPU reset extension, releases the CPU, and then tells the boot ROM to enter park mode,
//! where the core can be halted. The boot ROM then waits for the boot ROM reset extension (BREXT)
//! to be released before it starts the application.
//!
//! The sequences here follow the MPLAB programming scripts in the Microchip PIC32CM-PL device
//! family pack, which differ from the CMSIS sequences in the same pack.

use crate::{
    MemoryMappedRegister, Permissions,
    architecture::arm::{
        ArmDebugInterface, ArmError, FullyQualifiedApAddress, Pins,
        communication_interface::SwdSequence,
        core::armv6m::{BpCompx, BpCtrl, Dhcsr},
        memory::ArmMemoryInterface,
        sequences::{ArmDebugSequence, ArmDebugSequenceError, DebugEraseSequence},
        traits::DebugPortWire,
    },
    session::MissingPermissions,
    vendor::microchip::sequences::atsam::SwdSequenceShim,
};
use probe_rs_target::CoreType;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// DSU Status A register.
const DSU_STATUSA: u64 = 0x4100_2104;
/// DSU Status B register.
const DSU_STATUSB: u64 = 0x4100_2108;
/// DSU Boot Communication Channel 0, written by the debugger.
const DSU_BCC0: u64 = 0x4100_2110;
/// DSU Boot Communication Channel 1, written by the boot ROM.
const DSU_BCC1: u64 = 0x4100_2114;
/// DSU Debug Access Level register.
const DSU_DAL: u64 = 0x4100_2124;

/// DSU.STATUSA: CPU reset phase extension.
const STATUSA_CRSTEXT: u32 = 1 << 8;
/// DSU.STATUSA: boot ROM reset extension.
const STATUSA_BREXT: u32 = 1 << 16;
/// DSU.STATUSB: BCC0 holds a value that the boot ROM has not read yet.
const STATUSB_BCCD0: u32 = 1 << 0;
/// DSU.STATUSB: BCC1 holds a value that the debugger has not read yet.
const STATUSB_BCCD1: u32 = 1 << 1;

/// Debugger commands to the boot ROM.
const CMD_EXIT: u32 = 0x4442_47AA;
const CMD_IMODE: u32 = 0x4442_4755;
const CMD_CE_ALL: u32 = 0x4442_47E3;

/// Boot ROM statuses.
const STATUS_BOOT_OK: u32 = 0x4;
const STATUS_CMD_VALID: u32 = 0x5;
const STATUS_OK: u32 = 0x9;
const STATUS_CHALLENGE: u32 = 0xB;
/// Boot ROM statuses with these upper bits report an error, with the code in the low byte.
const STATUS_ERROR_MASK: u32 = 0xFFFF_0000;
const STATUS_ERROR: u32 = 0xEEEE_0000;

/// NVMCTRL registers.
const NVMCTRL_CTRLB: u64 = 0x4100_4004;
const NVMCTRL_INTFLAG: u64 = 0x4100_4014;
const NVMCTRL_STATUS: u64 = 0x4100_401C;
/// NVMCTRL.INTFLAG bits.
const INTFLAG_READY: u32 = 1 << 0;
const INTFLAG_ERROR: u32 = 1 << 1;
/// NVMCTRL.CTRLB commands, with the execution key.
const NVM_CMD_NOOP: u32 = 0xA501;
const NVM_CMD_WROMCFG: u32 = 0xA520;
const NVM_CMD_CHER: u32 = 0xA521;

/// ROMCFG word holding the DAL 2 setting, and its erased and default values.
const ROMCFG_DAL2: u64 = 0x0D00_0000;
const ROMCFG_DAL2_DEFAULT: u32 = 0xFFFF_FF55;

/// Address of the reset vector of the application in flash.
const RESET_VECTOR: u64 = 0x0C00_0004;

/// Flash range of the largest device in the family. A reset handler outside of it is not valid.
const FLASH_RANGE: std::ops::Range<u32> = 0x0C00_0000..0x0C02_0000;

/// How long to wait for the boot ROM to answer a command.
const BOOT_ROM_TIMEOUT: Duration = Duration::from_secs(1);
/// How long to wait for the boot ROM to erase the chip.
const BOOT_ROM_ERASE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the NVM controller to finish a command.
const NVM_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the core to reach the reset handler after leaving park mode. The boot
/// ROM validates the application first, which can take a while.
const HALT_IN_FLASH_TIMEOUT: Duration = Duration::from_secs(10);

/// Debug sequences for the Microchip PIC32CM-PL family.
#[derive(Debug, Default)]
pub struct Pic32cmPl {
    /// Whether the core must halt at the reset handler after the next reset.
    ///
    /// Vector catch does not work on these devices, because the boot ROM runs first. The halt
    /// is done with a breakpoint on the reset handler instead.
    halt_after_reset: AtomicBool,
}

fn sequence_error(message: impl Into<String>) -> ArmError {
    ArmDebugSequenceError::SequenceSpecific(message.into().into()).into()
}

impl Pic32cmPl {
    /// Create the sequencer for the PIC32CM-PL family.
    pub fn create() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reset the device with nRESET, holding SWCLK low as nRESET is released, so the CPU
    /// stays in CPU reset extension.
    fn reset_into_extension(interface: &mut dyn SwdSequence) -> Result<(), ArmError> {
        let mut n_reset = Pins(0);
        n_reset.set_nreset(true);
        let n_reset = n_reset.0 as u32;

        let mut swclk = Pins(0);
        swclk.set_swclk_tck(true);
        let swclk = swclk.0 as u32;

        interface.swj_pins(0, n_reset, 0)?;
        thread::sleep(Duration::from_millis(50));

        // Clock a few cycles, and end with SWCLK low.
        for _ in 0..3 {
            interface.swj_pins(0, swclk, 0)?;
            interface.swj_pins(swclk, swclk, 0)?;
        }
        interface.swj_pins(0, swclk, 0)?;

        interface.swj_pins(n_reset, n_reset, 0)?;
        thread::sleep(Duration::from_millis(50));

        Ok(())
    }

    /// Reset the device into CPU reset extension, and reconnect to it.
    ///
    /// The reset takes the debug port down with it, and invalidates any memory interface that
    /// was opened before, so this returns a new one.
    fn reset_and_reconnect<'i>(
        interface: &'i mut dyn ArmDebugInterface,
        ap: &FullyQualifiedApAddress,
    ) -> Result<Box<dyn ArmMemoryInterface + 'i>, ArmError> {
        Self::reset_into_extension(interface)?;
        interface.reinitialize()?;
        interface.memory_interface(ap)
    }

    /// Wait until the boot ROM reports `expected` on BCC1.
    fn wait_boot_rom_status(
        memory: &mut dyn ArmMemoryInterface,
        expected: u32,
        timeout: Duration,
    ) -> Result<(), ArmError> {
        let start = Instant::now();
        loop {
            if memory.read_word_32(DSU_STATUSB)? & STATUSB_BCCD1 != 0 {
                let status = memory.read_word_32(DSU_BCC1)?;
                if status == expected {
                    return Ok(());
                }
                if status & STATUS_ERROR_MASK == STATUS_ERROR {
                    return Err(sequence_error(format!(
                        "The boot ROM reported error {:#04x}",
                        status & 0xFF
                    )));
                }
                tracing::debug!(
                    "PIC32CM-PL: boot ROM status {status:#x}, waiting for {expected:#x}"
                );
            }

            if start.elapsed() >= timeout {
                return Err(sequence_error(format!(
                    "Timeout waiting for boot ROM status {expected:#x}"
                )));
            }

            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Release the CPU from CPU reset extension, so the boot ROM starts.
    fn exit_reset_extension(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        let statusa = memory.read_word_32(DSU_STATUSA)?;
        if statusa & STATUSA_CRSTEXT == 0 {
            tracing::warn!("PIC32CM-PL: the device did not enter CPU reset extension");
        }
        memory.write_word_32(DSU_STATUSA, STATUSA_CRSTEXT)?;
        memory.flush()?;

        // Give the boot ROM time to start.
        thread::sleep(Duration::from_millis(5));
        Ok(())
    }

    /// Release the CPU from CPU reset extension, and put the boot ROM into park mode. Then halt
    /// the core, unless debug access is disabled.
    ///
    /// Returns the debug access level.
    fn reset_to_park(memory: &mut dyn ArmMemoryInterface) -> Result<u32, ArmError> {
        Self::exit_reset_extension(memory)?;

        memory.write_word_32(DSU_BCC0, CMD_EXIT)?;
        memory.flush()?;
        Self::wait_boot_rom_status(memory, STATUS_BOOT_OK, BOOT_ROM_TIMEOUT)?;

        let dal = memory.read_word_32(DSU_DAL)? & 0x3;
        tracing::debug!("PIC32CM-PL: DAL = {dal}");
        if dal != 0 {
            let mut dhcsr = Dhcsr(0);
            dhcsr.enable_write();
            dhcsr.set_c_debugen(true);
            dhcsr.set_c_halt(true);
            memory.write_word_32(Dhcsr::get_mmio_address(), dhcsr.into())?;
            memory.flush()?;
        }

        Ok(dal)
    }

    /// Release the CPU from CPU reset extension, and put the boot ROM into interactive mode,
    /// where it accepts commands.
    fn reset_to_interactive_mode(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        Self::exit_reset_extension(memory)?;

        // After an error, the boot ROM is already in interactive mode.
        if memory.read_word_32(DSU_STATUSB)? & STATUSB_BCCD1 != 0 {
            let status = memory.read_word_32(DSU_BCC1)?;
            if status & STATUS_ERROR_MASK == STATUS_ERROR {
                tracing::warn!("PIC32CM-PL: boot ROM error {:#04x}", status & 0xFF);
                return Ok(());
            }
        }

        memory.write_word_32(DSU_BCC0, CMD_IMODE)?;
        memory.flush()?;
        Self::wait_boot_rom_status(memory, STATUS_CMD_VALID, BOOT_ROM_TIMEOUT)
    }

    /// Answer the boot ROM's challenge. The keys are not set, so the response is all zeros.
    fn answer_challenge(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        for _ in 0..4 {
            let start = Instant::now();
            while memory.read_word_32(DSU_STATUSB)? & STATUSB_BCCD1 == 0 {
                if start.elapsed() >= BOOT_ROM_TIMEOUT {
                    return Err(sequence_error("The boot ROM did not send a full challenge"));
                }
            }
            memory.read_word_32(DSU_BCC1)?;
        }
        for _ in 0..8 {
            let start = Instant::now();
            while memory.read_word_32(DSU_STATUSB)? & STATUSB_BCCD0 != 0 {
                if start.elapsed() >= BOOT_ROM_TIMEOUT {
                    return Err(sequence_error("The boot ROM did not accept the response"));
                }
            }
            memory.write_word_32(DSU_BCC0, 0)?;
            memory.flush()?;
        }
        Ok(())
    }

    /// Release the boot ROM reset extension, so the boot ROM leaves park mode and starts the
    /// application once the core runs.
    fn release_brext(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        memory.write_word_32(DSU_STATUSA, STATUSA_BREXT)?;
        memory.flush()?;
        Ok(())
    }

    /// Run the core.
    fn run(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        let mut dhcsr = Dhcsr(0);
        dhcsr.enable_write();
        dhcsr.set_c_debugen(true);
        memory.write_word_32(Dhcsr::get_mmio_address(), dhcsr.into())?;
        memory.flush()?;
        Ok(())
    }

    /// Read the application's reset handler, if the application can be started.
    ///
    /// Returns `None` if the flash is erased, or if the reset vector is not a Thumb address in
    /// flash. Leaving park mode then makes the device drop to debug access level 0: with an
    /// erased flash, the boot ROM resets the device in a loop, and with an invalid reset vector,
    /// the core faults on its first instruction.
    fn application_reset_handler(
        memory: &mut dyn ArmMemoryInterface,
    ) -> Result<Option<u32>, ArmError> {
        let reset_handler = memory.read_word_32(RESET_VECTOR)?;
        if reset_handler == 0xFFFF_FFFF {
            tracing::debug!("PIC32CM-PL: the flash is erased");
            return Ok(None);
        }
        if reset_handler & 1 == 0 || !FLASH_RANGE.contains(&(reset_handler & !1)) {
            tracing::warn!(
                "PIC32CM-PL: the reset vector {reset_handler:#010x} is not a valid reset handler"
            );
            return Ok(None);
        }
        Ok(Some(reset_handler))
    }

    /// Leave park mode with a breakpoint on the application's reset handler, and wait for the
    /// core to halt there. If it does not, halt it wherever it is.
    ///
    /// If there is no application that can be started, stay halted in park mode. The breakpoint
    /// could not stop an invalid reset handler either, as breakpoints only match in the code
    /// region.
    fn halt_in_flash(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        let Some(reset_handler) = Self::application_reset_handler(memory)? else {
            tracing::debug!("PIC32CM-PL: no application to start, staying halted in the boot ROM");
            return Ok(());
        };
        tracing::debug!("PIC32CM-PL: reset handler at {reset_handler:#010x}");

        let mut bp_ctrl = BpCtrl(0);
        bp_ctrl.set_key(true);
        bp_ctrl.set_enable(true);
        memory.write_word_32(BpCtrl::get_mmio_address(), bp_ctrl.into())?;

        let mut bp_comp = BpCompx(0);
        bp_comp.set_comp((reset_handler >> 2) & 0x7FF_FFFF);
        bp_comp.set_bp_match(if reset_handler & 0x2 == 0 { 0b01 } else { 0b10 });
        bp_comp.set_enable(true);
        memory.write_word_32(BpCompx::get_mmio_address(), bp_comp.into())?;

        Self::release_brext(memory)?;
        Self::run(memory)?;

        let start = Instant::now();
        while start.elapsed() < HALT_IN_FLASH_TIMEOUT {
            // Reads can fail while the boot ROM runs.
            match memory.read_word_32(Dhcsr::get_mmio_address()) {
                Ok(dhcsr) if Dhcsr(dhcsr).s_halt() => {
                    tracing::debug!(
                        "PIC32CM-PL: halted at the reset handler after {:?}",
                        start.elapsed()
                    );
                    return Ok(());
                }
                Ok(_) => {}
                Err(error) => tracing::debug!("PIC32CM-PL: reading DHCSR failed: {error}"),
            }
            thread::sleep(Duration::from_millis(10));
        }

        tracing::warn!("PIC32CM-PL: the core did not halt at the reset handler, halting it");
        let mut dhcsr = Dhcsr(0);
        dhcsr.enable_write();
        dhcsr.set_c_debugen(true);
        dhcsr.set_c_halt(true);
        memory.write_word_32(Dhcsr::get_mmio_address(), dhcsr.into())?;
        memory.flush()?;

        Ok(())
    }

    /// Run the boot ROM handshake on a device in CPU reset extension, then either halt at the
    /// reset handler or let the application run.
    fn boot_from_reset_extension(
        &self,
        memory: &mut dyn ArmMemoryInterface,
    ) -> Result<(), ArmError> {
        if Self::reset_to_park(memory)? == 0 {
            return Err(sequence_error(
                "The debug access level is 0, the device is locked. A chip erase unlocks it.",
            ));
        }

        if self.halt_after_reset.load(Ordering::Relaxed) {
            Self::halt_in_flash(memory)
        } else {
            Self::release_brext(memory)?;
            Self::run(memory)
        }
    }

    /// Wait for the NVM controller to be ready, and fail if the last command failed.
    fn nvm_wait_ready(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        let start = Instant::now();
        loop {
            let intflag = memory.read_word_32(NVMCTRL_INTFLAG)?;
            if intflag & INTFLAG_READY != 0 {
                if intflag & INTFLAG_ERROR != 0 {
                    let status = memory.read_word_32(NVMCTRL_STATUS)?;
                    memory.write_word_32(NVMCTRL_INTFLAG, INTFLAG_ERROR)?;
                    memory.flush()?;
                    return Err(sequence_error(format!(
                        "The NVM controller reported an error, STATUS = {status:#x}"
                    )));
                }
                return Ok(());
            }
            if start.elapsed() >= NVM_TIMEOUT {
                return Err(sequence_error("Timeout waiting for the NVM controller"));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Run an NVM controller command.
    fn nvm_command(memory: &mut dyn ArmMemoryInterface, command: u32) -> Result<(), ArmError> {
        // Clear errors left by earlier commands.
        memory.write_word_32(NVMCTRL_INTFLAG, INTFLAG_ERROR)?;
        Self::nvm_wait_ready(memory)?;
        memory.write_word_32(NVMCTRL_CTRLB, command)?;
        memory.flush()?;
        Ok(())
    }

    /// Erase the flash with the NVM controller. This requires debug access level 2.
    fn erase_with_nvmctrl(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        Self::nvm_command(memory, NVM_CMD_CHER)?;
        Self::nvm_wait_ready(memory)?;
        Self::nvm_command(memory, NVM_CMD_NOOP)?;
        Self::nvm_wait_ready(memory)?;

        // Restore the debug access level setting, if the erase cleared it.
        if memory.read_word_32(ROMCFG_DAL2)? == 0xFFFF_FFFF {
            tracing::debug!("PIC32CM-PL: restoring the DAL 2 setting in ROMCFG");
            Self::nvm_command(memory, NVM_CMD_WROMCFG)?;
            memory.write_word_32(ROMCFG_DAL2, ROMCFG_DAL2_DEFAULT)?;
            memory.flush()?;
            Self::nvm_command(memory, NVM_CMD_NOOP)?;
            Self::nvm_wait_ready(memory)?;
        }

        Ok(())
    }

    /// Erase the chip with the boot ROM's CE_ALL command. This works at any debug access level,
    /// unless the chip erase key was changed.
    fn erase_with_boot_rom(memory: &mut dyn ArmMemoryInterface) -> Result<(), ArmError> {
        Self::reset_to_interactive_mode(memory)?;

        memory.write_word_32(DSU_BCC0, CMD_CE_ALL)?;
        memory.flush()?;
        Self::wait_boot_rom_status(memory, STATUS_CHALLENGE, BOOT_ROM_TIMEOUT)?;
        Self::answer_challenge(memory)?;
        Self::wait_boot_rom_status(memory, STATUS_CMD_VALID, BOOT_ROM_TIMEOUT)?;
        Self::wait_boot_rom_status(memory, STATUS_OK, BOOT_ROM_ERASE_TIMEOUT)
    }

    /// Erase the whole flash, and reset the debug access level to 2.
    ///
    /// Leaves the core halted in the boot ROM's park mode.
    fn chip_erase(
        interface: &mut dyn ArmDebugInterface,
        ap: &FullyQualifiedApAddress,
    ) -> Result<(), ArmError> {
        tracing::info!("PIC32CM-PL: erasing the chip");

        let dal = {
            let mut memory = Self::reset_and_reconnect(interface, ap)?;
            let dal = Self::reset_to_park(&mut *memory)?;
            if dal == 2 {
                Self::erase_with_nvmctrl(&mut *memory)?;
            }
            dal
        };
        if dal != 2 {
            let mut memory = Self::reset_and_reconnect(interface, ap)?;
            Self::erase_with_boot_rom(&mut *memory)?;
        }

        let mut memory = Self::reset_and_reconnect(interface, ap)?;
        let dal = Self::reset_to_park(&mut *memory)?;
        if dal != 2 {
            return Err(sequence_error(format!(
                "The debug access level is {dal} after the chip erase"
            )));
        }

        tracing::info!("PIC32CM-PL: chip erase done");
        Ok(())
    }
}

impl ArmDebugSequence for Pic32cmPl {
    /// Restore debug access when the debug access level is 0.
    ///
    /// The device drops to DAL 0 when the boot ROM cannot start the application, or after the
    /// application faults. Then only the DSU and flash are accessible. Resetting into the boot
    /// ROM's park mode restores the configured access level. If DAL 0 is configured, the device
    /// is locked, and only a chip erase unlocks it.
    ///
    /// The core is left halted, and starts the application when it resumes. If there is no
    /// application that can be started, the core stays halted in the boot ROM's park mode, as
    /// leaving it would drop the debug access level to 0 again.
    fn debug_device_unlock(
        &self,
        interface: &mut dyn ArmDebugInterface,
        default_ap: &FullyQualifiedApAddress,
        permissions: &Permissions,
    ) -> Result<(), ArmError> {
        {
            let mut memory = interface.memory_interface(default_ap)?;
            if memory.read_word_32(DSU_DAL)? & 0x3 != 0 {
                return Ok(());
            }
            // Connecting under reset: `reset_hardware_deassert` runs the boot ROM handshake.
            if memory.read_word_32(DSU_STATUSA)? & STATUSA_CRSTEXT != 0 {
                return Ok(());
            }
        }

        tracing::warn!(
            "PIC32CM-PL: debug access level is 0, resetting into the boot ROM to restore it"
        );
        let mut memory = Self::reset_and_reconnect(interface, default_ap)?;
        if Self::reset_to_park(&mut *memory)? != 0 {
            if Self::application_reset_handler(&mut *memory)?.is_none() {
                tracing::debug!(
                    "PIC32CM-PL: no application to start, staying halted in the boot ROM"
                );
                return Ok(());
            }
            // Running from park mode with BREXT set restarts the boot ROM, which drops the
            // debug access level again. With BREXT released, it starts the application.
            return Self::release_brext(&mut *memory);
        }
        drop(memory);

        if let Err(MissingPermissions(permission)) = permissions.erase_all() {
            return Err(ArmError::MissingPermissions(format!(
                "The device is locked, with debug access level 0. A chip erase is required to \
                 unlock it. Re-run with the '{permission}' permission granted."
            )));
        }
        tracing::warn!("PIC32CM-PL: the device is locked, erasing it");
        Self::chip_erase(interface, default_ap)
    }

    /// Ignore a failed core debug deconfiguration when the debug access level is 0.
    ///
    /// When the core resumes in park mode, the boot ROM restarts, and the device can drop to
    /// debug access level 0 at any time during shutdown. The core debug registers then cannot
    /// be accessed, and the boot ROM has already reset the debug state.
    fn debug_core_stop(
        &self,
        interface: &mut dyn ArmMemoryInterface,
        core_type: CoreType,
    ) -> Result<(), ArmError> {
        let result = self
            .debug_core_stop_default(interface, core_type)
            .and_then(|()| interface.flush());
        let Err(error) = result else {
            return Ok(());
        };

        if interface.read_word_32(DSU_DAL)? & 0x3 == 0 {
            tracing::debug!(
                "PIC32CM-PL: debug access level is 0, the core debug stop failed: {error}"
            );
            return Ok(());
        }
        Err(error)
    }

    fn debug_erase_sequence(&self) -> Option<Arc<dyn DebugEraseSequence>> {
        Some(Self::create())
    }

    fn reset_hardware_assert(&self, interface: &mut dyn DebugPortWire) -> Result<(), ArmError> {
        Self::reset_into_extension(&mut SwdSequenceShim::from(interface))
    }

    fn reset_hardware_deassert(
        &self,
        probe: &mut dyn ArmDebugInterface,
        default_ap: &FullyQualifiedApAddress,
    ) -> Result<(), ArmError> {
        // nRESET was already released by `reset_hardware_assert`, the CPU is held in
        // CPU reset extension.
        let mut memory = probe.memory_interface(default_ap)?;
        self.boot_from_reset_extension(&mut *memory)
    }

    fn reset_catch_set(
        &self,
        _core: &mut dyn ArmMemoryInterface,
        _core_type: CoreType,
        _debug_base: Option<u64>,
    ) -> Result<(), ArmError> {
        self.halt_after_reset.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn reset_catch_clear(
        &self,
        core: &mut dyn ArmMemoryInterface,
        _core_type: CoreType,
        _debug_base: Option<u64>,
    ) -> Result<(), ArmError> {
        self.halt_after_reset.store(false, Ordering::Relaxed);

        let mut bp_ctrl = BpCtrl(0);
        bp_ctrl.set_key(true);
        core.write_word_32(BpCtrl::get_mmio_address(), bp_ctrl.into())?;
        core.write_word_32(BpCompx::get_mmio_address(), 0)?;
        core.flush()?;

        Ok(())
    }

    fn reset_system(
        &self,
        memory: &mut dyn ArmMemoryInterface,
        _core_type: CoreType,
        _debug_base: Option<u64>,
    ) -> Result<(), ArmError> {
        let ap = memory.fully_qualified_address();
        let interface = memory.get_arm_debug_interface()?;
        let mut memory = Self::reset_and_reconnect(interface, &ap)?;
        self.boot_from_reset_extension(&mut *memory)
    }
}

impl DebugEraseSequence for Pic32cmPl {
    fn erase_all(&self, interface: &mut dyn ArmDebugInterface) -> Result<(), ArmError> {
        let ap = FullyQualifiedApAddress::v1_with_default_dp(0);
        Self::chip_erase(interface, &ap)?;

        // The chip was reset, so the session must reconnect.
        Err(ArmError::ReAttachRequired)
    }
}
