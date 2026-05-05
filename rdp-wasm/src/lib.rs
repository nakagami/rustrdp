use async_trait::async_trait;
use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures_channel::oneshot;
use futures_util::{FutureExt, StreamExt};
use js_sys::{ArrayBuffer, Uint8Array};
use rdp_core::bitmap::Bitmap;
use rdp_core::protocol::Transport;
use rdp_core::{H264NalEvent, PointerEvent, RdpError, RdpEvent, RdpSession};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{BinaryType, MessageEvent, WebSocket};

#[wasm_bindgen(start)]
pub fn main() {
    console_log::init_with_level(log::Level::Debug).ok();
}

// ─── WsTransport ──────────────────────────────────────────────────────────────

struct WsTransport {
    ws: WebSocket,
    rx: UnboundedReceiver<Vec<u8>>,
    buf: VecDeque<u8>,
    // kept alive for the lifetime of this struct
    _onmessage: Closure<dyn FnMut(MessageEvent)>,
    _onclose: Closure<dyn FnMut(JsValue)>,
}

impl WsTransport {
    /// Open a WebSocket to `url` and wait until it's open.
    async fn connect(url: &str) -> Result<Self, JsValue> {
        let ws = WebSocket::new(url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let (raw_tx, rx): (UnboundedSender<Vec<u8>>, UnboundedReceiver<Vec<u8>>) =
            mpsc::unbounded();
        // Wrap in Rc<RefCell<Option>> so onclose can drop the sender,
        // which causes rx.next() to return None and unblock recv().
        let tx: Rc<RefCell<Option<UnboundedSender<Vec<u8>>>>> = Rc::new(RefCell::new(Some(raw_tx)));

        let (open_tx, open_rx) = oneshot::channel::<Result<(), String>>();

        // onopen
        let open_tx_cell = std::cell::Cell::new(Some(open_tx));
        let onopen = Closure::once(move |_: JsValue| {
            if let Some(tx) = open_tx_cell.take() {
                let _ = tx.send(Ok(()));
            }
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        // onerror
        let ws2 = ws.clone();
        let onerror = Closure::wrap(Box::new(move |_: JsValue| {
            log::error!("WebSocket error");
            let _ = ws2.close();
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();

        // onmessage — deliver data into the channel
        let tx_msg = tx.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Ok(ab) = e.data().dyn_into::<ArrayBuffer>() {
                let data = Uint8Array::new(&ab).to_vec();
                if let Some(ref sender) = *tx_msg.borrow() {
                    let _ = sender.unbounded_send(data);
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onclose — drop the sender so rx.next().await returns None
        let tx_close = tx.clone();
        let onclose = Closure::wrap(Box::new(move |_: JsValue| {
            log::debug!("WebSocket closed by server");
            tx_close.borrow_mut().take(); // drop sender → unblocks recv()
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        // wait for open
        open_rx
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?
            .map_err(|e| JsValue::from_str(&e))?;

        Ok(WsTransport {
            ws,
            rx,
            buf: VecDeque::new(),
            _onmessage: onmessage,
            _onclose: onclose,
        })
    }

    async fn fill_buf_to(&mut self, n: usize) -> Result<(), RdpError> {
        while self.buf.len() < n {
            match self.rx.next().await {
                Some(chunk) => self.buf.extend(chunk),
                None => return Err(RdpError::Closed),
            }
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl Transport for WsTransport {
    async fn send(&mut self, data: &[u8]) -> Result<(), RdpError> {
        let arr = Uint8Array::from(data);
        self.ws
            .send_with_array_buffer(&arr.buffer())
            .map_err(|e| RdpError::Io(format!("{:?}", e)))
    }

    async fn recv_exact(&mut self, n: usize) -> Result<Vec<u8>, RdpError> {
        self.fill_buf_to(n).await?;
        Ok(self.buf.drain(..n).collect())
    }

    async fn close(&mut self) {
        let _ = self.ws.close();
    }

    async fn start_tls(&mut self) -> Result<Vec<u8>, RdpError> {
        // The proxy sends a control message: [0x00, 0x00, len_hi, len_lo, ...spki]
        self.fill_buf_to(4).await?;
        let hdr: Vec<u8> = self.buf.drain(..4).collect();
        if hdr[0] != 0x00 || hdr[1] != 0x00 {
            return Err(RdpError::Protocol(
                "WsTransport: unexpected TLS control header".into(),
            ));
        }
        let len = ((hdr[2] as usize) << 8) | (hdr[3] as usize);
        if len == 0 {
            return Ok(vec![]);
        }
        self.fill_buf_to(len).await?;
        Ok(self.buf.drain(..len).collect())
    }
}

// ─── InputEvent ───────────────────────────────────────────────────────────────

enum InputEvent {
    KeyDown {
        flags: u16,
        scancode: u8,
    },
    KeyUp {
        scancode: u8,
    },
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: u8,
        down: bool,
        x: u16,
        y: u16,
    },
    MouseWheel {
        delta: i16,
    },
}

// ─── RdpController ────────────────────────────────────────────────────────────

/// JavaScript-facing handle for an RDP session.
#[wasm_bindgen]
pub struct RdpController {
    input_tx: UnboundedSender<InputEvent>,
}

#[wasm_bindgen]
impl RdpController {
    pub fn key_down(&self, flags: u16, scancode: u8) {
        let _ = self
            .input_tx
            .unbounded_send(InputEvent::KeyDown { flags, scancode });
    }

    pub fn key_up(&self, scancode: u8) {
        let _ = self.input_tx.unbounded_send(InputEvent::KeyUp { scancode });
    }

    pub fn mouse_move(&self, x: u16, y: u16) {
        let _ = self.input_tx.unbounded_send(InputEvent::MouseMove { x, y });
    }

    pub fn mouse_button(&self, button: u8, down: bool, x: u16, y: u16) {
        let _ = self
            .input_tx
            .unbounded_send(InputEvent::MouseButton { button, down, x, y });
    }

    pub fn mouse_wheel(&self, delta: i16) {
        let _ = self
            .input_tx
            .unbounded_send(InputEvent::MouseWheel { delta });
    }
}

// ─── connect ──────────────────────────────────────────────────────────────────

/// Connect to an RDP host through the proxy.
///
/// Waits for the full login sequence to complete before resolving.
/// On failure the returned Promise rejects with a descriptive error string.
#[wasm_bindgen]
pub async fn connect(
    proxy_url: String,
    domain: String,
    user: String,
    password: String,
    width: u16,
    height: u16,
    on_bitmap: js_sys::Function,
    on_h264: js_sys::Function,
    on_audio: js_sys::Function,
    on_pointer_hide: js_sys::Function,
    on_pointer_cached: js_sys::Function,
    on_pointer_update: js_sys::Function,
) -> Result<RdpController, JsValue> {
    let transport = WsTransport::connect(&proxy_url).await?;

    let (input_tx, mut input_rx): (UnboundedSender<InputEvent>, UnboundedReceiver<InputEvent>) =
        mpsc::unbounded();

    // Use a oneshot channel so the login result is propagated back to the
    // calling JavaScript Promise (visible as alert / catch block in HTML).
    let (login_tx, login_rx) = oneshot::channel::<Result<(), String>>();

    spawn_local(async move {
        let mut session = match RdpSession::login(
            transport, &domain, &user, &password, width, height, 0x0409, None,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("{:?}", e);
                log::error!("RDP login failed: {}", msg);
                let _ = login_tx.send(Err(msg));
                return;
            }
        };

        log::info!("RDP session ready");
        let _ = login_tx.send(Ok(()));

        loop {
            futures_util::select! {
                // Process one input event if available
                maybe_input = input_rx.next() => {
                    match maybe_input {
                        Some(event) => {
                            let result = match event {
                                InputEvent::KeyDown { flags, scancode } => {
                                    session.send_key_down(flags, scancode).await
                                }
                                InputEvent::KeyUp { scancode } => session.send_key_up(scancode).await,
                                InputEvent::MouseMove { x, y } => session.send_mouse_move(x, y).await,
                                InputEvent::MouseButton { button, down, x, y } => {
                                    session.send_mouse_button(button, down, x, y).await
                                }
                                InputEvent::MouseWheel { delta } => {
                                    session.send_mouse_wheel(delta).await
                                }
                            };
                            if let Err(e) = result {
                                log::error!("Input send error: {:?}", e);
                            }
                        }
                        None => break, // input channel closed
                    }
                }

                // Process one display event from the server
                result = session.recv_event().fuse() => {
                    match result {
                        Ok(RdpEvent::Bitmap(bitmaps)) => {
                            for bmp in bitmaps {
                                deliver_bitmap(&on_bitmap, &bmp);
                            }
                        }
                        Ok(RdpEvent::Deactivated) => {
                            log::info!("RDP session deactivated");
                            break;
                        }
                        Ok(RdpEvent::Ready) => {}
                        Ok(RdpEvent::Resize { .. }) => {}
                        Ok(RdpEvent::NV12Frame(_)) => {}
                        Ok(RdpEvent::H264Nal(nals)) => {
                            for nal in nals {
                                deliver_h264(&on_h264, &nal);
                            }
                        }
                        Ok(RdpEvent::Audio { format, data }) => {
                            let pcm = Uint8Array::from(data.as_slice());
                            let _ = on_audio.call4(
                                &JsValue::NULL,
                                &JsValue::from(format.sample_rate),
                                &JsValue::from(format.channels as u32),
                                &JsValue::from(format.bits_per_sample as u32),
                                &pcm.into(),
                            );
                        }
                        Ok(RdpEvent::Pointer(ptr)) => {
                            deliver_pointer(&on_pointer_hide, &on_pointer_cached, &on_pointer_update, ptr);
                        }
                        Err(e) => {
                            log::error!("RDP event error: {:?}", e);
                            break;
                        }
                    }
                }
            }
        }
    });

    // Wait for login to succeed or fail, then propagate to JS.
    match login_rx.await {
        Ok(Ok(())) => Ok(RdpController { input_tx }),
        Ok(Err(msg)) => Err(JsValue::from_str(&msg)),
        Err(_) => Err(JsValue::from_str("Login task was cancelled")),
    }
}

fn deliver_bitmap(callback: &js_sys::Function, bmp: &Bitmap) {
    let data_js = Uint8Array::from(bmp.data.as_slice());
    let _ = callback.call6(
        &JsValue::NULL,
        &JsValue::from(bmp.dest_left as u32),
        &JsValue::from(bmp.dest_top as u32),
        &JsValue::from(bmp.width as u32),
        &JsValue::from(bmp.height as u32),
        &JsValue::from(bmp.bits_per_pixel as u32),
        &data_js.into(),
    );
}

fn deliver_h264(callback: &js_sys::Function, nal: &H264NalEvent) {
    let data_js = Uint8Array::from(nal.data.as_slice());
    let _ = callback.call4(
        &JsValue::NULL,
        &JsValue::from(nal.dest_left),
        &JsValue::from(nal.dest_top),
        &JsValue::from(nal.is_key),
        &data_js.into(),
    );
}

fn deliver_pointer(
    on_hide: &js_sys::Function,
    on_cached: &js_sys::Function,
    on_update: &js_sys::Function,
    ptr: PointerEvent,
) {
    match ptr {
        PointerEvent::Hide => {
            let _ = on_hide.call0(&JsValue::NULL);
        }
        PointerEvent::Cached(idx) => {
            let _ = on_cached.call1(&JsValue::NULL, &JsValue::from(idx as u32));
        }
        PointerEvent::Update { idx, xor_bpp, hot_x, hot_y, width, height, and_mask, xor_data } => {
            let and_js = Uint8Array::from(and_mask.as_slice());
            let xor_js = Uint8Array::from(xor_data.as_slice());
            let _ = on_update.call8(
                &JsValue::NULL,
                &JsValue::from(idx as u32),
                &JsValue::from(xor_bpp as u32),
                &JsValue::from(hot_x as u32),
                &JsValue::from(hot_y as u32),
                &JsValue::from(width as u32),
                &JsValue::from(height as u32),
                &and_js.into(),
                &xor_js.into(),
            );
        }
    }
}
