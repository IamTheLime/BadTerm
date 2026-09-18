# Session 2026-09-17: Neovim ;f at large Wayland sizes

The earlier `nvim --clean` timing missed the user's actual workload: configured
Neovim with the `;f` fff.nvim picker. The test here used the Rust release build
with libghostty's `ReleaseFast` engine, configured Neovim, a 309x75 grid on KDE
Wayland, and scripted `;f`, query entry, and result navigation.

Before this change, a picker frame painted about 23,175 background quads in
20-24 ms and text layout sometimes took 100-165 ms. The shell output arrived
in large batches, but the logged Ghostty feed and grid snapshot were much
smaller costs. `libghostty-vt` handles terminal state and escape sequences;
this app builds GPUI shapes and submits paint operations itself.

The painter now merges adjacent same-colour backgrounds into row runs, skips
long undecorated space gaps when shaping text, and reuses shaped rows and text
fragments when their contents and styles match. The background count dropped
to roughly 77-163 quads in the tested picker frames, with paint below the
10 ms logging threshold. Layout in the same test was commonly 12-24 ms.
Fragment cache hits made some repeated frames cheaper, while a new search
query still required about 17-19 ms of layout. These are CPU stage timings,
not whole-frame latency, and do not establish parity with Ghostty's renderer.

The segment cache is bounded at 8192 entries. Row cache entries compare full
cell contents and the block cursor's text colour before reuse. It is cleared
when cell size changes. `make test` passed all 20 workspace tests, `make
build-release` passed, and `git diff --check` was clean. Fedora still needs
the temporary `libxkbcommon-x11.so` linker symlink for this environment.
