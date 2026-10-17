# PRIMARY selection support

SPICE selection 0 remains the normal clipboard; selection 1 is PRIMARY (Unix middle-click). Both ext-data-control and wlr-data-control v2 forward GRAB, REQUEST, DATA and RELEASE independently. wlr v1 retains normal clipboard support without issuing unsupported PRIMARY requests.

The implementation follows upstream C spice-vdagent commit f25c7927cae05ae9cf9838a70bb20b2fb09e6122: separate source, offer, ownership and pending paste state for each selection; source identity determines the selection; MIME lists belong to offer objects; host grabs advertise MIME types but request data only on paste. Data replies run through the main event loop so concurrent selections and interleaved messages remain intact. Pending requests close after three seconds without a reply; reading a reply retains the prior three-second per-read idle deadline.

Build with `cargo build --locked --release`. Run `tests/run-headless.sh` with labwc, wl-copy, wl-paste and Python 3 installed. The tests start an isolated headless compositor and simulated spice-vdagentd; no desktop clipboard, system agent or service is changed.

For diagnostics, `WAYLAND_VDAGENT_DATA_CONTROL=wlr` forces the legacy protocol; the default prefers ext when available. `WAYLAND_VDAGENT_SOCKET` overrides the daemon socket for isolated tests; normally leave it unset.

This is a per-session Rust agent; Debian's packaged spice-vdagent and spice-vdagentd do not need to be modified or replaced. Do not start this binary alongside another session clipboard agent on the real daemon socket.

The automated tests exercise actual Wayland clipboard and PRIMARY transfers against a simulated daemon. Normal copy/paste and physical middle-click paste were also verified interactively in both directions through SPICE on Wayfire.
