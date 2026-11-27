# tom-os build
#
# The kernel is Rust and needs rustup: rust/rust-toolchain.toml selects a
# nightly toolchain, which rustup downloads on the first build. The boot
# code is assembled with NASM, and the kernel is linked by GNU ld into a
# freestanding x86-64 ELF. On Linux the system binutils work. On macOS
# install the cross binutils (brew install nasm x86_64-elf-binutils qemu
# rustup); they are picked up automatically. Override with
# `make CROSS=<prefix>`.
ifeq ($(shell uname -s),Darwin)
CROSS ?= x86_64-elf-
else
CROSS ?=
endif

AS      := nasm
LD      := $(CROSS)ld
OBJCOPY := $(CROSS)objcopy
QEMU    ?= qemu-system-x86_64
GRUB_MKRESCUE ?= grub-mkrescue

# Fail early with a hint instead of a confusing compiler error.
REQUIRED := $(LD) $(OBJCOPY) $(AS) cargo
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
RUSTLIB := rust/target/x86_64-tomos/release/libtomos_rt.a
RUST_SRCS := $(shell find rust/src third_party/remedy-lua/src -type f) rust/Cargo.toml \
             rust/x86_64-tomos.json rust/assets/font_ja.bin third_party/remedy-lua/Cargo.toml
OBJS := $(BUILD)/arch/x86_64/boot.o

KERNEL := $(BUILD)/tomos.elf

# LLM endpoint as seen from inside the VM (10.0.2.2 is the host under QEMU user networking)
LLM   ?= 10.0.2.2:8080
MODEL ?= default
MEM   ?= 256M
# Screen size and scale for graphical mode, e.g. RES=1920x1080 or
# RES=2048x1536 SCALE=2 (the same layout as 1024x768, twice as big).
RES   ?=
SCALE ?=
# 64 MiB of video memory allows screens up to about 4K.
QEMU_FLAGS := -m $(MEM) -no-reboot -kernel $(KERNEL) -vga none -device VGA,vgamem_mb=64 \
              -netdev user,id=n0 -device virtio-net-pci,netdev=n0
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
QEMU_FLAGS += -audiodev $(AUDIO),id=snd -device AC97,audiodev=snd

ISO := $(BUILD)/tomos.iso

all: $(KERNEL)

$(RUSTLIB): $(RUST_SRCS)
	+cd rust && cargo build --release

$(KERNEL): $(OBJS) $(RUSTLIB) linker.ld
	$(LD) -n -nostdlib -z max-page-size=0x1000 -z noexecstack --no-warn-rwx-segments --gc-sections \
	    -T linker.ld -o $(BUILD)/tomos64.elf $(OBJS) $(RUSTLIB)
	$(OBJCOPY) -O elf32-i386 --strip-debug $(BUILD)/tomos64.elf $@
	@ls -l $@ | awk '{printf "built %s (%d KiB)\n", $$NF, $$5 / 1024}'

$(BUILD)/arch/x86_64/boot.o: arch/x86_64/boot.asm
	@mkdir -p $(dir $@)
	$(AS) -f elf64 $< -o $@

# Text mode: the serial console in this terminal. Ctrl-C goes to tom-os;
# quit QEMU with Ctrl-A X.
run: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) -append "$(ARGS) gui=0" -nographic

# Graphical mode: a QEMU window with the desktop. This terminal stays
# connected to the shell as well (useful for typing non-ASCII text).
run-gui: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) $(QEMU_DISPLAY) -append "$(ARGS)" -serial mon:stdio

# Text mode on the VGA screen in a QEMU window.
run-vga: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) -append "$(ARGS) gui=0" -serial mon:stdio

test: $(KERNEL)
	python3 tests/boot_test.py
	python3 tests/lua_test.py
	python3 tests/gui_test.py
	python3 tests/phone_test.py
	python3 tests/ime_test.py
	python3 tests/sound_test.py
	python3 tests/gateway_test.py

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
