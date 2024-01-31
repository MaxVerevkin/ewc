use ewc_wayland_scanner::generate as g;

g!("protocol/wayland.xml");

g!("protocol/ewc-debug.xml");

g!("wayland-protocols/stable/xdg-shell/xdg-shell.xml");
