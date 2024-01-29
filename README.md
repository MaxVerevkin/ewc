# EWC - Experimental Wayland Compositor

Wayland compositor from scratch in Rust.

- No `wlroots` or `smithay`.
- No `libwayland`.

## Roadmap

- [x] Enough to run `foot`.
- [x] Sowfware (`pixman`) renderer.
- [x] OpenGL renderer.
- [x] Nested wayland backend.
- [x] Basic single-output drm/kms backend.
- [ ] Basic dynamic window management (master-stack layout).
- [ ] Full `wayland.xml` conformance (minus deprecated `wl_shell`).
- [ ] Full `xdg-shell.xml` conformance.
- [ ] Basic `linux-dmabuf-v1.xml` support.
- [ ] Yes/no damage tracking.
- [ ] Full damage tracking.
- [ ] Direct scan-out support.

## Supported protocols

- [ ] `wayland.xml` (partial)
- [ ] `xdg-shell.xml` (partial)
- [ ] `linux-dmabuf-v1.xml`
- [ ] `viewporter.xml`
- [ ] `single-pixel-buffer-v1.xml`
- [ ] `fractional-scale-v1.xml`
- [ ] `wlr-layer-shell-unstable-v1.xml`
