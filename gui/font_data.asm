; The Japanese font (see tools/make_font.py), linked into the kernel as data.
section .rodata
global font_ja
font_ja:
    incbin "gui/font_ja.bin"
