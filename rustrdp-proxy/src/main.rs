use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query,
    },
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use native_tls::TlsConnector;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower_http::services::ServeDir;

const INDEX_HTML: &[u8] = include_bytes!("../assets/index.html");
const WASM_JS: &[u8] = include_bytes!("../assets/pkg/rdp_wasm.js");
const WASM_BG: &[u8] = include_bytes!("../assets/pkg/rdp_wasm_bg.wasm");

#[derive(Parser)]
#[command(name = "rustrdp-proxy", about = "WebSocket-to-TCP proxy + web server for RDP")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8081")]
    listen: SocketAddr,
    /// Override static files directory (for development; disables embedded assets)
    #[arg(long = "static")]
    static_dir: Option<String>,
}

async fn serve_index() -> impl IntoResponse {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(Bytes::from_static(INDEX_HTML)))
        .unwrap()
}

async fn serve_wasm_js() -> impl IntoResponse {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/javascript; charset=utf-8")
        .body(Body::from(Bytes::from_static(WASM_JS)))
        .unwrap()
}

async fn serve_wasm_bg() -> impl IntoResponse {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/wasm")
        .body(Body::from(Bytes::from_static(WASM_BG)))
        .unwrap()
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let mut app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/", get(serve_index))
        .route("/pkg/rdp_wasm.js", get(serve_wasm_js))
        .route("/pkg/rdp_wasm_bg.wasm", get(serve_wasm_bg));

    if let Some(static_dir) = args.static_dir {
        app = app.fallback_service(ServeDir::new(static_dir));
    }

    let listener = tokio::net::TcpListener::bind(args.listen).await.unwrap();
    eprintln!("Listening on http://{}", args.listen);
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = params.get("target").cloned().unwrap_or_default();
    ws.on_upgrade(move |socket| handle_socket(socket, target))
}

/// Read exactly one TPKT frame from `tcp`.
async fn read_tpkt_frame(tcp: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    tcp.read_exact(&mut header).await?;
    if header[0] != 0x03 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Not a TPKT frame",
        ));
    }
    let total_len = u16::from_be_bytes([header[2], header[3]]) as usize;
    if total_len < 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "TPKT length too short",
        ));
    }
    let mut body = vec![0u8; total_len - 4];
    tcp.read_exact(&mut body).await?;
    let mut frame = header.to_vec();
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Extract selectedProtocol from a TPKT-framed X.224 CC (Connect Confirm) at full-frame offsets.
/// TPKT header: [0..4], LI: [4], CC code: [5], DST-REF: [6..8], SRC-REF: [8..10], CLASS: [10]
/// Neg type: [11], flags: [12], length: [13..15], selectedProtocol: [15..19]
fn extract_selected_protocol(cc_frame: &[u8]) -> u32 {
    if cc_frame.len() >= 19 && cc_frame[11] == 0x02 {
        u32::from_le_bytes([cc_frame[15], cc_frame[16], cc_frame[17], cc_frame[18]])
    } else {
        0 // PROTOCOL_RDP
    }
}

/// Walk DER TBSCertificate to extract SubjectPublicKeyInfo SEQUENCE bytes.
/// Extract the RSAPublicKey (PKCS#1) from a DER-encoded X.509 certificate.
/// Returns the SEQUENCE { INTEGER N, INTEGER E } bytes, which is the
/// subjectPublicKey content from SubjectPublicKeyInfo.
/// This matches grdp's TlsPubKey() which does asn1.Marshal(rsa.PublicKey).
fn extract_rsa_pubkey(cert_der: &[u8]) -> Option<Vec<u8>> {
    // outer SEQUENCE (certificate)
    let mut pos = 0;
    der_expect_tag(cert_der, &mut pos, 0x30)?;
    der_skip_length(cert_der, &mut pos)?;

    // TBSCertificate SEQUENCE
    der_expect_tag(cert_der, &mut pos, 0x30)?;
    let tbs_len = der_skip_length(cert_der, &mut pos)?;
    let tbs_end = pos + tbs_len;

    // optional version [0]
    if pos < cert_der.len() && cert_der[pos] == 0xA0 {
        der_expect_tag(cert_der, &mut pos, 0xA0)?;
        let l = der_skip_length(cert_der, &mut pos)?;
        pos += l;
    }

    // skip 5 fields: serialNumber, signature, issuer, validity, subject
    for _ in 0..5 {
        if pos >= tbs_end {
            return None;
        }
        pos += 1; // tag
        let l = der_skip_length(cert_der, &mut pos)?;
        pos += l;
    }

    // next SEQUENCE is subjectPublicKeyInfo
    if pos >= tbs_end || cert_der[pos] != 0x30 {
        return None;
    }
    pos += 1;
    der_skip_length(cert_der, &mut pos)?;

    // skip AlgorithmIdentifier SEQUENCE
    der_expect_tag(cert_der, &mut pos, 0x30)?;
    let alg_len = der_skip_length(cert_der, &mut pos)?;
    pos += alg_len;

    // BIT STRING subjectPublicKey
    der_expect_tag(cert_der, &mut pos, 0x03)?;
    let bs_len = der_skip_length(cert_der, &mut pos)?;
    if bs_len < 1 || pos >= cert_der.len() {
        return None;
    }
    // first byte of BIT STRING is unused-bits count (always 0x00 for RSA)
    pos += 1;

    // remaining bytes are RSAPublicKey: SEQUENCE { INTEGER N, INTEGER E }
    let rsa_start = pos;
    let rsa_len = bs_len - 1;
    if rsa_start + rsa_len > cert_der.len() {
        return None;
    }
    Some(cert_der[rsa_start..rsa_start + rsa_len].to_vec())
}

fn der_expect_tag(data: &[u8], pos: &mut usize, expected: u8) -> Option<()> {
    if *pos >= data.len() || data[*pos] != expected {
        return None;
    }
    *pos += 1;
    Some(())
}

fn der_skip_length(data: &[u8], pos: &mut usize) -> Option<usize> {
    if *pos >= data.len() {
        return None;
    }
    let b = data[*pos];
    *pos += 1;
    if b & 0x80 == 0 {
        Some(b as usize)
    } else {
        let n = (b & 0x7F) as usize;
        if *pos + n > data.len() {
            return None;
        }
        let mut len = 0usize;
        for _ in 0..n {
            len = (len << 8) | data[*pos] as usize;
            *pos += 1;
        }
        Some(len)
    }
}

async fn handle_socket(mut socket: WebSocket, target: String) {
    if target.is_empty() {
        let _ = socket
            .send(Message::Text(
                "ERROR: missing ?target=host:port".to_string(),
            ))
            .await;
        return;
    }

    let mut tcp = match TcpStream::connect(&target).await {
        Ok(t) => t,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("ERROR: {}", e))).await;
            return;
        }
    };

    // --- X.224 Connection Request/Confirm exchange ---
    // Forward the client's CR from WebSocket to TCP
    let cr_frame = match socket.recv().await {
        Some(Ok(Message::Binary(data))) => data,
        _ => {
            return;
        }
    };
    if tcp.write_all(&cr_frame).await.is_err() {
        return;
    }

    // Read X.224 CC from the RDP server
    let cc_frame = match read_tpkt_frame(&mut tcp).await {
        Ok(f) => f,
        Err(_) => {
            return;
        }
    };

    let selected_protocol = extract_selected_protocol(&cc_frame);

    // --- TLS upgrade if needed ---
    const PROTOCOL_RDP: u32 = 0;

    if selected_protocol != PROTOCOL_RDP {
        // Send CC to WASM first
        if socket.send(Message::Binary(cc_frame)).await.is_err() {
            return;
        }

        // Perform TLS handshake with the RDP server
        let hostname = target.split(':').next().unwrap_or("localhost").to_string();
        let connector = match TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
        {
            Ok(c) => c,
            Err(_) => {
                return;
            }
        };
        let connector = tokio_native_tls::TlsConnector::from(connector);
        let tls = match connector.connect(&hostname, tcp).await {
            Ok(t) => t,
            Err(_) => {
                return;
            }
        };

        // Extract server certificate RSA public key (PKCS#1 RSAPublicKey bytes)
        let spki = tls
            .get_ref()
            .peer_certificate()
            .ok()
            .flatten()
            .and_then(|cert| cert.to_der().ok())
            .and_then(|der| extract_rsa_pubkey(&der))
            .unwrap_or_default();

        // Send control message: [0x00, 0x00, len_hi, len_lo, ...spki]
        let len = spki.len();
        let mut ctrl = vec![0x00u8, 0x00, (len >> 8) as u8, (len & 0xFF) as u8];
        ctrl.extend_from_slice(&spki);
        if socket.send(Message::Binary(ctrl)).await.is_err() {
            return;
        }

        // Now relay all traffic transparently
        let (mut tls_read, mut tls_write) = tokio::io::split(tls);
        let (mut ws_sink, mut ws_stream) = socket.split();

        let ws_to_tls = async move {
            while let Some(Ok(msg)) = ws_stream.next().await {
                match msg {
                    Message::Binary(data) => {
                        if tls_write.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        };

        let tls_to_ws = async move {
            let mut buf = vec![0u8; 32768];
            loop {
                match tls_read.read(&mut buf).await {
                    Ok(0) => {
                        break;
                    }
                    Ok(n) => {
                        if ws_sink
                            .send(Message::Binary(buf[..n].to_vec()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        };

        tokio::select! {
            _ = ws_to_tls => {},
            _ = tls_to_ws => {},
        }
    } else {
        // Plain RDP (no TLS) — forward CC and relay bidirectionally
        if socket.send(Message::Binary(cc_frame)).await.is_err() {
            return;
        }

        let (mut tcp_read, mut tcp_write) = tcp.into_split();
        let (mut ws_sink, mut ws_stream) = socket.split();

        let ws_to_tcp = async move {
            while let Some(Ok(msg)) = ws_stream.next().await {
                match msg {
                    Message::Binary(data) => {
                        if tcp_write.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        };

        let tcp_to_ws = async move {
            let mut buf = vec![0u8; 32768];
            loop {
                match tcp_read.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if ws_sink
                            .send(Message::Binary(buf[..n].to_vec()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        };

        tokio::select! {
            _ = ws_to_tcp => {},
            _ = tcp_to_ws => {},
        }
    }
}
