# Session 2026-09-17: maximized Wayland terminal performance

Typing and scrolling in Neovim slowed sharply when the terminal was maximized
on KDE Wayland. The Rust release build used the Ghostty `ReleaseFast` engine.
At a 309x75 grid, the terminal snapshot took about 1-2 ms, while building
GPUI text layouts commonly took around 100 ms per frame. The same operation
was much quicker in a small window, so the grid width was the useful variable.

The painter was shaping trailing spaces in every row. A first change skipped
trailing `CellContent::Blank` cells, but Neovim also writes explicit spaces
that arrive as `CellContent::Text(" ")`. The final change skips both kinds of
trailing undecorated space while retaining background quads. Interior spaces
and underlined or struck-through spaces still enter the text layout.

With the same Neovim test file at 309x75, scripted typing and scrolling in the
release build produced 14-18 ms text layout frames. On a 149x137 grid, the
observed range was 18-29 ms. These are layout timings, not whole-frame or GPU
timings. `make build-release`, all 20 workspace tests, and `git diff --check`
passed. The Fedora test run used a temporary linker symlink for the installed
`libxkbcommon-x11.so.0`, as described in the Makefile session log. Cargo fmt
is unavailable on this host.
