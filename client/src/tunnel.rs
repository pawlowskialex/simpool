//! Local ends of the server's WebSocket tunnels.

use crate::Client;
use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn connect(c: &Client, path: &str) -> Result<Ws> {
    let url = c.url(path).replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
    let mut req = url.as_str().into_client_request()?;
    if let Some(t) = &c.token {
        req.headers_mut().insert("authorization", format!("Bearer {t}").parse()?);
    }
    match tokio_tungstenite::connect_async(req).await {
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
