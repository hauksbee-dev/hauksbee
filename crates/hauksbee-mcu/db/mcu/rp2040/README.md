# RP2040 Renode support bundle

Renode 1.16.1 (the pinned build, and every release up to it) ships **no RP2040
platform at all**: `platforms/cpus/` carries `picosoc.repl` and
`litex_picorv32.repl`, neither of which is an RP2040, and there is no rp2040
platform on Renode `master` either. Unlike the STM32F1 case, where the stock
platform existed and only needed extending, there is nothing upstream to extend
here, so the peripheral **models themselves** have to come from somewhere.

They come from this directory. The files are vendored, not written here, and
Renode compiles the C# at run time (its `include` Monitor command drives the
bundled C# compiler), so no .NET SDK and no prebuilt DLL is needed on the user's
machine. `crates/hauksbee-mcu/src/renode/support.rs` embeds everything below
into the binary, unpacks it to a temp directory when an RP2040 machine is
created, and `include`s the sources before the platform description parses.

## What is vendored, and from where

| File(s) | Origin | Licence |
| --- | --- | --- |
| `peripherals/*.cs` | [`matgla/Renode_RP2040`](https://github.com/matgla/Renode_RP2040) at commit `205a5e4b25440582008a4292074bb07f80a72328`, from `emulation/peripherals/**` and `emulation/externals/w25q16.cs`, flattened into one directory | MIT, see `LICENSE.Renode_RP2040` |
| `RP2040.svd.gz` | `src/rp2040/hardware_regs/RP2040.svd` from [`raspberrypi/pico-sdk`](https://github.com/raspberrypi/pico-sdk) tag `2.1.1`, gzipped verbatim. Uncompressed sha256 `9dc5b4eba17042477b812b1fc769ecb8c0a69a545fd69ad5f33ff5868151bda9` | BSD-3-Clause, see `LICENSE.pico-sdk` |
| `bootrom.elf` | the `b2.elf` release asset of [`raspberrypi/pico-bootrom-rp2040`](https://github.com/raspberrypi/pico-bootrom-rp2040/releases/tag/b2), byte-identical, sha256 `3a3c4c617d83bc9b910a6fa7bfdffa11ce26ff89370271d861507be821cc49aa` | BSD-3-Clause, see `LICENSE.pico-bootrom` |
| `rp2040.repl` | written here, derived from `cores/rp2040.repl` of the same Renode_RP2040 commit | Apache-2.0, this repo |

The `.cs` files are byte-for-byte copies. Nothing in `peripherals/` is edited;
when a fix is needed it goes upstream and the file is re-copied, so that this
directory never becomes a silent fork. To refresh, clone the upstream repo at
the commit you want and copy the same file list (the list lives in
`src/renode/support.rs`, in load order, because Renode's C# `include` is order
sensitive: later files reference types from earlier ones).

## Why the SVD and the bootrom are here at all

**The SVD** is what makes the unmodelled corners of the chip survivable.
`ApplySVD` registers every RP2040 register named in the SVD as a tag, so a read
of, say, `TBMAN:PLATFORM` returns its documented reset value with a loud
`[WARNING] ... returning a value from SVD` line instead of a bare "non existing
peripheral". The SDK's own start-up touches SYSCFG, VREG_AND_CHIP_RESET and
TBMAN, none of which any of the vendored models implement, so without the SVD
boot is a wall of unexplained bus errors. Raspberry Pi's own SVD works; it was
checked against the same firmware the smoke test uses.

**The bootrom** is a real requirement, not a convenience. The pico-sdk runtime
calls into the RP2040 boot ROM's function table (`rom_func_lookup`) during
`runtime_init`, so the ROM's contents must be present at address 0 before the
firmware runs. It is loaded as an ELF into `bootrom0`. Note that it is *loaded*,
not *executed from reset*: `sysbus LoadELF` of the firmware sets the PC to the
firmware's own entry point, and the vendored platform maps the XIP window as
plain memory, so the flash-resident image is directly fetchable and the
second-stage-bootloader/QSPI path is never on the critical path to `main`.

## What is deliberately NOT vendored

`piocpu0`/`piocpu1` (the two PIO state machines). Upstream models PIO as an
extra CPU backed by a native C++ library (`piosim`), shipped prebuilt for
x86-64 only. The checked-in `libpiosim.dylib` is x86-64 and will not load into
the arm64 Renode, and building it per host is a native-toolchain dependency this
crate does not otherwise have. `peripherals/rp2040_pio.cs` IS vendored, because
the SIO, GPIO, SPI and ADC models reference its types and will not compile
without it; it simply is never instantiated. Consequence: **PIO is not
available**, and any firmware whose observable behaviour goes through PIO (the
usual WS2812 driver, PIO-driven I2C/SPI, `pico_stdio_pio`) will not produce
those effects. That is a capability gap, recorded here and in
`rp2040.soc.toml`, not a bug to be discovered later.
