use rdp_sdl2::config::RdpConfig;
use rdp_sdl2::connection::RdpConnection;
use rdp_sdl2::input::InputHandler;
use rdp_sdl2::ui::RdpUI;
use rdp_core::client::RdpEvent;
use rdp_core::protocol::rdpsnd::AudioFormat;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use std::error::Error;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

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
    let mut rdp_session = RdpConnection::connect(&config).await?;

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

    log::info!("RDP client ready");

    // Main loop
    while running {
        // Handle SDL2 events
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
                        if let Err(e) = rdp_session.send_key_down(0, scancode).await {
                            log::error!("Failed to send key down: {}", e);
                        }
                    }
                }
                Event::KeyUp {
                    keycode: Some(keycode),
                    ..
                } => {
                    if let Some((scancode, _)) = input_handler.handle_keyboard_event(keycode, false) {
                        if let Err(e) = rdp_session.send_key_up(scancode).await {
                            log::error!("Failed to send key up: {}", e);
                        }
                    }
                }
                Event::MouseMotion { x, y, .. } => {
                    mouse_x = x as u16;
                    mouse_y = y as u16;
                    if let Err(e) = rdp_session.send_mouse_move(mouse_x, mouse_y).await {
                        log::error!("Failed to send mouse move: {}", e);
                    }
                }
                Event::MouseButtonDown { mouse_btn, .. } => {
                    if let Some((btn, _)) = input_handler.handle_mouse_button(mouse_btn, true) {
                        if let Err(e) = rdp_session.send_mouse_button(btn, true, mouse_x, mouse_y).await {
                            log::error!("Failed to send mouse button down: {}", e);
                        }
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if let Some((btn, _)) = input_handler.handle_mouse_button(mouse_btn, false) {
                        if let Err(e) = rdp_session.send_mouse_button(btn, false, mouse_x, mouse_y).await {
                            log::error!("Failed to send mouse button up: {}", e);
                        }
                    }
                }
                _ => {}
            }
        }

        // Receive RDP events
        if let Ok(rdp_event) = tokio::time::timeout(
            Duration::from_millis(50),
            rdp_session.recv_event(),
        )
        .await
        {
            match rdp_event {
                Ok(RdpEvent::Ready) => {
                    log::info!("RDP session ready");
                }
                Ok(RdpEvent::Bitmap(bitmaps)) => {
                    if let Err(e) = rdp_ui.update_screen(&bitmaps) {
                        log::error!("Failed to update screen: {}", e);
                    }
                }
                Ok(RdpEvent::Deactivated) => {
                    log::info!("RDP session deactivated");
                    running = false;
                }
                Ok(RdpEvent::Audio { format, data }) => {
                    // (Re-)open the audio queue when the format changes
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
                        // Convert raw bytes to signed 16-bit samples (little-endian PCM)
                        let samples: Vec<i16> = data.chunks_exact(2)
                            .map(|b| i16::from_le_bytes([b[0], b[1]]))
                            .collect();
                        if let Err(e) = q.queue_audio(&samples) {
                            log::warn!("[audio] queue_audio error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    log::error!("RDP error: {}", e);
                    running = false;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(16));
    }

    log::info!("RDP client closing");
    Ok(())
}
