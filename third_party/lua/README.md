# Lua 5.4.6

Unmodified source files from the official Lua 5.4.6 release
(`lua-5.4.6.tar.gz`, SHA-256
`7d5ea1b9cb6aa0b59ca3dde1c6adcb57ef83a1ba8e5432c0ecd06bf439b3ad88`).

Only the core and the libraries tom-os uses are included. Left out:
`lua.c`, `luac.c`, `linit.c`, `liolib.c`, `loslib.c`, `loadlib.c`, `ldblib.c`
(tom-os provides its own `io`, `os` and `require` in `lua/lbind.c`).

Lua is Copyright (C) 1994-2023 Lua.org, PUC-Rio, and distributed under the MIT
license. The full license text is at the end of `lua.h`.
