use rdp_sdl2::config::RdpConfig;
use rdp_sdl2::connection::RdpConnection;
use rdp_sdl2::input::InputHandler;
use rdp_sdl2::ui::RdpUI;
use rdp_core::client::RdpEvent;
use rdp_core::protocol::rdpsnd::AudioFormat;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use std::error::Error;
use std::time::Duration;
use tokio::sync::mpsc;

/// Commands sent from the SDL event loop to the RDP I/O task.
///
/// Decoupling input from display processing mirrors grdpsdl2's goroutine model:
/// the SDL loop never blocks on a network write, keeping event polling responsive.
#[derive(Debug)]
enum InputCmd {
    KeyDown { flags: u16, scancode: u8 },
    KeyUp { scancode: u8 },
    MouseMove { x: u16, y: u16 },
    MouseButton { btn: u8, down: bool, x: u16, y: u16 },
    MouseWheel { delta: i16 },
}

// Use current_thread runtime so that spawn_local works without requiring Send bounds.
// RdpSession<SimpleTransport> is !Send because the Transport trait uses ?Send.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    // LocalSet is required for spawn_local (non-Send futures on the current thread).
    tokio::task::LocalSet::new().run_until(run()).await
}

async fn run() -> Result<(), Box<dyn Error>> {
    // Load configuration from environment
    let config = RdpConfig::from_env().map_err(|e| {
        eprintln!(
            "Configuration error: {}. Please set the following environment variables:",
            e
        );
        eprintln!("  GRDP_HOST     - RDP server hostname");
        eprintln!("  GRDP_PORT     - RDP server port");
        eprintln!("  GRDP_USER     - Username");
        eprintln!("  GRDP_PASSWORD - Password");
        eprintln!("  GRDP_DOMAIN   - Domain (optional)");
        eprintln!("  GRDP_WINDOW_SIZE - Window size (default: 1280x800)");
        e
    })?;

    log::info!(
        "Starting RDP client {}x{}",
        config.width,
        config.height
    );

    // Connect to RDP server
    let rdp_session = RdpConnection::connect(&config).await?;

    // Initialize SDL2
    let sdl_context = sdl2::init()?;
    let audio_subsystem = sdl_context.audio()?;
    let mut rdp_ui = RdpUI::new(
        &sdl_context,
        config.width,
        config.height,
        "RDP Client",
    )?;

    // Initialize input handler
    let input_handler = InputHandler::new(config.swap_alt_meta);

    // Get event pump
    let mut event_pump = sdl_context.event_pump()?;
    let mut running = true;
    let mut mouse_x: u16 = 0;
    let mut mouse_y: u16 = 0;
    // Audio queue — opened lazily on first audio event (16-bit signed PCM)
    let mut audio_queue: Option<AudioQueue<i16>> = None;
    let mut audio_fmt: Option<AudioFormat> = None;

    // --- Channel setup (mirrors grdpsdl2's bitmapCh / input goroutines) ---
    //
    // input_tx  (SDL → RDP task): keyboard / mouse commands
    // event_rx  (RDP task → SDL): display updates, audio, resize, etc.
    //
    // Buffer sizes: 64 input slots absorb a burst of fast typing; 32 event
    // slots match the depth of grdpsdl2's bitmapCh (128 bitmaps / ~4 per PDU).
    let (input_tx, mut input_rx) = mpsc::channel::<InputCmd>(64);
    let (event_tx, mut event_rx) = mpsc::channel::<RdpEvent>(32);

    // --- RDP I/O task ---
    //
    // Runs concurrently with the SDL loop on the same OS thread via cooperative
    // multitasking (current_thread runtime).  The design mirrors grdpsdl2:
    //
    //   grdpsdl2 goroutine:  RecvEvent() → bitmapCh push  (never interrupted by SDL)
    //   rdp-sdl2 task:       recv_event() → event_tx send  (+ input drain every 8ms)
    //
    // Using an 8ms timeout (= grdpsdl2's WaitEventTimeout(8)) ensures the input
    // drain loop runs at least every 8ms even when no RDP data arrives.
    // Frame ACKs missed due to the 8ms cancellation are re-sent at the start of
    // the next recv_event() call via RdpSession::pending_acks.
    tokio::task::spawn_local(async move {
        let mut session = rdp_session;
        loop {
            // Drain all pending input commands before waiting for the next RDP frame.
            // try_recv is non-blocking: if the channel is empty we immediately proceed
            // to recv_event, matching grdpsdl2's "handle events then render" order.
            while let Ok(cmd) = input_rx.try_recv() {
                let result = match cmd {
                    InputCmd::KeyDown { flags, scancode } =>
                        session.send_key_down(flags, scancode).await,
                    InputCmd::KeyUp { scancode } =>
                        session.send_key_up(scancode).await,
                    InputCmd::MouseMove { x, y } =>
                        session.send_mouse_move(x, y).await,
                    InputCmd::MouseButton { btn, down, x, y } =>
                        session.send_mouse_button(btn, down, x, y).await,
                    InputCmd::MouseWheel { delta } =>
                        session.send_mouse_wheel(delta).await,
                };
                if let Err(e) = result {
                    log::error!("Input send error: {}", e);
                    return;
                }
            }

            // Wait for the next RDP event (frame, audio, resize, etc.).
            // 8ms timeout matches grdpsdl2's WaitEventTimeout(8) and ensures the
            // input drain above runs regularly even on an idle desktop.
            match tokio::time::timeout(
                Duration::from_millis(8),
                session.recv_event(),
            ).await {
                Ok(Ok(event)) => {
                    let deactivated = matches!(event, RdpEvent::Deactivated);
                    if event_tx.send(event).await.is_err() {
                        break; // SDL side dropped — application is exiting
                    }
                    if deactivated { break; }
                }
                Ok(Err(e)) => {
                    log::error!("RDP session error: {}", e);
                    break;
                }
                Err(_timeout) => {} // 8ms with no data — loop back to drain input
            }
        }
    });

    log::info!("RDP client ready");

    // --- SDL main loop ---
    //
    // Mirrors grdpsdl2 main loop structure:
    //   1. Process SDL events  (input → channel, no network .await)
    //   2. Drain RDP events    (try_recv, non-blocking)
    //   3. Yield               (sleep lets the RDP task run)
    //
    // Removing network .await from the SDL loop means keystrokes and clicks are
    // never delayed by network back-pressure; the input channel absorbs bursts.
    while running {
        // Step 1: process SDL input events
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => {
                    running = false;
                    break;
                }
                Event::KeyDown {
                    keycode: Some(keycode),
                    ..
                } => {
                    if keycode == Keycode::Escape {
                        running = false;
                    } else if let Some((scancode, _)) = input_handler.handle_keyboard_event(keycode, true) {
                        input_tx.send(InputCmd::KeyDown { flags: 0, scancode }).await.ok();
                    }
                }
                Event::KeyUp {
                    keycode: Some(keycode),
                    ..
                } => {
                    if let Some((scancode, _)) = input_handler.handle_keyboard_event(keycode, false) {
                        input_tx.send(InputCmd::KeyUp { scancode }).await.ok();
                    }
                }
                Event::MouseMotion { x, y, .. } => {
                    mouse_x = x as u16;
                    mouse_y = y as u16;
                    // Mouse moves are high-frequency: use try_send so that only
                    // the latest position is forwarded when the channel is full,
                    // rather than queuing many stale coordinates.
                    input_tx.try_send(InputCmd::MouseMove { x: mouse_x, y: mouse_y }).ok();
                }
                Event::MouseButtonDown { mouse_btn, x, y, .. } => {
                    mouse_x = x as u16;
                    mouse_y = y as u16;
                    if let Some((btn, _)) = input_handler.handle_mouse_button(mouse_btn, true) {
                        input_tx.send(InputCmd::MouseButton {
                            btn, down: true, x: mouse_x, y: mouse_y,
                        }).await.ok();
                    }
                }
                Event::MouseButtonUp { mouse_btn, x, y, .. } => {
                    mouse_x = x as u16;
                    mouse_y = y as u16;
                    if let Some((btn, _)) = input_handler.handle_mouse_button(mouse_btn, false) {
                        input_tx.send(InputCmd::MouseButton {
                            btn, down: false, x: mouse_x, y: mouse_y,
                        }).await.ok();
                    }
                }
                Event::MouseWheel { x, y, .. } => {
                    if let Some(delta) = input_handler.handle_mouse_wheel(x, y) {
                        input_tx.send(InputCmd::MouseWheel { delta }).await.ok();
                    }
                }
                Event::Window {
                    win_event: WindowEvent::Exposed
                        | WindowEvent::Restored
                        | WindowEvent::FocusGained,
                    ..
                } => {
                    if let Err(e) = rdp_ui.repaint() {
                        log::error!("Failed to repaint: {}", e);
                    }
                }
                _ => {}
            }
        }

        // Step 2: drain all available RDP display events (non-blocking).
        // Mirrors grdpsdl2: `for { select { case bs := <-bitmapCh: ... default: break } }`.
        loop {
            match event_rx.try_recv() {
                Ok(RdpEvent::Ready) => {
                    log::info!("RDP session ready");
                }
                Ok(RdpEvent::Resize { width, height }) => {
                    #[cfg(debug_assertions)]
                    eprintln!("[rdp-sdl2] Resize {}x{}", width, height);
                    if let Err(e) = rdp_ui.resize(width, height) {
                        log::error!("Failed to resize UI: {}", e);
                    }
                }
                Ok(RdpEvent::Bitmap(bitmaps)) => {
                    #[cfg(debug_assertions)]
                    if let Some(first) = bitmaps.first() {
                        eprintln!(
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
                    if let Err(e) = rdp_ui.update_screen(&bitmaps) {
                        log::error!("Failed to update screen: {}", e);
                    }
                }
                Ok(RdpEvent::Deactivated) => {
                    log::info!("RDP session deactivated");
                    running = false;
                }
                Ok(RdpEvent::Audio { format, data }) => {
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
                                    format.sample_rate, format.channels, format.bits_per_sample
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
                        let samples: Vec<i16> = data.chunks_exact(2)
                            .map(|b| i16::from_le_bytes([b[0], b[1]]))
                            .collect();
                        if let Err(e) = q.queue_audio(&samples) {
                            log::warn!("[audio] queue_audio error: {}", e);
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break, // no more events this frame
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // RDP task exited (connection closed or error)
                    running = false;
                    break;
                }
            }
        }

        // Step 3: yield to the RDP I/O task.
        // 4ms here + 8ms in the RDP task ≈ 12ms max latency for a new frame,
        // comparable to grdpsdl2's ~8ms WaitEventTimeout rhythm.
        tokio::time::sleep(Duration::from_millis(4)).await;
    }

    log::info!("RDP client closing");
    Ok(())
}
