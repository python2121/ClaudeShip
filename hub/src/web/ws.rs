//! A browser terminal: one WebSocket attached to one session. Binary
//! messages are pty bytes both ways; text messages are JSON control —
//! from the page `{type: resize|fit|ping}`, to it `size`, `exit`, `gone`,
//! `pong`. The hub sees it as one more client with a `Sink`, like a
//! terminal on the Unix socket.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::{ConnectionSlot, Shared};
use crate::hub::{Command, clamp_size};
use crate::local::NEXT_CLIENT;
use crate::session::{Event, Sink};

/// Output goes out in messages no larger than this, so that on a slow link
/// control messages interleave with a big redraw.
const CHUNK: usize = 256 * 1024;
/// A peer that hasn't taken a message in this long while output waits for
/// it has stopped reading; it can reconnect and take the replay.
const STALL: Duration = Duration::from_secs(60);
/// How long to wait for the peer's answer to our close frame.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// What the page asked for in the upgrade's query.
pub struct Attach {
    pub id: String,
    /// `None` if the query's size is out of range: told `gone`.
    pub size: Option<(u16, u16)>,
    pub claim: bool,
}

fn number(object: &Value, key: &str) -> i64 {
    let value = &object[key];
    value
        .as_i64()
        .or_else(|| value.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .unwrap_or(0)
}

/// Why sending stopped: the peer can't be written to (gone, stalled, cut
/// off, unpaired), as opposed to us closing it on purpose.
struct Dropped;

/// Send one message, giving up on a peer that doesn't take it within
/// `STALL`, or when the client is cut off or every web connection must go.
async fn send(
    socket: &mut WebSocket,
    message: Message,
    abort: &mut watch::Receiver<bool>,
    epoch: &mut watch::Receiver<u64>,
) -> Result<(), Dropped> {
    tokio::select! {
        sent = tokio::time::timeout(STALL, socket.send(message)) => match sent {
            Ok(Ok(())) => Ok(()),
            _ => Err(Dropped),
        },
        _ = abort.wait_for(|&a| a) => Err(Dropped),
        _ = epoch.changed() => Err(Dropped),
    }
}

fn text(value: Value) -> Message {
    Message::Text(value.to_string().into())
}

pub async fn run(
    socket: WebSocket,
    shared: Arc<Shared>,
    attach: Attach,
    slot: Option<Arc<ConnectionSlot>>,
    // From before the upgrade's secret was checked, so a rotation that
    // lands while the upgrade completes still counts.
    epoch: watch::Receiver<u64>,
) {
    run_on(socket, shared.hub.clone(), attach, slot, epoch).await;
}

/// `run` with only the hub's command channel: what a peer's relayed
/// terminal (`/peer/ws/term`, on `LocalOnly`) gets.
pub async fn run_on(
    mut socket: WebSocket,
    hub: mpsc::UnboundedSender<Command>,
    attach: Attach,
    slot: Option<Arc<ConnectionSlot>>,
    mut epoch: watch::Receiver<u64>,
) {
    let (sink, mut events, mut abort) = Sink::new();
    let Some((rows, cols)) = attach.size else {
        let _ = send(&mut socket, text(json!({"type": "gone"})), &mut abort, &mut epoch).await;
        close(&mut socket).await;
        return;
    };
    let client = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    if hub
        .send(Command::Connected {
            client,
            sink: sink.clone(),
        })
        .is_err()
    {
        return;
    }
    let _ = hub.send(Command::WebAttach {
        client,
        id: attach.id,
        rows,
        cols,
        claim: attach.claim,
    });
    let mut closing = false;
    loop {
        enum Next {
            Message(Option<Result<Message, axum::Error>>),
            Event(Option<Event>),
            Stop,
        }
        let next = tokio::select! {
            message = socket.recv() => Next::Message(message),
            event = events.rx.recv() => Next::Event(event),
            _ = abort.wait_for(|&a| a) => Next::Stop,
            _ = epoch.changed() => Next::Stop,
        };
        match next {
            Next::Stop | Next::Message(None | Some(Err(_))) | Next::Event(None) => break,
            Next::Message(Some(Ok(message))) => {
                let command = match message {
                    Message::Binary(data) => Command::Input {
                        client,
                        data: data.to_vec(),
                    },
                    Message::Text(body) => {
                        let object: Value =
                            serde_json::from_str(body.as_str()).unwrap_or(Value::Null);
                        let size = clamp_size(number(&object, "rows"), number(&object, "cols"));
                        match (object["type"].as_str(), size) {
                            (Some("resize"), Some((rows, cols))) => {
                                Command::Resize { client, rows, cols }
                            }
                            (Some("fit"), Some((rows, cols))) => Command::Fit { client, rows, cols },
                            (Some("ping"), _) => {
                                // The page's liveness probe: a half-open
                                // socket never answers.
                                let pong = text(json!({"type": "pong"}));
                                if send(&mut socket, pong, &mut abort, &mut epoch)
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    // The library answers pings and the peer's close; the
                    // next read sends the reply and then ends.
                    Message::Close(_) => {
                        closing = true;
                        continue;
                    }
                    _ => continue,
                };
                if !closing {
                    let _ = hub.send(command);
                }
            }
            Next::Event(Some(event)) => {
                let finished = deliver(&mut socket, &event, &mut abort, &mut epoch).await;
                events.written(&event);
                match finished {
                    Ok(false) => {}
                    Ok(true) => {
                        close(&mut socket).await;
                        break;
                    }
                    Err(Dropped) => break,
                }
            }
        }
    }
    let _ = hub.send(Command::Closed { client });
    sink.abort();
    drop(slot);
}

/// Write one event in the page's terms. `Ok(true)`: that was the last
/// thing to say (the session ended, or never existed).
async fn deliver(
    socket: &mut WebSocket,
    event: &Event,
    abort: &mut watch::Receiver<bool>,
    epoch: &mut watch::Receiver<u64>,
) -> Result<bool, Dropped> {
    match event {
        Event::Output(data) => {
            for chunk in data.chunks(CHUNK) {
                send(socket, Message::Binary(chunk.to_vec().into()), abort, epoch).await?;
            }
            Ok(false)
        }
        Event::Size { rows, cols, owner } => {
            let size = json!({"type": "size", "rows": rows, "cols": cols, "owner": owner});
            send(socket, text(size), abort, epoch).await?;
            Ok(false)
        }
        Event::Exit(code) => {
            send(socket, text(json!({"type": "exit", "code": code})), abort, epoch).await?;
            Ok(true)
        }
        Event::Gone => {
            send(socket, text(json!({"type": "gone"})), abort, epoch).await?;
            Ok(true)
        }
        // Terminal-client and request events never reach a browser.
        Event::Attached { .. } => Ok(false),
        Event::Reply(..) | Event::Error(_) => Ok(true),
    }
}

/// A normal close (1000), and a moment for the peer to answer it.
async fn close(socket: &mut WebSocket) {
    let frame = CloseFrame {
        code: 1000,
        reason: "".into(),
    };
    let sent = tokio::time::timeout(CLOSE_GRACE, socket.send(Message::Close(Some(frame)))).await;
    if !matches!(sent, Ok(Ok(()))) {
        return;
    }
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        while let Some(Ok(_)) = socket.recv().await {}
    })
    .await;
}
