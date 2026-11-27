# tom-os build
#
#   make                 the PC version (x86-64)
#   make ARCH=aarch64    the 64-bit ARM version (QEMU's "virt" machine)
#
# The kernel is Rust and needs rustup: rust/rust-toolchain.toml selects a
# nightly toolchain, which rustup downloads on the first build. The boot
# code is assembled with NASM, and the kernel is linked by GNU ld into a
# freestanding x86-64 ELF. On Linux the system binutils work. On macOS
# install the cross binutils (brew install nasm x86_64-elf-binutils qemu
# rustup); they are picked up automatically. Override with
# `make CROSS=<prefix>`.
#
# The ARM version needs only rustup (and QEMU to run it): its boot code is
# in Rust's built-in assembler and it is linked with the linker that comes
# with Rust.
ARCH ?= x86_64
ifeq ($(shell uname -s),Darwin)
CROSS ?= x86_64-elf-
else
CROSS ?=
endif

AS      := nasm
LD      := $(CROSS)ld
OBJCOPY := $(CROSS)objcopy
QEMU    ?= qemu-system-$(ARCH)
GRUB_MKRESCUE ?= grub-mkrescue

# Fail early with a hint instead of a confusing compiler error.
ifeq ($(ARCH),x86_64)
REQUIRED := $(LD) $(OBJCOPY) $(AS) cargo
else
REQUIRED := cargo
endif
MISSING  := $(strip $(foreach t,$(REQUIRED),$(if $(shell command -v $(t) 2>/dev/null),,$(t))))
ifneq ($(MISSING),)
ifeq ($(filter clean,$(MAKECMDGOALS)),)
ifeq ($(shell uname -s),Darwin)
$(error Missing tools: $(MISSING). Install them with: brew install nasm x86_64-elf-binutils qemu rustup (then: rustup-init))
else
$(error Missing tools: $(MISSING). Install binutils, nasm and Rust (https://rustup.rs))
endif
endif
endif

BUILD := build
# The kernel: rust/ and third_party/remedy-lua (the Lua interpreter), built
# by cargo into one static library, plus the boot code.
RUST_SRCS := $(shell find rust/src third_party/remedy-lua/src rust/assets -type f) rust/Cargo.toml \
             rust/x86_64-tomos.json third_party/remedy-lua/Cargo.toml
ifeq ($(ARCH),x86_64)
RUSTLIB := rust/target/x86_64-tomos/release/libtomos_rt.a
OBJS := $(BUILD)/arch/x86_64/boot.o
KERNEL := $(BUILD)/tomos.elf
else
RUSTLIB := rust/target/aarch64-unknown-none/release/libtomos_rt.a
# A Linux-style kernel image: what QEMU's -kernel and ARM boot loaders take.
KERNEL := $(BUILD)/aarch64/Image
endif

# LLM endpoint as seen from inside the VM (10.0.2.2 is the host under QEMU user networking)
LLM   ?= 10.0.2.2:8080
MODEL ?= default
MEM   ?= 256M
# Screen size and scale for graphical mode, e.g. RES=1920x1080 or
# RES=2048x1536 SCALE=2 (the same layout as 1024x768, twice as big).
RES   ?=
SCALE ?=
# 64 MiB of video memory allows screens up to about 4K.
ifeq ($(ARCH),x86_64)
QEMU_FLAGS := -m $(MEM) -no-reboot -kernel $(KERNEL) -vga none -device VGA,vgamem_mb=64 \
              -netdev user,id=n0 -device virtio-net-pci,netdev=n0
else
# The "virt" machine with a network card; in graphical mode also a simple
# framebuffer, a keyboard and a tablet (a pointer that follows the host's).
QEMU_FLAGS := -M virt -cpu cortex-a72 -m $(MEM) -no-reboot -kernel $(KERNEL) \
              -global virtio-mmio.force-legacy=false \
              -netdev user,id=n0 -device virtio-net-device,netdev=n0
QEMU_GUI   := -device ramfb -device virtio-keyboard-device -device virtio-tablet-device
endif
ARGS := llm=$(LLM) model=$(MODEL) $(if $(RES),res=$(RES)) $(if $(SCALE),scale=$(SCALE))

# On macOS, the picture follows the size of the QEMU window.
ifeq ($(shell uname -s),Darwin)
QEMU_DISPLAY ?= -display cocoa,zoom-to-fit=on
else
QEMU_DISPLAY ?=
endif

# Sound: an AC'97 card played through the host. macOS: coreaudio. On Linux
# pick your sound system, e.g. AUDIO=pa (PulseAudio/PipeWire) or AUDIO=alsa;
# the default "none" keeps the card but plays nothing.
ifeq ($(shell uname -s),Darwin)
AUDIO ?= coreaudio
else
AUDIO ?= none
endif
ifeq ($(ARCH),x86_64)
QEMU_FLAGS += -audiodev $(AUDIO),id=snd -device AC97,audiodev=snd
endif

ISO := $(BUILD)/tomos.iso

all: $(KERNEL)

ifeq ($(ARCH),x86_64)
$(RUSTLIB): $(RUST_SRCS)
	+cd rust && cargo build --release
else
$(RUSTLIB): $(RUST_SRCS)
	+cd rust && cargo build --release --target aarch64-unknown-none

# The linker and objcopy that come with the Rust toolchain.
RUST_BIN = $(shell cd rust && rustc --print sysroot)/lib/rustlib/$(shell cd rust && rustc -vV | sed -n 's/host: //p')/bin

$(KERNEL): $(RUSTLIB) arch/aarch64/linker.ld
	@mkdir -p $(dir $@)
	$(RUST_BIN)/rust-lld -flavor gnu -nostdlib -z max-page-size=0x1000 --gc-sections \
	    -T arch/aarch64/linker.ld -o $(BUILD)/aarch64/tomos.elf $(RUSTLIB)
	$(RUST_BIN)/rust-objcopy -O binary $(BUILD)/aarch64/tomos.elf $@
	@ls -l $@ | awk '{printf "built %s (%d KiB)\n", $$NF, $$5 / 1024}'
endif

ifeq ($(ARCH),x86_64)
$(KERNEL): $(OBJS) $(RUSTLIB) linker.ld
	$(LD) -n -nostdlib -z max-page-size=0x1000 -z noexecstack --no-warn-rwx-segments --gc-sections \
	    -T linker.ld -o $(BUILD)/tomos64.elf $(OBJS) $(RUSTLIB)
	$(OBJCOPY) -O elf32-i386 --strip-debug $(BUILD)/tomos64.elf $@
	@ls -l $@ | awk '{printf "built %s (%d KiB)\n", $$NF, $$5 / 1024}'

$(BUILD)/arch/x86_64/boot.o: arch/x86_64/boot.asm
	@mkdir -p $(dir $@)
	$(AS) -f elf64 $< -o $@
endif

# Text mode: the serial console in this terminal. Ctrl-C goes to tom-os;
# quit QEMU with Ctrl-A X.
run: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) -append "$(ARGS) gui=0" -nographic

# Graphical mode: a QEMU window with the desktop. This terminal stays
# connected to the shell as well (useful for typing non-ASCII text).
run-gui: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) $(QEMU_GUI) $(QEMU_DISPLAY) -append "$(ARGS)" -serial mon:stdio

# Text mode on the VGA screen in a QEMU window (PC only).
run-vga: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) -append "$(ARGS) gui=0" -serial mon:stdio

# The ARM version has no sound driver yet, so there is no sound test for it.
test: $(KERNEL)
	TOMOS_ARCH=$(ARCH) python3 tests/boot_test.py
	TOMOS_ARCH=$(ARCH) python3 tests/lua_test.py
	TOMOS_ARCH=$(ARCH) python3 tests/gui_test.py
	TOMOS_ARCH=$(ARCH) python3 tests/phone_test.py
	TOMOS_ARCH=$(ARCH) python3 tests/ime_test.py
ifeq ($(ARCH),x86_64)
	python3 tests/sound_test.py
endif
	TOMOS_ARCH=$(ARCH) python3 tests/gateway_test.py

# Bootable CD image (BIOS boot via GRUB). Needs grub-pc-bin, xorriso and mtools.
iso: $(ISO)

$(ISO): $(KERNEL) iso/grub.cfg
	rm -rf $(BUILD)/isoroot
	mkdir -p $(BUILD)/isoroot/boot/grub
	cp $(KERNEL) $(BUILD)/isoroot/boot/tomos.elf
	sed 's|@MODEL@|$(MODEL)|g' iso/grub.cfg > $(BUILD)/isoroot/boot/grub/grub.cfg
	$(GRUB_MKRESCUE) -o $@ $(BUILD)/isoroot 2> $(BUILD)/grub-mkrescue.log || (cat $(BUILD)/grub-mkrescue.log; false)
	@ls -l $@ | awk '{printf "built %s (%.1f MiB)\n", $$NF, $$5 / 1048576}'

run-iso: $(ISO)
	$(QEMU) -m $(MEM) -no-reboot -cdrom $(ISO) -boot d -vga none -device VGA,vgamem_mb=64 $(QEMU_DISPLAY) \
	    -audiodev $(AUDIO),id=snd -device AC97,audiodev=snd \
	    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -serial mon:stdio

test-iso: $(ISO)
	TOMOS_ISO=$(ISO) python3 tests/boot_test.py
	TOMOS_ISO=$(ISO) python3 tests/gui_test.py
	TOMOS_ISO=$(ISO) python3 tests/phone_test.py
	TOMOS_ISO=$(ISO) python3 tests/ime_test.py

clean:
	rm -rf $(BUILD) rust/target

.PHONY: all run run-gui run-vga test iso run-iso test-iso clean
