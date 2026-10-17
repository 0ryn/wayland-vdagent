//! SPICE clipboard bridge for Wayland.
//!
//! Matches the Windows SPICE agent flow: on host GRAB we only *offer* a data
//! source; on paste (compositor's `Send` event) we then issue REQUEST to the
//! daemon and forward the bytes. Eager REQUEST breaks the SPICE grab channel.

mod data_control;
mod file_xfer;
mod monitors;
mod udscs;

use data_control::*;
use std::io::{Read, Write};
use std::os::unix::io::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use udscs::*;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};

const VDAGENTD_SOCKET: &str = "/run/spice-vdagentd/spice-vdagent-sock";

/// Maximum idle gap while reading a requested clipboard payload.
const CLIPBOARD_IDLE_TIMEOUT: Duration = Duration::from_millis(3000);

/// Short deadline for other daemon messages after poll signals readability.
const DAEMON_PEEK_TIMEOUT: Duration = Duration::from_millis(10);

/// For each SPICE clipboard type we care about, the list of Wayland mime types
/// that map to it. The first entry in each list is the "canonical" mime — it's
/// what we use when receiving from a remote offer if we need to pick one.
struct ClipboardFormat {
    spice_type: u32,
    mimes: &'static [&'static str],
}

const FORMATS: &[ClipboardFormat] = &[
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
        mimes: &[
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
            "TEXT",
        ],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_PNG,
        mimes: &["image/png"],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_BMP,
        mimes: &["image/bmp", "image/x-bmp"],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_JPG,
        mimes: &["image/jpeg", "image/jpg"],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_TIFF,
        mimes: &["image/tiff"],
    },
];

/// Given an advertised-mime list (from an incoming offer), return the SPICE
/// types we can serve. Keeps original `FORMATS` order so GRAB types are stable.
fn spice_types_for_mimes(mimes: &[String]) -> Vec<u32> {
    FORMATS
        .iter()
        .filter(|f| f.mimes.iter().any(|m| mimes.iter().any(|om| om == m)))
        .map(|f| f.spice_type)
        .collect()
}

/// Pick the canonical mime for a given advertised-mime list and a wanted SPICE
/// type. Returns `None` if the offer does not advertise any compatible mime.
fn canonical_mime_for(spice_type: u32, mimes: &[String]) -> Option<String> {
    let fmt = FORMATS.iter().find(|f| f.spice_type == spice_type)?;
    for candidate in fmt.mimes {
        if mimes.iter().any(|m| m == candidate) {
            return Some((*candidate).to_string());
        }
    }
    None
}

/// Map the mime the compositor asked for (in a source `Send` event) back to
/// its SPICE clipboard type.
fn spice_type_for_mime(mime: &str) -> Option<u32> {
    FORMATS
        .iter()
        .find(|f| f.mimes.contains(&mime))
        .map(|f| f.spice_type)
}

#[derive(Default)]
struct SelectionState {
    source: Option<Source>,
    offer: Option<Offer>,
    mimes: Vec<String>,
    owned: bool,
    pending: Option<(u32, std::os::fd::OwnedFd, Instant)>,
}

struct AppState {
    seat: Option<WlSeat>,
    manager: Option<Manager>,
    device: Option<Device>,
    daemon: Arc<Mutex<UnixStream>>,
    conn: Option<Connection>,
    // SPICE selection IDs: 0 = normal clipboard, 1 = PRIMARY.
    selections: [SelectionState; 2],
    // Offers are announced before their selection is known. Keep MIME lists
    // attached to object IDs until Selection / PrimarySelection assigns them.
    offers: Vec<(Offer, Vec<String>)>,
    // Globals discovered during first roundtrip; one is bound afterwards.
    ext_manager_name: Option<u32>,
    wlr_manager_name: Option<(u32, u32)>,
    // Tracked output geometry. `width`/`height` are the last values we've
    // already reported to the daemon; `pending_*` are staged from wl_output
    // Mode events and committed on `done`.
    width: i32,
    height: i32,
    pending_width: i32,
    pending_height: i32,
    // In-flight host→guest file transfers.
    xfers: file_xfer::Xfers,
}

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("wayland-vdagent {}", env!("GIT_VERSION"));
        return;
    }
    eprintln!("wayland-vdagent: starting");

    let stream = UnixStream::connect(
        std::env::var("WAYLAND_VDAGENT_SOCKET").unwrap_or_else(|_| VDAGENTD_SOCKET.to_string()),
    )
    .unwrap_or_else(|e| {
        eprintln!("failed to connect to daemon: {}", e);
        std::process::exit(1);
    });
    eprintln!("wayland-vdagent: daemon connected");

    let daemon = Arc::new(Mutex::new(stream));

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland");
    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = AppState {
        seat: None,
        manager: None,
        device: None,
        daemon: daemon.clone(),
        conn: Some(conn.clone()),
        selections: std::array::from_fn(|_| SelectionState::default()),
        offers: Vec::new(),
        ext_manager_name: None,
        wlr_manager_name: None,
        width: 0,
        height: 0,
        pending_width: 0,
        pending_height: 0,
        xfers: file_xfer::Xfers::new(),
    };

    let registry = conn.display().get_registry(&qh, ());
    event_queue.roundtrip(&mut state).expect("roundtrip failed");

    // A diagnostic override allows exercising wlr on compositors that expose both.
    if std::env::var("WAYLAND_VDAGENT_DATA_CONTROL").as_deref() == Ok("wlr") {
        state.ext_manager_name = None;
    }
    // Now that all globals are known, bind exactly one data-control manager.
    state.manager = if let Some(name) = state.ext_manager_name {
        eprintln!("wayland-vdagent: using ext-data-control-v1");
        Some(Manager::Ext(
            registry.bind::<ExtDataControlManagerV1, _, _>(name, 1, &qh, ()),
        ))
    } else if let Some((name, version)) = state.wlr_manager_name {
        eprintln!("wayland-vdagent: using wlr-data-control (legacy)");
        Some(Manager::Wlr(
            registry.bind::<ZwlrDataControlManagerV1, _, _>(name, version.min(2), &qh, ()),
        ))
    } else {
        None
    };

    let (Some(manager), Some(seat)) = (&state.manager, &state.seat) else {
        eprintln!("wayland-vdagent: missing manager or seat");
        std::process::exit(1);
    };
    state.device = Some(manager.get_data_device(seat, &qh));

    // Second roundtrip so wl_output Mode/Done events arrive (they come after
    // the global advertisement we processed above).
    event_queue.roundtrip(&mut state).expect("roundtrip failed");

    if state.width == 0 || state.height == 0 {
        eprintln!("wayland-vdagent: no wl_output geometry yet, falling back to 1280x800");
        state.width = 1280;
        state.height = 800;
        send_resolution(&state.daemon, state.width, state.height);
    }
    eprintln!(
        "wayland-vdagent: Wayland ready ({}x{})",
        state.width, state.height
    );

    let daemon_fd = daemon.lock().unwrap().as_raw_fd();
    let wayland_fd = conn.as_fd().as_raw_fd();

    loop {
        conn.flush().ok();

        let mut fds = [
            PollFd {
                fd: daemon_fd,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: wayland_fd,
                events: POLLIN,
                revents: 0,
            },
        ];
        let timeout = if state.selections.iter().any(|s| s.pending.is_some()) {
            100
        } else {
            -1
        };
        unsafe { poll(fds.as_mut_ptr(), 2, timeout) };
        for selection in &mut state.selections {
            if selection
                .pending
                .as_ref()
                .is_some_and(|(_, _, start)| start.elapsed() >= CLIPBOARD_IDLE_TIMEOUT)
            {
                selection.pending = None;
            }
        }

        if fds[0].revents & POLLIN != 0 {
            let mut d = daemon.lock().unwrap();
            let read_timeout = if state.selections.iter().any(|s| s.pending.is_some()) {
                CLIPBOARD_IDLE_TIMEOUT
            } else {
                DAEMON_PEEK_TIMEOUT
            };
            d.set_read_timeout(Some(read_timeout)).ok();
            match read_msg(&mut d) {
                Ok(msg) => {
                    drop(d);
                    handle_daemon_msg(&mut state, &qh, &msg);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    eprintln!("wayland-vdagent: daemon error: {}", e);
                    break;
                }
            }
        }

        if fds[1].revents & POLLIN != 0 {
            if let Some(guard) = conn.prepare_read() {
                guard.read().ok();
            }
            event_queue.dispatch_pending(&mut state).ok();
        }
    }
}

fn send_resolution(daemon: &Mutex<UnixStream>, width: i32, height: i32) {
    let d = daemon.lock().unwrap();
    let res = guest_xorg_resolution(width, height, 0, 0, 0);
    send_msg(
        &d,
        VDAGENTD_GUEST_XORG_RESOLUTION,
        width as u32,
        height as u32,
        &res,
    );
}

fn handle_daemon_msg(state: &mut AppState, qh: &QueueHandle<AppState>, msg: &UdscsMsg) {
    match msg.msg_type {
        VDAGENTD_VERSION => {
            let ver = String::from_utf8_lossy(&msg.data);
            eprintln!(
                "wayland-vdagent: daemon version: {}",
                ver.trim_end_matches('\0')
            );
        }
        VDAGENTD_GRAPHICS_DEVICE_INFO => {
            eprintln!(
                "wayland-vdagent: graphics device info, resending resolution {}x{}",
                state.width, state.height
            );
            send_resolution(&state.daemon, state.width, state.height);
        }
        VDAGENTD_CLIPBOARD_GRAB => {
            let sel = msg.arg1 as usize;
            if sel >= 2
                || !state
                    .device
                    .as_ref()
                    .is_some_and(|d| d.supports_selection(sel))
            {
                return;
            }
            // Collect the SPICE types advertised by the host and map each to
            // the full set of Wayland mimes we can offer for that type.
            let host_types: Vec<u32> = msg
                .data
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            let mimes_to_offer: Vec<&'static str> = FORMATS
                .iter()
                .filter(|f| host_types.contains(&f.spice_type))
                .flat_map(|f| f.mimes.iter().copied())
                .collect();
            if mimes_to_offer.is_empty() {
                eprintln!("wayland-vdagent: host GRAB with no supported types ({host_types:?})",);
                return;
            }

            eprintln!(
                "wayland-vdagent: host GRAB — offering {} mimes for types {host_types:?}",
                mimes_to_offer.len()
            );

            // Fresh source per grab — the previous one may already be cancelled.
            if let Some(manager) = &state.manager {
                state.selections[sel].pending = None;
                if let Some(old) = state.selections[sel].source.take() {
                    old.destroy();
                }
                let source = manager.create_data_source(qh);
                for mime in &mimes_to_offer {
                    source.offer((*mime).to_string());
                }
                if let Some(device) = &state.device {
                    device.set_selection(sel, Some(&source));
                }
                state.selections[sel].source = Some(source);
                state.selections[sel].owned = true;
            }
        }
        VDAGENTD_CLIPBOARD_DATA => {
            if let Some(selection) = state.selections.get_mut(msg.arg1 as usize) {
                if let Some((wanted, fd, _)) = selection.pending.take() {
                    if wanted == msg.arg2 {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&msg.data);
                    }
                }
            }
        }
        VDAGENTD_CLIPBOARD_REQUEST => {
            let sel = msg.arg1 as usize;
            if sel >= 2 {
                return;
            }
            let wanted_type = msg.arg2;
            eprintln!("wayland-vdagent: host requests our clipboard (type={wanted_type})");
            let data = state.selections[sel].offer.as_ref().and_then(|offer| {
                canonical_mime_for(wanted_type, &state.selections[sel].mimes)
                    .and_then(|mime| read_offer(offer, &mime, state.conn.as_ref()))
            });
            let d = state.daemon.lock().unwrap();
            match data {
                Some(bytes) if !bytes.is_empty() => {
                    eprintln!(
                        "wayland-vdagent: sending {} bytes to host (type={wanted_type})",
                        bytes.len()
                    );
                    send_msg(&d, VDAGENTD_CLIPBOARD_DATA, msg.arg1, wanted_type, &bytes);
                }
                _ => {
                    eprintln!("wayland-vdagent: no data from offer");
                    send_msg(
                        &d,
                        VDAGENTD_CLIPBOARD_DATA,
                        msg.arg1,
                        VD_AGENT_CLIPBOARD_NONE,
                        &[],
                    );
                }
            }
        }
        VDAGENTD_CLIPBOARD_RELEASE => {
            if let Some(selection) = state.selections.get_mut(msg.arg1 as usize) {
                if selection.owned {
                    if let Some(device) = &state.device {
                        device.set_selection(msg.arg1 as usize, None);
                    }
                }
                selection.owned = false;
                selection.pending = None;
                if let Some(source) = selection.source.take() {
                    source.destroy();
                }
            }
        }
        VDAGENTD_CLIENT_DISCONNECTED => {
            for (sel, selection) in state.selections.iter_mut().enumerate() {
                if selection.owned {
                    if let Some(device) = &state.device {
                        device.set_selection(sel, None);
                    }
                }
                selection.owned = false;
                selection.pending = None;
                if let Some(source) = selection.source.take() {
                    source.destroy();
                }
            }
        }
        VDAGENTD_MONITORS_CONFIG => {
            if let Some((w, h)) = monitors::parse_first_monitor(&msg.data) {
                eprintln!("wayland-vdagent: host requests resize to {w}x{h}");
                monitors::apply(w, h);
                // The new size will flow back via `wl_output::Mode` →
                // `Done` → `send_resolution`, no manual ack needed.
            } else {
                eprintln!("wayland-vdagent: malformed MONITORS_CONFIG payload");
            }
        }
        VDAGENTD_FILE_XFER_START => {
            // id and keyfile payload are packed in the body; arg1/arg2 are 0.
            state.xfers.start(&state.daemon, &msg.data);
        }
        VDAGENTD_FILE_XFER_STATUS => {
            // Host is telling us about a transfer: CANCELLED, ERROR, etc.
            // Body layout: u32 id, u32 result, u8 data[].
            state.xfers.remote_status(&msg.data);
        }
        VDAGENTD_FILE_XFER_DATA => {
            // Body layout: u32 id, u64 size, u8 data[].
            state.xfers.data(&state.daemon, &msg.data);
        }
        VDAGENTD_FILE_XFER_DISABLE => {
            eprintln!("wayland-vdagent: file transfer disabled by daemon");
        }
        VDAGENTD_AUDIO_VOLUME_SYNC => {}
        other => {
            eprintln!("wayland-vdagent: unhandled msg type={}", other);
        }
    }
}

// Wayland dispatchers

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ()));
                }
                "wl_output" => {
                    // We only care about the first output — SPICE reports one.
                    // Binding additional outputs is harmless (dispatch is a noop
                    // if we already have geometry), but avoid leaking extras.
                    let _ = registry.bind::<WlOutput, _, _>(name, 2, qh, ());
                }
                EXT_MANAGER_INTERFACE => state.ext_manager_name = Some(name),
                WLR_MANAGER_INTERFACE => state.wlr_manager_name = Some((name, version)),
                _ => {}
            }
        }
    }
}

delegate_noop!(AppState: ignore WlSeat);
delegate_noop!(AppState: ignore ZwlrDataControlManagerV1);
delegate_noop!(AppState: ignore ExtDataControlManagerV1);

impl Dispatch<ZwlrDataControlOfferV1, ()> for AppState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            handle_offer_mime(state, offer.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for AppState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            handle_offer_mime(state, offer.id(), mime_type);
        }
    }
}

impl Dispatch<WlOutput, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                // Only the current mode is interesting; compositors also advertise
                // non-current modes when enumerating the output's capabilities.
                let is_current = match flags {
                    WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                    _ => false,
                };
                if is_current {
                    state.pending_width = width;
                    state.pending_height = height;
                }
            }
            wl_output::Event::Done => {
                if state.pending_width > 0
                    && state.pending_height > 0
                    && (state.pending_width != state.width || state.pending_height != state.height)
                {
                    state.width = state.pending_width;
                    state.height = state.pending_height;
                    eprintln!(
                        "wayland-vdagent: output resized to {}x{}",
                        state.width, state.height
                    );
                    send_resolution(&state.daemon, state.width, state.height);
                }
            }
            _ => {}
        }
    }
}

fn handle_data_offer(state: &mut AppState, offer: Offer) {
    state.offers.push((offer, Vec::new()));
}

fn handle_offer_mime(state: &mut AppState, id: wayland_client::backend::ObjectId, mime: String) {
    if let Some((_, mimes)) = state.offers.iter_mut().find(|(o, _)| o.id() == id) {
        mimes.push(mime);
    }
}

fn handle_selection(
    state: &mut AppState,
    sel: usize,
    id: Option<wayland_client::backend::ObjectId>,
) {
    let incoming = id
        .and_then(|id| state.offers.iter().position(|(o, _)| o.id() == id))
        .map(|i| state.offers.remove(i));
    let selection = &mut state.selections[sel];
    if let Some(old) = selection.offer.take() {
        old.destroy();
    }
    selection.mimes.clear();
    if selection.owned {
        if let Some((offer, _)) = incoming {
            offer.destroy();
        }
        return;
    }
    if let Some((offer, mimes)) = incoming {
        selection.offer = Some(offer);
        selection.mimes = mimes;
    }
    let types = spice_types_for_mimes(&selection.mimes);
    let d = state.daemon.lock().unwrap();
    if types.is_empty() {
        send_msg(&d, VDAGENTD_CLIPBOARD_RELEASE, sel as u32, 0, &[]);
    } else {
        let payload: Vec<u8> = types.iter().flat_map(|t| t.to_le_bytes()).collect();
        send_msg(&d, VDAGENTD_CLIPBOARD_GRAB, sel as u32, 0, &payload);
    }
}

/// Drain `offer` into a `Vec` via a pipe for a given mime type.
/// Returns `None` if pipe creation fails.
fn read_offer(offer: &Offer, mime: &str, conn: Option<&Connection>) -> Option<Vec<u8>> {
    let mut fds = [0i32; 2];
    if unsafe { libc_pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let write_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
    offer.receive(mime.to_string(), write_fd.as_fd());
    // Flush so the compositor actually sees the receive request, then close
    // our write end so the subsequent read terminates with EOF.
    if let Some(c) = conn {
        c.flush().ok();
    }
    drop(write_fd);

    let mut read_file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut data = Vec::new();
    let _ = Read::read_to_end(&mut read_file, &mut data);
    Some(data)
}

// Defer REQUEST until paste, and keep each selection's reply independent.
// Reading DATA in the main loop also preserves interleaved daemon messages.
fn handle_source_send(
    state: &mut AppState,
    id: wayland_client::backend::ObjectId,
    mime_type: String,
    fd: std::os::fd::OwnedFd,
) {
    let Some(sel) = state
        .selections
        .iter()
        .position(|s| s.source.as_ref().is_some_and(|s| s.id() == id))
    else {
        return;
    };
    let Some(spice_type) = spice_type_for_mime(&mime_type) else {
        return;
    };
    if state.selections[sel].pending.is_some() {
        return;
    }
    state.selections[sel].pending = Some((spice_type, fd, Instant::now()));
    let d = state.daemon.lock().unwrap();
    send_msg(&d, VDAGENTD_CLIPBOARD_REQUEST, sel as u32, spice_type, &[]);
}

fn handle_source_cancelled(state: &mut AppState, id: wayland_client::backend::ObjectId) {
    if let Some(selection) = state
        .selections
        .iter_mut()
        .find(|s| s.source.as_ref().is_some_and(|s| s.id() == id))
    {
        selection.owned = false;
        selection.pending = None;
        if let Some(source) = selection.source.take() {
            source.destroy();
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for AppState {
    event_created_child!(AppState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ()),
    ]);
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                handle_data_offer(state, Offer::Wlr(id))
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                handle_selection(state, 0, id.map(|o| o.id()))
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                handle_selection(state, 1, id.map(|o| o.id()))
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                handle_source_send(state, source.id(), mime_type, fd)
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                handle_source_cancelled(state, source.id())
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for AppState {
    event_created_child!(AppState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ()),
    ]);
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                handle_data_offer(state, Offer::Ext(id))
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                handle_selection(state, 0, id.map(|o| o.id()))
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                handle_selection(state, 1, id.map(|o| o.id()))
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                handle_source_send(state, source.id(), mime_type, fd)
            }
            ext_data_control_source_v1::Event::Cancelled => {
                handle_source_cancelled(state, source.id())
            }
            _ => {}
        }
    }
}

fn guest_xorg_resolution(w: i32, h: i32, x: i32, y: i32, id: i32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&w.to_le_bytes());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf.extend_from_slice(&id.to_le_bytes());
    buf
}

const POLLIN: i16 = 1;
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
extern "C" {
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    #[link_name = "pipe"]
    fn libc_pipe(fds: *mut i32) -> i32;
}
