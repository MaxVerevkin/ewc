use ewc_wayland_scanner::generate as g;

g!("protocol/wayland.xml");

g!("protocol/ewc-debug.xml");

g!("wayland-protocols/stable/xdg-shell/xdg-shell.xml");
g!("wayland-protocols/stable/viewporter/viewporter.xml");
g!("wayland-protocols/stable/linux-dmabuf/linux-dmabuf-v1.xml");
g!("wayland-protocols/staging/cursor-shape/cursor-shape-v1.xml");
// g!("wayland-protocols/staging/single-pixel-buffer/single-pixel-buffer-v1.xml");
g!("wayland-protocols/unstable/tablet/tablet-unstable-v2.xml");
