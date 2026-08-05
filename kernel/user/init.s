.intel_syntax noprefix
.globl _start
_start:
    lea  rsi, [rip + msg]
    mov  rax, 1
    mov  rdi, rsi
    mov  rsi, 25
    syscall
    mov  rax, 0
    mov  rdi, 7
    syscall
    ud2
msg:
    .ascii "hello from ring 3, qunix\n"
