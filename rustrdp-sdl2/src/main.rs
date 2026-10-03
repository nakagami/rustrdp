use rustrdp_core::bitmap::Bitmap;
use rustrdp_core::client::{PointerEvent, RdpEvent};
use rustrdp_core::protocol::rdpsnd::AudioFormat;
use rustrdp_sdl2::config::RdpConfig;
use rustrdp_sdl2::connection::RdpConnection;
use rustrdp_sdl2::input::InputHandler;
use rustrdp_sdl2::ui::RdpUI;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use sdl2::mouse::Cursor;
use sdl2::pixels::PixelFormatEnum;
use sdl2::surface::Surface;
use std::collections::HashMap;
use std::error::Error;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Audio queue soft cap: drop incoming audio when the SDL2 device queue exceeds
/// this many bytes.  ≈ 1 s of 44 100 Hz / stereo / 16-bit PCM.
/// Prevents ever-growing playback latency when the application falls behind the
/// server's audio stream.  Matches grdpsdl2's maxAudioQueueBytes = 176 400.
const MAX_AUDIO_QUEUE_BYTES: u32 = 176_400;

/// If no data of any kind is received from the server for this duration after
/// the session was active, the H.264 decoder or network connection is assumed
/// to be stuck and the session is automatically reconnected.
/// Matches grdpsdl2's videoStallTimeout = 10 * time.Second.
const VIDEO_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum idle time after the last SDL window-resize event before the
/// reconnect with the new resolution is triggered.  Coalesces rapid resize
/// events (e.g. live dragging) into a single reconnect.
/// Matches grdpsdl2's resizeTime debounce of 500 ms.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Maximum time allowed for a single TCP+RDP handshake.  If the server does
/// not complete the handshake within this window the attempt is aborted and
/// retried with backoff.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of consecutive failed connection / session attempts before
/// the application gives up and exits.  Successful sessions (where the server
/// sent at least one RdpEvent::Ready) reset the counter to zero.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Exponential backoff for reconnect retries.
/// Returns 100 ms, 500 ms, 2 500 ms, 12 500 ms, 30 000 ms for attempts 1–5+.
fn reconnect_backoff(attempt: u32) -> Duration {
    let ms = 100u64.saturating_mul(5u64.pow(attempt.saturating_sub(1).min(4)));
    Duration::from_millis(ms.min(30_000))
}

/// Convert RDP pointer update data to a flat RGBA pixel buffer (top-down).
///
/// Supports xor_bpp = 1, 24, and 32. Returns `None` for empty or unsupported cursors.
fn build_cursor_rgba(
    xor_bpp: u16,
    width: u16,
    height: u16,
    and_mask: &[u8],
    xor_data: &[u8],
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let w = width as usize;
    let h = height as usize;
    // AND mask rows are 16-bit aligned.
    let and_stride = ((w + 15) >> 4) << 1;

    let and_bit = |x: usize, y: usize| -> u8 {
        let src_y = h - 1 - y; // stored bottom-up
        let o = src_y * and_stride + (x >> 3);
        if o >= and_mask.len() {
            return 1;
        }
        (and_mask[o] >> (7 - (x & 7))) & 1
    };

    let mut rgba = vec![0u8; w * h * 4];
    match xor_bpp {
        1 => {
            let xor_stride = and_stride;
            for y in 0..h {
                let src_y = h - 1 - y;
                for x in 0..w {
                    let a = and_bit(x, y);
                    let xo = xor_stride * src_y + (x >> 3);
                    let xb = if xo < xor_data.len() {
                        (xor_data[xo] >> (7 - (x & 7))) & 1
                    } else {
                        0
                    };
                    let o = (y * w + x) * 4;
                    match (a, xb) {
                        (0, 0) => { rgba[o]=0;   rgba[o+1]=0;   rgba[o+2]=0;   rgba[o+3]=255; }
                        (0, 1) => { rgba[o]=255; rgba[o+1]=255; rgba[o+2]=255; rgba[o+3]=255; }
                        (1, 0) => {} // transparent (already 0)
                        _      => { rgba[o]=0;   rgba[o+1]=0;   rgba[o+2]=0;   rgba[o+3]=255; }
                    }
                }
            }
        }
        32 => {
            let stride = w * 4;
            for y in 0..h {
                let src_y = h - 1 - y;
                for x in 0..w {
                    let s = src_y * stride + x * 4;
                    if s + 3 >= xor_data.len() {
                        continue;
                    }
                    let o = (y * w + x) * 4;
                    let (b, g, r, a) = (xor_data[s], xor_data[s+1], xor_data[s+2], xor_data[s+3]);
                    if a != 0 {
                        rgba[o]=r; rgba[o+1]=g; rgba[o+2]=b; rgba[o+3]=a;
                    } else if and_bit(x, y) == 0 {
                        rgba[o]=r; rgba[o+1]=g; rgba[o+2]=b; rgba[o+3]=255;
                    }
                    // else transparent
                }
            }
        }
        24 => {
            let stride = ((w * 3 + 1) >> 1) << 1;
            for y in 0..h {
                let src_y = h - 1 - y;
                for x in 0..w {
                    let s = src_y * stride + x * 3;
                    if s + 2 >= xor_data.len() {
                        continue;
                    }
                    let o = (y * w + x) * 4;
                    if and_bit(x, y) == 0 {
                        rgba[o]=xor_data[s+2]; rgba[o+1]=xor_data[s+1]; rgba[o+2]=xor_data[s]; rgba[o+3]=255;
                    }
                    // else transparent
                }
            }
        }
        _ => {
            log::warn!("[cursor] unsupported xor_bpp={}", xor_bpp);
            return None;
        }
    }
    Some(rgba)
}

/// Commands sent from the SDL event loop to the RDP I/O task.
///
/// Decoupling input from display processing mirrors grdpsdl2's goroutine model:
/// the SDL loop never blocks on a network write, keeping event polling responsive.
#[derive(Debug)]
enum InputCmd {
    KeyDown { flags: u16, scancode: u8 },
    KeyUp { flags: u16, scancode: u8 },
    MouseMove { x: u16, y: u16 },
    MouseButton { btn: u8, down: bool, x: u16, y: u16 },
    MouseWheel { delta: i16 },
    ForceRefresh,
}

// Use current_thread runtime so that spawn_local works without requiring Send bounds.
// RdpSession<SimpleTransport> is !Send because the Transport trait uses ?Send.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // LocalSet is required for spawn_local (non-Send futures on the current thread).
    tokio::task::LocalSet::new().run_until(run()).await
}

async fn run() -> Result<(), Box<dyn Error>> {
    // Load configuration from environment
    let config = RdpConfig::from_env_and_args().map_err(|e| {
        eprintln!(
            "Configuration error: {}. Please set the following environment variables:",
            e
        );
        eprintln!("  RDP_HOST     - RDP server hostname");
        eprintln!("  RDP_PORT     - RDP server port");
        eprintln!("  RDP_USER     - Username");
        eprintln!("  RDP_PASSWORD - Password");
        eprintln!("  RDP_DOMAIN   - Domain (optional)");
        eprintln!("  RDP_WINDOW_SIZE - Window size (default: 1280x800)");
        e
    })?;

    // Initialize SDL2 subsystems once; they are reused across reconnects.
    let sdl_context = sdl2::init()?;
    let audio_subsystem = sdl_context.audio()?;
    let input_handler = InputHandler::new(config.swap_alt_meta);
    let mut rdp_ui = RdpUI::new(&sdl_context, config.width, config.height, "RDP Client")?;
    let mut event_pump = sdl_context.event_pump()?;
    let mut cursor_cache: HashMap<u16, Cursor> = HashMap::new();
    let mut cursor_visible = true;

    // Track current session dimensions; updated on window resize reconnect.
    let mut session_width = config.width;
    let mut session_height = config.height;
    // Consecutive failure counter: reset to 0 on the first Ready event of a
    // session.  Incremented on connection timeout, connect error, or unexpected
    // mid-session disconnect.  Application exits when it reaches MAX_RECONNECT_ATTEMPTS.
    let mut consecutive_failures: u32 = 0;

    // Outer reconnect loop.  Each iteration establishes one RDP session.
    // Exits when the user quits or a fatal error prevents reconnection.
    'session: loop {
        // Apply any dimension change from the previous iteration's resize.
        if rdp_ui.get_window_size() != (session_width, session_height) {
            if let Err(e) = rdp_ui.resize(session_width, session_height) {
                log::error!("Failed to resize UI before reconnect: {}", e);
            }
        }

        log::info!(
            "Starting RDP client {}x{} → {}:{}",
            session_width, session_height,
            config.host, config.port
        );

        let mut session_config = config.clone();
        session_config.width = session_width;
        session_config.height = session_height;

        let rdp_session = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            RdpConnection::connect(&session_config),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                consecutive_failures += 1;
                log::error!(
                    "Connection failed (attempt {}/{}): {}",
                    consecutive_failures, MAX_RECONNECT_ATTEMPTS, e
                );
                if consecutive_failures >= MAX_RECONNECT_ATTEMPTS {
                    log::error!("Max reconnect attempts reached, giving up");
                    break 'session;
                }
                let delay = reconnect_backoff(consecutive_failures);
                log::info!("Retrying in {:?}", delay);
                tokio::time::sleep(delay).await;
                continue 'session;
            }
            Err(_) => {
                consecutive_failures += 1;
                log::error!(
                    "Connection timed out after {:?} (attempt {}/{})",
                    CONNECT_TIMEOUT, consecutive_failures, MAX_RECONNECT_ATTEMPTS
                );
                if consecutive_failures >= MAX_RECONNECT_ATTEMPTS {
                    log::error!("Max reconnect attempts reached, giving up");
                    break 'session;
                }
                let delay = reconnect_backoff(consecutive_failures);
                log::info!("Retrying in {:?}", delay);
                tokio::time::sleep(delay).await;
                continue 'session;
            }
        };

        // Reset per-session audio state on each (re)connect.
        let mut audio_queue: Option<AudioQueue<i16>> = None;
        let mut audio_fmt: Option<AudioFormat> = None;

        let (input_tx, mut input_rx) = mpsc::channel::<InputCmd>(64);
        let (event_tx, mut event_rx) = mpsc::channel::<RdpEvent>(64);
        // Separate low-capacity channel for NV12 video frames.
        // Capacity 2: if the SDL loop falls behind, stale frames are dropped
        // via try_send so only the latest frame is rendered.
        // Mirrors grdpsdl2's yuvCh = 4 with select { default: } drop logic.
        let (video_tx, mut video_rx) = mpsc::channel::<RdpEvent>(2);

        // --- RDP I/O task ---
        //
        // Mirrors grdpsdl2's goroutine: RecvEvent() → event channel.
        // Runs concurrently on the same OS thread via cooperative multitasking.
        let task = tokio::task::spawn_local(async move {
            let mut session = rdp_session;
            loop {
                tokio::select! {
                    biased;
                    cmd_opt = input_rx.recv() => {
                        match cmd_opt {
                            Some(cmd) => {
                                let result = match cmd {
                                    InputCmd::KeyDown { flags, scancode } => {
                                        session.send_key_down(flags, scancode).await
                                    }
                                    InputCmd::KeyUp { flags, scancode } => session.send_key_up(flags, scancode).await,
                                    InputCmd::MouseMove { x, y } => session.send_mouse_move(x, y).await,
                                    InputCmd::MouseButton { btn, down, x, y } => {
                                        session.send_mouse_button(btn, down, x, y).await
                                    }
                                    InputCmd::MouseWheel { delta } => session.send_mouse_wheel(delta).await,
                                    InputCmd::ForceRefresh => session.send_force_refresh().await,
                                };
                                if let Err(e) = result {
                                    log::error!("Input send error: {}", e);
                                    return;
                                }
                            }
                            None => return, // SDL loop exited (input_tx dropped)
                        }
                    }
                    res = session.recv_event() => {
                        match res {
                            Ok(event) => {
                                // NV12 video frames go through the dedicated drop-on-full
                                // channel so that stale frames are discarded when the SDL
                                // loop is momentarily behind, matching grdpsdl2's non-blocking
                                // channel send for YUV frames.
                                if matches!(event, RdpEvent::NV12Frame(_)) {
                                    let _ = video_tx.try_send(event);
                                } else {
                                    let deactivated = matches!(event, RdpEvent::Deactivated);
                                    if event_tx.send(event).await.is_err() {
                                        break; // SDL side dropped — application is exiting
                                    }
                                    if deactivated {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("RDP session error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        });

        log::info!("RDP client ready");

        // --- SDL main loop ---
        //
        // last_server_activity arms the video-stall watchdog.  None means the
        // watchdog is disarmed (no data received yet this session).
        let session_start = Instant::now();
        let mut ever_showed_frame = false;
        let mut never_shown_last_keyframe: Option<Instant> = None;
        let mut last_server_activity: Option<Instant> = None;
        // pending_resize: (new_w, new_h, timestamp of last resize event).
        // After RESIZE_DEBOUNCE idle time the session reconnects with the new size.
        let mut pending_resize: Option<(u16, u16, Instant)> = None;
        // Set to Some((w, h)) when the loop should exit for a reconnect.
        let mut reconnect_dims: Option<(u16, u16)> = None;
        // Set to true when the user explicitly quits (Escape / window close).
        // Prevents unexpected server-side disconnects from triggering a reconnect.
        let mut user_quit = false;
        let mut running = true;

        while running {
            // Step 1: process SDL input events.
            // IMPORTANT: never call .await inside poll_iter(). If we block on
            // input_tx.send().await while still inside the iterator, event_rx
            // cannot be drained (Step 2 is unreachable), which in turn blocks
            // the RDP I/O task on event_tx.send().await — a deadlock on the
            // single-threaded executor.  Collect keyboard/mouse-button commands
            // in a Vec; send them after event_rx is drained below.
            let mut pending_wheel: i32 = 0;
            let mut pending_inputs: Vec<InputCmd> = Vec::new();
            for event in event_pump.poll_iter() {
                match event {
                    Event::Quit { .. } => {
                        user_quit = true;
                        running = false;
                        break;
                    }
                    Event::KeyDown {
                        keycode: Some(keycode),
                        ..
                    } => {
                        if keycode == Keycode::Escape {
                            user_quit = true;
                            running = false;
                        } else if let Some((flags, scancode)) =
                            input_handler.handle_keyboard_event(keycode, true)
                        {
                            pending_inputs.push(InputCmd::KeyDown { flags, scancode });
                        }
                    }
                    Event::KeyUp {
                        keycode: Some(keycode),
                        ..
                    } => {
                        if let Some((flags, scancode)) =
                            input_handler.handle_keyboard_event(keycode, false)
                        {
                            pending_inputs.push(InputCmd::KeyUp { flags, scancode });
                        }
                    }
                    Event::MouseMotion { x, y, .. } => {
                        // Mouse moves are high-frequency: use try_send so that only
                        // the latest position is forwarded when the channel is full.
                        input_tx
                            .try_send(InputCmd::MouseMove {
                                x: x as u16,
                                y: y as u16,
                            })
                            .ok();
                    }
                    Event::MouseButtonDown {
                        mouse_btn, x, y, ..
                    } => {
                        if let Some((btn, _)) =
                            input_handler.handle_mouse_button(mouse_btn, true)
                        {
                            pending_inputs.push(InputCmd::MouseButton {
                                btn,
                                down: true,
                                x: x as u16,
                                y: y as u16,
                            });
                        }
                    }
                    Event::MouseButtonUp {
                        mouse_btn, x, y, ..
                    } => {
                        if let Some((btn, _)) =
                            input_handler.handle_mouse_button(mouse_btn, false)
                        {
                            pending_inputs.push(InputCmd::MouseButton {
                                btn,
                                down: false,
                                x: x as u16,
                                y: y as u16,
                            });
                        }
                    }
                    Event::MouseWheel { x, y, .. } => {
                        if let Some(delta) = input_handler.handle_mouse_wheel(x, y) {
                            pending_wheel += delta as i32;
                        }
                    }
                    Event::Window {
                        win_event:
                            WindowEvent::Exposed
                            | WindowEvent::Restored
                            | WindowEvent::FocusGained,
                        ..
                    } => {
                        if let Err(e) = rdp_ui.repaint() {
                            log::error!("Failed to repaint: {}", e);
                        }
                    }
                    // User resized the window: debounce and reconnect with the
                    // new resolution so the server re-encodes at the correct size.
                    // Matches grdpsdl2's resizePending / resizeTime pattern.
                    Event::Window {
                        win_event: WindowEvent::Resized(data1, data2),
                        ..
                    } => {
                        let rw = (data1 as u16).max(1);
                        let rh = (data2 as u16).max(1);
                        pending_resize = Some((rw, rh, Instant::now()));
                    }
                    _ => {}
                }
            }

            // Flush accumulated wheel delta as a single event.
            if pending_wheel != 0 {
                let clamped =
                    pending_wheel.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                input_tx
                    .try_send(InputCmd::MouseWheel { delta: clamped })
                    .ok();
            }

            // Check resize debounce: reconnect once the window has been stable
            // for RESIZE_DEBOUNCE after the last resize event.
            if let Some((rw, rh, rt)) = pending_resize {
                if rt.elapsed() >= RESIZE_DEBOUNCE {
                    log::info!("Window resized to {}x{}, reconnecting", rw, rh);
                    reconnect_dims = Some((rw, rh));
                    pending_resize = None;
                    running = false;
                }
            }

            // Watchdog for initial black screen / never-shown-a-frame:
            // Send ForceRefresh and MouseMove at 2.5s if no genuine frame has been shown yet.
            if !ever_showed_frame && pending_resize.is_none() {
                let elapsed = session_start.elapsed();
                if elapsed >= Duration::from_millis(2500)
                    && (never_shown_last_keyframe.is_none()
                        || never_shown_last_keyframe.unwrap().elapsed() >= Duration::from_secs(2))
                {
                    log::warn!(
                        "Black screen, sending ForceRefresh and MouseMove sinceStart={:.3}s",
                        elapsed.as_secs_f64()
                    );
                    pending_inputs.push(InputCmd::MouseMove { x: 500, y: 500 });
                    pending_inputs.push(InputCmd::ForceRefresh);
                    never_shown_last_keyframe = Some(Instant::now());
                }
                if elapsed > VIDEO_STALL_TIMEOUT {
                    log::warn!(
                        "Video stalled without ever showing a frame, reconnecting stalled={:.3}s",
                        elapsed.as_secs_f64()
                    );
                    reconnect_dims = Some((session_width, session_height));
                    running = false;
                }
            }

            // Video stall watchdog: if server traffic stops for VIDEO_STALL_TIMEOUT
            // (and no resize is pending), check whether the I/O task is still alive.
            // If the task has already exited (network error), reconnect immediately.
            // If the task is still running (server is genuinely idle), just reset
            // the timer — avoiding spurious reconnects during idle sessions.
            // Mirrors grdpsdl2: reconnect only when connectionErrorPending.
            if let Some(last) = last_server_activity {
                if pending_resize.is_none()
                    && reconnect_dims.is_none()
                    && last.elapsed() >= VIDEO_STALL_TIMEOUT
                {
                    if task.is_finished() {
                        log::warn!(
                            "Video stalled for {:?} and I/O task ended, reconnecting",
                            last.elapsed()
                        );
                        reconnect_dims = Some((session_width, session_height));
                        running = false;
                    } else {
                        // Connection is alive — server is just idle.  Reset the
                        // watchdog so we do not spin reconnects on an idle desktop.
                        log::debug!(
                            "Video idle for {:?} but connection is alive, resetting watchdog",
                            last.elapsed()
                        );
                        last_server_activity = Some(Instant::now());
                    }
                }
            }

            // Step 2: drain all available RDP display events (non-blocking).
            // Accumulate all Bitmap and NV12 events, then call canvas.present()
            // exactly once per SDL loop iteration — multiple GPU flips per 4 ms
            // cycle are expensive and block the tokio executor on VSync.
            let mut pending_bitmaps: Vec<Bitmap> = Vec::new();
            let mut nv12_updated = false;
            loop {
                match event_rx.try_recv() {
                    Ok(RdpEvent::Ready) => {
                        log::info!("RDP session ready");
                        last_server_activity = Some(Instant::now());
                        // Successful session: reset the failure counter so a
                        // future transient disconnect gets the full retry budget.
                        consecutive_failures = 0;
                    }
                    Ok(RdpEvent::Resize { width, height }) => {
                        log::debug!("[rdp-sdl2] Resize {}x{}", width, height);
                        // Flush accumulated bitmaps at the old resolution first.
                        if !pending_bitmaps.is_empty() {
                            if let Err(e) = rdp_ui.update_screen(&pending_bitmaps) {
                                log::error!("Failed to update screen: {}", e);
                            }
                            pending_bitmaps.clear();
                        }
                        if let Err(e) = rdp_ui.resize(width, height) {
                            log::error!("Failed to resize UI: {}", e);
                        }
                    }
                    Ok(RdpEvent::Bitmap(bitmaps)) => {
                        last_server_activity = Some(Instant::now());
                        if !bitmaps.is_empty() {
                            ever_showed_frame = true;
                        }
                        if log::log_enabled!(log::Level::Trace) {
                            if let Some(first) = bitmaps.first() {
                                log::trace!(
                                    "[rdp-sdl2] Bitmap count={} first=({},{}-{},{} {}x{} bpp={})",
                                    bitmaps.len(),
                                    first.dest_left,
                                    first.dest_top,
                                    first.dest_right,
                                    first.dest_bottom,
                                    first.width,
                                    first.height,
                                    first.bits_per_pixel
                                );
                            }
                        }
                        pending_bitmaps.extend(bitmaps);
                    }
                    Ok(RdpEvent::Deactivated) => {
                        log::info!("RDP session deactivated");
                        running = false;
                        break;
                    }
                    Ok(RdpEvent::Audio { format, data }) => {
                        last_server_activity = Some(Instant::now());
                        let needs_open = audio_fmt.as_ref().map_or(true, |f| {
                            f.channels != format.channels
                                || f.sample_rate != format.sample_rate
                                || f.bits_per_sample != format.bits_per_sample
                        });
                        if needs_open {
                            let desired = AudioSpecDesired {
                                freq: Some(format.sample_rate as i32),
                                channels: Some(format.channels as u8),
                                samples: None,
                            };
                            match audio_subsystem.open_queue::<i16, _>(None, &desired) {
                                Ok(q) => {
                                    q.resume();
                                    log::info!(
                                        "[audio] opened queue: {}Hz {}ch {}bit",
                                        format.sample_rate,
                                        format.channels,
                                        format.bits_per_sample
                                    );
                                    audio_queue = Some(q);
                                    audio_fmt = Some(format.clone());
                                }
                                Err(e) => {
                                    log::error!("[audio] failed to open audio queue: {}", e);
                                }
                            }
                        }
                        if let Some(q) = &audio_queue {
                            // Drop the packet when the queue is near-full to prevent
                            // ever-growing latency.  Mirrors grdpsdl2's audio drop logic.
                            if q.size() >= MAX_AUDIO_QUEUE_BYTES {
                                log::debug!("[audio] queue full ({} bytes), dropping packet", q.size());
                            } else {
                                let samples: Vec<i16> = data
                                    .chunks_exact(2)
                                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                                    .collect();
                                if let Err(e) = q.queue_audio(&samples) {
                                    log::warn!("[audio] queue_audio error: {}", e);
                                }
                            }
                        }
                    }
                    Ok(RdpEvent::H264Nal(_)) => {
                        // H264Decoder decodes NALs to NV12Frame before this event fires;
                        // this arm exists only for exhaustiveness.
                    }
                    Ok(RdpEvent::NV12Frame(_)) => {
                        // NV12 frames are routed via the dedicated video_rx channel;
                        // this arm should not fire but is kept for exhaustiveness.
                    }
                    Ok(RdpEvent::Pointer(ptr)) => {
                        last_server_activity = Some(Instant::now());
                        match ptr {
                            PointerEvent::Hide => {
                                sdl_context.mouse().show_cursor(false);
                                cursor_visible = false;
                            }
                            PointerEvent::Cached(idx) => {
                                if !cursor_visible {
                                    sdl_context.mouse().show_cursor(true);
                                    cursor_visible = true;
                                }
                                if let Some(cursor) = cursor_cache.get(&idx) {
                                    cursor.set();
                                }
                            }
                            PointerEvent::Update {
                                idx,
                                xor_bpp,
                                hot_x,
                                hot_y,
                                width,
                                height,
                                and_mask,
                                xor_data,
                            } => {
                                if !cursor_visible {
                                    sdl_context.mouse().show_cursor(true);
                                    cursor_visible = true;
                                }
                                if let Some(mut rgba) = build_cursor_rgba(
                                    xor_bpp, width, height, &and_mask, &xor_data,
                                ) {
                                    match Surface::from_data(
                                        &mut rgba,
                                        width as u32,
                                        height as u32,
                                        width as u32 * 4,
                                        PixelFormatEnum::RGBA32,
                                    ) {
                                        Ok(surface) => {
                                            match Cursor::from_surface(
                                                &surface,
                                                hot_x as i32,
                                                hot_y as i32,
                                            ) {
                                                Ok(cursor) => {
                                                    cursor.set();
                                                    cursor_cache.insert(idx, cursor);
                                                }
                                                Err(e) => {
                                                    log::warn!(
                                                        "[cursor] from_surface error: {}",
                                                        e
                                                    )
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            log::warn!("[cursor] surface error: {}", e)
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break, // no more events this frame
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        // RDP task exited (connection closed or error).
                        if user_quit {
                            // User asked to quit — don't reconnect.
                            running = false;
                        } else if reconnect_dims.is_none() {
                            // Unexpected server-side disconnect (e.g. ERROR INFO PDU +
                            // "Connection reset by peer").  Schedule a retry so the
                            // application recovers automatically.
                            consecutive_failures += 1;
                            log::warn!(
                                "Unexpected disconnect (failure {}/{}), will reconnect",
                                consecutive_failures, MAX_RECONNECT_ATTEMPTS
                            );
                            reconnect_dims = Some((session_width, session_height));
                            running = false;
                        }
                        break;
                    }
                }
            }

            // Drain the video channel (NV12 frames, drop-on-full path).
            // Only the latest frame matters; earlier ones are already stale.
            loop {
                match video_rx.try_recv() {
                    Ok(RdpEvent::NV12Frame(frames)) => {
                        last_server_activity = Some(Instant::now());
                        if !frames.is_empty() {
                            ever_showed_frame = true;
                        }
                        // Upload each frame to GPU but defer present until after
                        // all events are drained — avoids N VSync blocks per loop.
                        for frame in frames {
                            let is_full_screen = frame.screen_x == 0
                                && frame.screen_y == 0
                                && frame.width >= session_width as u32
                                && frame.height >= session_height as u32;
                            if is_full_screen {
                                let _ = rdp_ui.clear_overlay();
                            }
                            if let Err(e) = rdp_ui.upload_nv12_frame(&frame) {
                                log::error!("Failed to upload NV12 frame: {}", e);
                            }
                            nv12_updated = true;
                        }
                    }
                    _ => break,
                }
            }

            // Single GPU flip for all accumulated bitmaps and/or NV12 frames.
            if !pending_bitmaps.is_empty() || nv12_updated {
                if !pending_bitmaps.is_empty() {
                    if let Err(e) = rdp_ui.compose_bitmaps(&pending_bitmaps) {
                        log::error!("Failed to compose bitmaps: {}", e);
                    }
                }
                if let Err(e) = rdp_ui.present_composed() {
                    log::error!("Failed to present: {}", e);
                }
            }

            // Forward keyboard/mouse-button commands collected in Step 1.
            // Sending here (after event_rx has been drained) guarantees that the
            // RDP I/O task is no longer blocked on event_tx.send(), so it can
            // immediately drain input_rx — eliminating the cross-channel deadlock.
            for cmd in pending_inputs {
                input_tx.send(cmd).await.ok();
            }

            // Step 3: yield to the RDP I/O task.
            // 4 ms here + 8 ms in the RDP task ≈ 12 ms max latency for a new
            // frame, comparable to grdpsdl2's ~8 ms WaitEventTimeout rhythm.
            tokio::time::sleep(Duration::from_millis(4)).await;
        }

        // Signal the RDP task to exit by dropping all channel endpoints, then
        // await it so we don't leak the task across reconnects.
        // input_tx must be dropped first so the RDP task detects Disconnected
        // on its next input_rx.try_recv() — even when the server is idle and
        // never sends an event for event_tx.send() to fail on.
        drop(input_tx);
        drop(event_rx);
        drop(video_rx);
        let _ = task.await;

        match reconnect_dims {
            None => {
                // Normal exit (user quit or server deactivated).
                break 'session;
            }
            Some((rw, rh)) => {
                if consecutive_failures >= MAX_RECONNECT_ATTEMPTS {
                    log::error!(
                        "Max reconnect attempts ({}) reached, giving up",
                        MAX_RECONNECT_ATTEMPTS
                    );
                    break 'session;
                }
                session_width = rw;
                session_height = rh;
                let delay = reconnect_backoff(consecutive_failures);
                log::info!(
                    "Reconnecting to {} ({}x{}) in {:?} (attempt {}/{})",
                    config.host, rw, rh, delay, consecutive_failures, MAX_RECONNECT_ATTEMPTS
                );
                tokio::time::sleep(delay).await;
                // Continue outer loop → new session.
            }
        }
    }

    log::info!("RDP client closing");
    Ok(())
}
