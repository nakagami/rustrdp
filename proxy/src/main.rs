use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower_http::services::ServeDir;

#[derive(Parser)]
#[command(name = "proxy", about = "WebSocket-to-TCP proxy for RDP")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    #[arg(long = "static", default_value = "static")]
    static_dir: String,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let static_dir = args.static_dir.clone();
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new(&static_dir));

    log::info!("Listening on {}", args.listen);
    let listener = tokio::net::TcpListener::bind(args.listen).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    let target = match socket.recv().await {
        Some(Ok(Message::Text(t))) => t.to_string(),
        _ => {
            let _ = socket.send(Message::Text("ERROR: expected target address".to_string())).await;
            return;
        }
    };

    log::info!("Connecting to TCP target: {}", target);
    let tcp = match TcpStream::connect(&target).await {
        Ok(t) => t,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("ERROR: {}", e))).await;
            return;
        }
    };
    log::info!("Connected to {}", target);

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
    log::info!("Connection to {} closed", target);
}
