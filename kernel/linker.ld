/* Layout of the kernel image, as Limine requires it.
 *
 * This replaces the `--image-base` / `--entry` LLD flags M0 used instead of a
 * script. Those were enough while the only requirement was "start in the higher
 * half", but they leave every section an *orphan*: LLD places it by name and
 * attributes, in whatever order that produces. Two things then go wrong.
 *
 * The requests are found by a scan of the loaded image rather than by the
 * markers, because orphan placement cannot put the markers around them -- it
 * sorted `.requests_end_marker` *before* `.requests_start_marker` and left
 * `.requests` outside the pair entirely. That scan is not a guarantee the
 * protocol makes, and it stopped finding them once the image grew past a
 * certain size: a release build faulted on `base revision unsupported` while
 * the same source in debug booted. Bisect named a commit of renames and doc
 * comments, which is the signature of a layout dependency rather than a logic
 * bug.
 *
 * So the requests get an explicit section, bracketed by the markers in the
 * order the protocol specifies, `KEEP`-ed so `--gc-sections` cannot drop what
 * nothing references.
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

/* One loadable segment per permission set. Limine maps each with the
 * permissions its flags carry, so merging them would mean either writable code
 * or unwritable data.
 *
 * `requests` is separate from `rodata` for exactly that reason: the bootloader
 * *writes* each request's response pointer, so the requests must be writable,
 * and sharing a segment with `.rodata` silently made the kernel's entire
 * read-only data writable. */
PHDRS
{
    text     PT_LOAD;
    requests PT_LOAD;
    rodata   PT_LOAD;
    data     PT_LOAD;
}

SECTIONS
{
    /* The top 2 GiB, where the kernel code model puts every symbol: `kernel`
     * assumes RIP-relative displacements reach the whole image, which only
     * holds inside a 2 GiB window. */
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    /* Each segment starts on its own page, because the permission change
     * happens at page granularity. Without this the last page of one segment
     * and the first of the next share a mapping and one of them gets the wrong
     * permissions. */
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* Before `.rodata`, so the requests sit early in the image -- the
     * bootloader reads them before anything else and their placement is the
     * whole point of this script. */
    .requests : {
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    .eh_frame_hdr : { *(.eh_frame_hdr) } :rodata
    .eh_frame     : { *(.eh_frame) } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* `.data.rel.ro` and `.got` are writable here rather than relro-protected:
     * this image is non-PIE with static relocation, so they hold no relocations
     * to apply and nothing re-protects them after load. */
    .data : {
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.plt)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /* Dynamic-linking machinery for an image that is never dynamically linked.
     * `.note.gnu.property` in particular is placed as an orphan ahead of
     * everything else and pushes the load address down. */
    /DISCARD/ : {
        *(.note .note.*)
        *(.interp)
        *(.dynamic)
        *(.dynsym .dynstr .hash .gnu.hash)
    }
}
