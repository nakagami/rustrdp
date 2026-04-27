use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{WebSocket, MessageEvent, BinaryType};
use js_sys::Uint8Array;
use futures_channel::mpsc::{self, UnboundedSender, UnboundedReceiver};
use futures_util::{StreamExt, SinkExt};

#[wasm_bindgen(start)]
pub fn main() {
    console_log::init_with_level(log::Level::Debug).ok();
}

#[wasm_bindgen]
pub struct RdpClient {
    ws_sender: UnboundedSender<Vec<u8>>,
}

#[wasm_bindgen]
impl RdpClient {
    #[wasm_bindgen(constructor)]
    pub fn new(proxy_url: &str, target: &str) -> Result<RdpClient, JsValue> {
        let ws = WebSocket::new(proxy_url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let (tx, rx): (UnboundedSender<Vec<u8>>, UnboundedReceiver<Vec<u8>>) = mpsc::unbounded();

        let ws_clone = ws.clone();
        let target = target.to_string();

        let onopen = Closure::once(move |_: JsValue| {
            ws_clone.send_with_str(&target).unwrap_or(());
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let data = Uint8Array::new(&buf).to_vec();
                log::debug!("Received {} bytes from proxy", data.len());
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        let onerror = Closure::wrap(Box::new(move |_e: web_sys::ErrorEvent| {
            log::error!("WebSocket error");
        }) as Box<dyn FnMut(web_sys::ErrorEvent)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();

        let ws_send = ws.clone();
        spawn_local(async move {
            let mut rx = rx;
            while let Some(data) = rx.next().await {
                let arr = Uint8Array::from(data.as_slice());
                ws_send.send_with_array_buffer(&arr.buffer()).unwrap_or(());
            }
        });

        Ok(RdpClient { ws_sender: tx })
    }

    pub fn send(&mut self, data: &[u8]) -> Result<(), JsValue> {
        self.ws_sender
            .unbounded_send(data.to_vec())
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    pub fn disconnect(&self) {
    }
}
