//! Local ends of the server's WebSocket tunnels.

use crate::Client;
use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// A connection to the server: TCP, or a Unix socket.
trait Conn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Conn for T {}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<Box<dyn Conn>>>;

#[cfg(unix)]
async fn dial_unix(c: &Client) -> Result<Option<Box<dyn Conn>>> {
    let Some(sock) = &c.unix else { return Ok(None) };
    let stream = tokio::net::UnixStream::connect(sock).await.with_context(|| format!("connecting to {}", sock.display()))?;
    Ok(Some(Box::new(stream)))
}

#[cfg(not(unix))]
async fn dial_unix(_: &Client) -> Result<Option<Box<dyn Conn>>> {
    Ok(None)
}

async fn connect(c: &Client, path: &str) -> Result<Ws> {
    let url = c.url(path).replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
    let mut req = url.as_str().into_client_request()?;
    if let Some(t) = &c.token {
        req.headers_mut().insert("authorization", format!("Bearer {t}").parse()?);
    }
    let stream: Box<dyn Conn> = match dial_unix(c).await? {
        Some(stream) => stream,
        None => {
            let uri = req.uri();
            let host = uri.host().unwrap_or_default().trim_matches(['[', ']']).to_string();
            let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });
            let tcp = TcpStream::connect((host.as_str(), port)).await.with_context(|| format!("connecting to {url}"))?;
            tcp.set_nodelay(true)?;
            Box::new(tcp)
        }
    };
    match tokio_tungstenite::client_async_tls(req, stream).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let body = resp.body().as_ref().map(|b| String::from_utf8_lossy(b).to_string()).unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(body);
            bail!("{msg} ({})", resp.status())
        }
        Err(e) => Err(e).with_context(|| format!("connecting to {url}")),
    }
}

/// Each local connection gets its own WebSocket to `path`.
pub async fn serve_forward(c: &Client, listener: TcpListener, path: String) -> Result<()> {
    // Fail early if the server refuses (tunnels disabled, session gone…) rather than on first use.
    loop {
        let (tcp, peer) = listener.accept().await?;
        let ws = match connect(c, &path).await {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("simpool: {peer}: {e:#}");
                continue;
            }
        };
        tokio::spawn(pipe(ws, tcp));
    }
}

async fn pipe(ws: Ws, tcp: TcpStream) {
    let (mut wtx, mut wrx) = ws.split();
    let (mut rd, mut wr) = tcp.into_split();
    let up = async {
        let mut buf = vec![0u8; 64 << 10];
        while let Ok(n) = rd.read(&mut buf).await {
            if n == 0 || wtx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                break;
            }
        }
        let _ = wtx.send(Message::Close(None)).await;
    };
    let down = async {
        while let Some(Ok(m)) = wrx.next().await {
            let data = match m {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            if wr.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    };
    tokio::join!(up, down);
}

/// Passes each local TCP connection to the server's Unix socket untouched, for browsers, which
/// can't speak to a socket themselves.
#[cfg(unix)]
pub async fn serve_socket_proxy(listener: TcpListener, sock: std::path::PathBuf) -> Result<()> {
    loop {
        let (mut tcp, _) = listener.accept().await?;
        let sock = sock.clone();
        tokio::spawn(async move {
            match tokio::net::UnixStream::connect(&sock).await {
                Ok(mut server) => {
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut server).await;
                }
                Err(e) => eprintln!("simpool: connecting to {}: {e}", sock.display()),
            }
        });
    }
}

const OPEN: u8 = 0;
const DATA: u8 = 1;
const CLOSE: u8 = 2;

fn frame(conn: u32, kind: u8, payload: &[u8]) -> Message {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.extend_from_slice(&conn.to_be_bytes());
    v.push(kind);
    v.extend_from_slice(payload);
    Message::Binary(v.into())
}

/// One WebSocket carries every connection the server accepts; each is dialed to `local` here.
pub async fn serve_reverse(c: &Client, path: String, local: &str) -> Result<()> {
    let ws = connect(c, &path).await?;
    let (mut wtx, mut wrx) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let mut conns: HashMap<u32, mpsc::Sender<Vec<u8>>> = HashMap::new();
    loop {
        tokio::select! {
            m = out_rx.recv() => {
                let Some(m) = m else { break };
                wtx.send(m).await?;
            }
            m = wrx.next() => {
                let m = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                };
                let Message::Binary(b) = m else {
                    if matches!(m, Message::Close(_)) { break }
                    continue;
                };
                if b.len() < 5 { continue }
                let id = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                match b[4] {
                    OPEN => {
                        let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
                        conns.insert(id, in_tx);
                        let tx = out_tx.clone();
                        let local = local.to_string();
                        tokio::spawn(async move {
                            let tcp = match TcpStream::connect(&local).await {
                                Ok(t) => t,
                                Err(e) => {
                                    eprintln!("simpool: connecting to {local}: {e}");
                                    let _ = tx.send(frame(id, CLOSE, &[])).await;
                                    return;
                                }
                            };
                            let (mut rd, mut wr) = tcp.into_split();
                            let writer = tokio::spawn(async move {
                                while let Some(d) = in_rx.recv().await {
                                    if wr.write_all(&d).await.is_err() { break }
                                }
                                let _ = wr.shutdown().await;
                            });
                            let mut buf = vec![0u8; 32 << 10];
                            while let Ok(n) = rd.read(&mut buf).await {
                                if n == 0 || tx.send(frame(id, DATA, &buf[..n])).await.is_err() { break }
                            }
                            let _ = tx.send(frame(id, CLOSE, &[])).await;
                            let _ = writer.await;
                        });
                    }
                    DATA => if let Some(tx) = conns.get(&id) {
                        let _ = tx.send(b[5..].to_vec()).await;
                    },
                    CLOSE => { conns.remove(&id); }
                    _ => {}
                }
            }
        }
    }
    bail!("the server closed the tunnel (lease released?)")
}
