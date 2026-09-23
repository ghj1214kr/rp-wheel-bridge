MEMORY {
    /*
     * Waveshare RP2350-USB-A
     * W25Q16 = 2 MiB external QSPI flash
     */
    FLASH : ORIGIN = 0x10000000, LENGTH = 2048K

    /*
     * RP2350 striped SRAM banks.
     */
    RAM : ORIGIN = 0x20000000, LENGTH = 512K

    /*
     * Dedicated SRAM banks.
     * Later useful for Core 0/Core 1 stacks or USB buffers.
     */
    SRAM4 : ORIGIN = 0x20080000, LENGTH = 4K
    SRAM5 : ORIGIN = 0x20081000, LENGTH = 4K
}

SECTIONS {
    /*
     * RP2350 Image Definition / Boot information.
     * Must be visible to the Boot ROM near the start of flash.
     */
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
        KEEP(*(.boot_info));

        /* .text requires 8-byte alignment */
        . = ALIGN(8);
    } > FLASH
} INSERT AFTER .vector_table;

_stext = ADDR(.start_block) + SIZEOF(.start_block);

SECTIONS {
    /*
     * picotool Binary Info entries.
     */
    .bi_entries : ALIGN(4)
    {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

SECTIONS {
    /*
     * RP2350 Boot ROM end block.
     */
    .end_block : ALIGN(4)
    {
        __end_block_addr = .;
        KEEP(*(.end_block));
    } > FLASH
} INSERT AFTER .uninit;

PROVIDE(start_to_end = __end_block_addr - __start_block_addr);
PROVIDE(end_to_start = __start_block_addr - __end_block_addr);