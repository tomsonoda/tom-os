; tom-os boot: Multiboot1 entry (32-bit protected mode) -> 64-bit long mode -> kmain
;
; Identity-maps the first 4 GiB with 2 MiB pages, enables SSE and the x87 FPU,
; and provides the exception / IRQ entry stubs and setjmp/longjmp.

MB_MAGIC    equ 0x1BADB002
MB_FLAGS    equ 0x00000003          ; page-align modules, provide memory map
STACK_SIZE  equ 2 * 1024 * 1024

section .multiboot
align 4
    dd MB_MAGIC
    dd MB_FLAGS
    dd -(MB_MAGIC + MB_FLAGS)

section .bss
align 4096
pml4:       resb 4096
pdpt:       resb 4096
pd:         resb 4096 * 4
stack_bottom:
            resb STACK_SIZE
stack_top:

section .rodata
align 16
gdt:
    dq 0
    dq 0x00209A0000000000           ; 0x08: 64-bit code
    dq 0x0000920000000000           ; 0x10: data
gdt_ptr:
    dw gdt_ptr - gdt - 1
    dq gdt

section .text
bits 32
global _start
extern kmain
extern __bss_start
extern __bss_end

_start:
    cli
    cld
    mov esi, eax                    ; multiboot magic
    ; ebx = multiboot info pointer (kept in ebx: rdmsr below clobbers edx)

    ; zero .bss (page tables and stack live there)
    mov edi, __bss_start
    mov ecx, __bss_end
    sub ecx, edi
    xor eax, eax
    rep stosb

    mov esp, stack_top

    ; PML4[0] -> PDPT, PDPT[0..3] -> PD[0..3], 2048 x 2 MiB pages
    mov eax, pdpt
    or eax, 3
    mov [pml4], eax

    xor ecx, ecx
.pdpt_loop:
    mov eax, ecx
    shl eax, 12
    add eax, pd
    or eax, 3
    mov [pdpt + ecx * 8], eax
    inc ecx
    cmp ecx, 4
    jne .pdpt_loop

    xor ecx, ecx
.pd_loop:
    mov eax, ecx
    shl eax, 21
    or eax, 0x83                    ; present, writable, 2 MiB
    mov [pd + ecx * 8], eax
    inc ecx
    cmp ecx, 2048
    jne .pd_loop

    mov eax, pml4
    mov cr3, eax

    mov eax, cr4
    or eax, (1 << 5) | (1 << 9) | (1 << 10)     ; PAE, OSFXSR, OSXMMEXCPT
    mov cr4, eax

    mov ecx, 0xC0000080             ; EFER
    rdmsr
    or eax, 1 << 8                  ; LME
    wrmsr

    mov eax, cr0
    and eax, ~(1 << 2)              ; clear EM
    or eax, (1 << 31) | (1 << 1) | (1 << 5)     ; PG, MP, NE
    mov cr0, eax

    lgdt [gdt_ptr]
    jmp 0x08:long_start

bits 64
long_start:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    fninit
    mov rsp, stack_top
    mov edi, ebx                    ; arg0: multiboot info
    mov esi, esi                    ; arg1: magic
    call kmain
.hang:
    cli
    hlt
    jmp .hang

; ---------------------------------------------------------------- exceptions

extern exception_handler

%macro EXC_NOERR 1
exc%1:
    push 0
    push %1
    jmp exc_common
%endmacro

%macro EXC_ERR 1
exc%1:
    push %1
    jmp exc_common
%endmacro

EXC_NOERR 0
EXC_NOERR 1
EXC_NOERR 2
EXC_NOERR 3
EXC_NOERR 4
EXC_NOERR 5
EXC_NOERR 6
EXC_NOERR 7
EXC_ERR   8
EXC_NOERR 9
EXC_ERR   10
EXC_ERR   11
EXC_ERR   12
EXC_ERR   13
EXC_ERR   14
EXC_NOERR 15
EXC_NOERR 16
EXC_ERR   17
EXC_NOERR 18
EXC_NOERR 19
EXC_NOERR 20
EXC_ERR   21
EXC_NOERR 22
EXC_NOERR 23
EXC_NOERR 24
EXC_NOERR 25
EXC_NOERR 26
EXC_NOERR 27
EXC_NOERR 28
EXC_ERR   29
EXC_ERR   30
EXC_NOERR 31

exc_common:
    mov rdi, [rsp]                  ; vector
    mov rsi, [rsp + 8]              ; error code
    mov rdx, [rsp + 16]             ; rip
    mov rcx, cr2
    and rsp, -16
    call exception_handler
.halt:
    cli
    hlt
    jmp .halt

; IRQs only exist to wake the CPU from HLT; all devices are polled.
global irq_master
global irq_slave
irq_master:
    push rax
    mov al, 0x20
    out 0x20, al
    pop rax
    iretq
irq_slave:
    push rax
    mov al, 0x20
    out 0xA0, al
    out 0x20, al
    pop rax
    iretq

section .rodata
global exc_table
align 8
exc_table:
%assign i 0
%rep 32
    dq exc %+ i
%assign i i + 1
%endrep

; ------------------------------------------------------- thread switching
; void ctx_switch(u64 *save_rsp, u64 new_rsp)
; Cooperative switch: only callee-saved registers need to survive.
section .text
global ctx_switch
ctx_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

; ------------------------------------------------------------ setjmp/longjmp
section .text
global setjmp
global longjmp
setjmp:
    mov [rdi], rbx
    mov [rdi + 8], rbp
    mov [rdi + 16], r12
    mov [rdi + 24], r13
    mov [rdi + 32], r14
    mov [rdi + 40], r15
    lea rdx, [rsp + 8]
    mov [rdi + 48], rdx
    mov rdx, [rsp]
    mov [rdi + 56], rdx
    xor eax, eax
    ret

longjmp:
    mov eax, esi
    test eax, eax
    jnz .ok
    inc eax
.ok:
    mov rbx, [rdi]
    mov rbp, [rdi + 8]
    mov r12, [rdi + 16]
    mov r13, [rdi + 24]
    mov r14, [rdi + 32]
    mov r15, [rdi + 40]
    mov rsp, [rdi + 48]
    jmp [rdi + 56]
