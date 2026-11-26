# tom-os build
CC      ?= gcc
AS      := nasm
LD      := ld
OBJCOPY := objcopy
QEMU    ?= qemu-system-x86_64

BUILD := build
GCCINC := $(shell $(CC) -print-file-name=include)

CFLAGS := -std=gnu11 -O2 -g -ffreestanding -fno-stack-protector -fno-pic -fno-pie \
          -mno-red-zone -mcmodel=small -msse2 -fno-asynchronous-unwind-tables \
          -fno-tree-loop-distribute-patterns -fno-strict-aliasing \
          -nostdinc -isystem $(GCCINC) -isystem lib/include -D_LIBC_LIMITS_H_
WARN   := -Wall -Wextra

SRCS := $(wildcard kernel/*.c net/*.c lib/*.c fs/*.c agent/*.c lua/*.c)
LUA_SRCS := $(wildcard third_party/lua/*.c)
OBJS := $(BUILD)/arch/x86_64/boot.o \
        $(SRCS:%.c=$(BUILD)/%.o) $(LUA_SRCS:%.c=$(BUILD)/%.o)

KERNEL := $(BUILD)/tomos.elf

# LLM endpoint as seen from inside the VM (10.0.2.2 is the host under QEMU user networking)
LLM   ?= 10.0.2.2:8080
MODEL ?= default
MEM   ?= 256M
QEMU_FLAGS := -m $(MEM) -no-reboot -kernel $(KERNEL) \
              -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
              -append "llm=$(LLM) model=$(MODEL)"

ISO := $(BUILD)/tomos.iso

all: $(KERNEL)

$(KERNEL): $(OBJS) linker.ld
	$(LD) -n -nostdlib -z max-page-size=0x1000 -z noexecstack --no-warn-rwx-segments -T linker.ld -o $(BUILD)/tomos64.elf $(OBJS)
	$(OBJCOPY) -O elf32-i386 --strip-debug $(BUILD)/tomos64.elf $@
	@ls -l $@ | awk '{printf "built %s (%d KiB)\n", $$NF, $$5 / 1024}'

$(BUILD)/arch/x86_64/boot.o: arch/x86_64/boot.asm
	@mkdir -p $(dir $@)
	$(AS) -f elf64 $< -o $@

$(BUILD)/third_party/lua/%.o: third_party/lua/%.c
	@mkdir -p $(dir $@)
	$(CC) $(CFLAGS) -w -c $< -o $@

$(BUILD)/%.o: %.c kernel/tomos.h
	@mkdir -p $(dir $@)
	$(CC) $(CFLAGS) $(WARN) -c $< -o $@

# Serial console in this terminal. Ctrl-C goes to tom-os; quit QEMU with Ctrl-A X.
run: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) -nographic

# VGA window plus serial console in this terminal.
run-vga: $(KERNEL)
	$(QEMU) $(QEMU_FLAGS) -serial mon:stdio

test: $(KERNEL)
	python3 tests/boot_test.py

# Bootable CD image (BIOS boot via GRUB). Needs grub-pc-bin, xorriso and mtools.
iso: $(ISO)

$(ISO): $(KERNEL) iso/grub.cfg
	rm -rf $(BUILD)/isoroot
	mkdir -p $(BUILD)/isoroot/boot/grub
	cp $(KERNEL) $(BUILD)/isoroot/boot/tomos.elf
	sed 's|@MODEL@|$(MODEL)|g' iso/grub.cfg > $(BUILD)/isoroot/boot/grub/grub.cfg
	grub-mkrescue -o $@ $(BUILD)/isoroot 2> $(BUILD)/grub-mkrescue.log || (cat $(BUILD)/grub-mkrescue.log; false)
	@ls -l $@ | awk '{printf "built %s (%.1f MiB)\n", $$NF, $$5 / 1048576}'

run-iso: $(ISO)
	$(QEMU) -m $(MEM) -no-reboot -cdrom $(ISO) -boot d \
	    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -serial mon:stdio

test-iso: $(ISO)
	TOMOS_ISO=$(ISO) python3 tests/boot_test.py

clean:
	rm -rf $(BUILD)

.PHONY: all run run-vga test iso run-iso test-iso clean
