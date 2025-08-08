use arboard::Clipboard;
#[cfg(target_os = "linux")]
use arboard::SetExtLinux;

use bytes::{BufMut, Bytes, BytesMut};
use clap::Parser;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc},
    time,
};

const CLIPBOARD_MESSAGE_TYPE: &[u8; 4] = &[0, 0, 0, 1];
const PREFIX_LENGTH: usize = 8;
const BODY_MAX_LENGTH: i32 = 0x800000; // 8 Mio

#[derive(Debug, Error)]
enum AppError {
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Clipboard Error: {0}")]
    Clipboard(#[from] arboard::Error),
    #[error("UTF-8 Conversion Error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("Task Join Error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
}

#[derive(Parser, Debug, Clone)]
#[command(version, about)]
struct Args {
    // enable client-mode if server ip are provided
    #[arg(short, long)]
    server: Option<Ipv4Addr>,

    #[arg(short, long, default_value_t = 7582)]
    port: u16,
}

type PeersMap = HashMap<SocketAddr, mpsc::Sender<Bytes>>;

#[derive(Debug)]
struct Server {
    // A map from client address to a sender channel.
    // We send data *to* a client's dedicated writer task via its channel.
    peers: Mutex<PeersMap>,

    // Store the last text seen on the clipboard
    last_clipboard_text: Mutex<String>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
            last_clipboard_text: Mutex::new(String::new()),
        }
    }

    pub async fn add_peer(&self, addr: &SocketAddr, tx: mpsc::Sender<Bytes>) {
        let mut peers_guard = self.peers.lock().await;
        peers_guard.insert(*addr, tx.clone());
        println!("Peer {} added. Total peers: {}", addr, peers_guard.len());
    }

    pub async fn remove_peer(&self, addr: &SocketAddr) -> Option<mpsc::Sender<Bytes>> {
        let mut peers_guard = self.peers.lock().await;
        let removed = peers_guard.remove(addr);
        if removed.is_some() {
            println!("Peer {} removed. Total peers: {}", addr, peers_guard.len());
        }
        removed
    }

    pub async fn broadcast(&self, sender_addr: SocketAddr, message: Bytes) {
        // self.last_clipboard_text = Some(message.clone());

        let peers_guard = self.peers.lock().await;
        for (peer_addr, tx) in peers_guard.iter() {
            if *peer_addr != sender_addr {
                if let Err(_) = tx.send(message.clone()).await {
                    // This usually means the receiving client's task has already shutdown.
                    // The cleanup logic below will eventually remove them.
                    eprintln!(
                        "Failed to send broadcast to {}. Channel might be closed.",
                        peer_addr
                    );
                }
            }
        }
    }

    // Broadcast a message to *all* connected peers
    async fn broadcast_to_all(&self, message: Bytes) {
        let peers_guard = self.peers.lock().await;
        if peers_guard.is_empty() {
            return; // No peers to broadcast to
        }
        println!(
            "Broadcasting clipboard update to {} peers",
            peers_guard.len()
        );
        for (peer_addr, tx) in peers_guard.iter() {
            if tx.send(message.clone()).await.is_err() {
                eprintln!(
                    "Failed to send clipboard broadcast to {}. Channel might be closed.",
                    peer_addr
                );
                // Note: Disconnected peers will be cleaned up by their handle_connection task
            }
        }
    }

    pub async fn check_and_push_clipboard(&self) -> Result<(), AppError> {
        let has_peers = !self.peers.lock().await.is_empty();
        if !has_peers {
            return Ok(());
        }

        let current_text = tokio::task::spawn_blocking(|| -> Result<String, AppError> {
            let mut clipboard = Clipboard::new()?;
            match clipboard.get_text() {
                Ok(text) => Ok(text),
                Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
                Err(e) => Err(AppError::Clipboard(e)),
            }
        })
        .await??;

        let mut last_text_guard = self.last_clipboard_text.lock().await;
        if current_text == *last_text_guard || current_text.is_empty() {
            return Ok(());
        }
        *last_text_guard = current_text.clone();

        let text_bytes = current_text.as_bytes();
        let message_length = PREFIX_LENGTH + text_bytes.len();
        let mut message_data = BytesMut::with_capacity(message_length);

        // big-endian
        message_data.put_i32(message_length as i32);
        message_data.put_slice(CLIPBOARD_MESSAGE_TYPE);
        message_data.put_slice(text_bytes);

        // Freeze into immutable Bytes for sending
        let message_bytes = message_data.freeze();
        // drop the lock on last_clipboard_text *before* broadcasting to avoid
        // holding it while waiting for network operations.
        drop(last_text_guard);

        self.broadcast_to_all(message_bytes).await;
        Ok(())
    }
}

// IMPORTANT: Clipboard operations can be blocking!
// Using spawn_blocking ensures we don't block the async runtime thread.
async fn write_to_clipboard(data: Bytes) -> Result<(), AppError> {
    #[cfg(debug_assertions)]
    println!(
        "attempt to write string to clipboard: {}",
        String::from_utf8(data.to_vec())?
    );
    tokio::task::spawn_blocking(move || {
        // We assume the data is UTF-8 text for the clipboard.
        // Handle non-UTF8 data appropriately if needed (e.g., log error, ignore).
        let text_content = String::from_utf8(data.to_vec())?;

        let mut clipboard = Clipboard::new()?;
        if cfg!(target_os = "linux") {
            println!("Will write {} bytes to clipboard and wait a consummer (paste event).", data.len());
            clipboard.set().wait().text(text_content)?;
        } else {
            println!("Will write {} bytes to clipboard.", data.len());
            clipboard.set_text(text_content)?;
            println!("Wrote {} bytes to clipboard.", data.len());
        }
        Ok(())
    })
    .await
    .unwrap() // Propagate panics from spawn_blocking and return the inner Result
}

async fn clipboard_check_task(server: Arc<Server>) {
    let mut interval = time::interval(Duration::from_secs_f32(0.8));

    // start clipboard watcher
    loop {
        interval.tick().await;

        if let Err(e) = server.check_and_push_clipboard().await {
            eprintln!("Error during clipboard check: {}", e);
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    server: Arc<Server>,
) -> Result<(), AppError> {
    println!("got new connection from: {}", addr);

    // Create a channel for this specific client.
    // Other tasks will send messages *to* this client via `tx`
    // The `rx` will be used by a dedicated task to write data *out* to the socket
    let (tx, mut rx) = mpsc::channel::<Bytes>(32);

    // Add the client's sender channel to the shared peer map
    server.add_peer(&addr, tx).await;

    // split the stream into independent reader and writer halves
    let (mut reader, mut writer) = stream.into_split();

    // --- WRITE TASK ---
    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if writer.write_all(&message).await.is_err() {
                eprintln!(
                    "Error writeing to client {}, connection likely closed.",
                    addr
                );
                break;
            }
        }
        let _ = writer.shutdown().await;
        println!("Writer task for {} finished.", addr);
    });

    // --- READ TASK (within the main handle_connection body) ---
    // This part reads data *from* the client socket.
    let mut header_buf = vec![0; PREFIX_LENGTH];
    loop {
        match reader.read(&mut header_buf).await {
            Ok(_) => {
                let msg_len = i32::from_be_bytes(header_buf[..4].try_into().unwrap());
                println!("got copied data length: {}", msg_len);
                if msg_len <= PREFIX_LENGTH as i32 || ( msg_len > (PREFIX_LENGTH as i32 + BODY_MAX_LENGTH) ) {
                    eprintln!("Error msg_len indicates a negative body length or too large");
                    break;
                }

                let mut body_buf = vec![0; msg_len as usize - PREFIX_LENGTH];
                match reader.read(&mut body_buf).await {
                    Ok(n) => {
                        if n == 0 {
                            println!("connection closed by {}", addr);
                            break; // EOF
                        }

                        let mut received_data = BytesMut::with_capacity(msg_len as usize);
                        received_data.extend_from_slice(&header_buf);
                        received_data.extend_from_slice(&body_buf);
                        println!("Received {} bytes from {}", n, addr);
                        server.broadcast(addr, received_data.freeze()).await;

                        let received_without_prefix = Bytes::copy_from_slice(&body_buf[..]);
                        // spawn a separate task for clipboard to avoid blokcing broadcast/read loop
                        tokio::spawn(async move {
                            if let Err(e) = write_to_clipboard(received_without_prefix).await {
                                eprintln!("Error writing to clipboard from {}: {}", addr, e);
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("Error reading from {}: {}", addr, e);
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("Unsupported protocol not start with i32: {}", e);
            }
        }
    }

    println!("cleaning up connection: {}", addr);
    server.remove_peer(&addr).await;
    writer_task.abort();

    Ok(())
}

#[tokio::main]
#[allow(unreachable_code)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let server = Arc::new(Server::new());

    let server_for_checker = Arc::clone(&server);
    tokio::spawn(clipboard_check_task(server_for_checker));

    if let Some(server_ip) = args.server {
        // Client mode
        loop {
            println!("Connecting to server: {}", server_ip);
            match TcpStream::connect(format!("{}:{}", server_ip, args.port)).await {
                Ok(stream) => {
                    let server_peer_addr = stream.peer_addr()?;
                    let result =
                        handle_connection(stream, server_peer_addr, Arc::clone(&server)).await;
                    eprintln!(
                        "Disconnected from server: {:?}. Retrying in 5s...",
                        result.err()
                    );
                }
                Err(e) => {
                    eprintln!("Connection failed: {}. Retrying in 5s...", e);
                }
            }
            // wait before retrying
            time::sleep(Duration::from_secs(5)).await;
        }
    } else {
        // Server mode
        let addr = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), args.port);
        let listener = TcpListener::bind(addr).await?;
        println!("clipboard server listening on {}", addr);

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    // Clone the Arc pointer to the Server for the new task
                    let server_clone = Arc::clone(&server);

                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, addr, server_clone).await {
                            eprintln!("Error handling connection from {}: {}", addr, e);
                            // server.remove_peer(&addr).await;
                        }
                    });
                }
                Err(e) => {
                    eprintln!("failed to accept connection: {}", e);
                    // TODO: Consider if the server should exit on accept errors
                }
            };
        }
    }

    Ok(())
}
