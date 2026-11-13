//! CDP client over one browser-level WebSocket using flat sessions.
//! A reader thread routes responses by id and hands events to a callback.

use crate::json::{self, Value};
use crate::ws::{self, Msg, WsWriter};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Event {
    pub session: String,
    pub method: String,
    pub params: Value,
}

/// Runs on the reader thread: must not block and must not use `call`
/// (the response could never be read). `send` is fine.
pub type Handler = Box<dyn Fn(&Cdp, Event) + Send + Sync>;

#[derive(Debug)]
pub enum CdpErr {
    Timeout,
    Cancelled,
    Dead,
    Remote(String),
}

pub struct Cdp {
    writer: Mutex<WsWriter>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Result<Value, String>>>>,
    dead: AtomicBool,
}

impl Cdp {
    pub fn connect(port: u16, path: &str, handler: Handler) -> io::Result<Arc<Cdp>> {
        let (mut reader, writer) = ws::connect(port, path)?;
        let cdp = Arc::new(Cdp {
            writer: Mutex::new(writer),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        });
        let me = cdp.clone();
        std::thread::Builder::new().name("cdp-reader".into()).spawn(move || {
            loop {
                match reader.read() {
                    Ok(Msg::Text(text)) => me.dispatch(&text, &handler),
                    Ok(Msg::Ping(p)) => {
                        let _ = me.writer.lock().unwrap().send(ws::OP_PONG, &p);
                    }
                    Ok(Msg::Binary(_)) => {}
                    Ok(Msg::Close) | Err(_) => break,
                }
            }
            me.dead.store(true, Ordering::SeqCst);
            // Dropping the senders fails every in-flight call with `Dead`.
            me.pending.lock().unwrap().clear();
            crate::util::log("cdp connection closed");
        })?;
        Ok(cdp)
    }

    fn dispatch(&self, text: &str, handler: &Handler) {
        let msg = match json::parse(text) {
            Ok(m) => m,
            Err(e) => {
                crate::util::log(&format!("bad cdp json: {}", e));
                return;
            }
        };
        if let Some(id) = msg.get("id").as_f64() {
            if let Some(tx) = self.pending.lock().unwrap().remove(&(id as u64)) {
                let err = msg.get("error");
                let _ = tx.send(if err.is_null() {
                    Ok(msg.get("result").clone())
                } else {
                    Err(err.get("message").str().to_string())
                });
            }
        } else if let Some(method) = msg.get("method").as_str() {
            handler(
                self,
                Event {
                    session: msg.get("sessionId").str().to_string(),
                    method: method.to_string(),
                    params: msg.get("params").clone(),
                },
            );
        }
    }

    fn write(&self, id: u64, session: &str, method: &str, params: Value) -> bool {
        let mut msg = obj! { "id" => id, "method" => method, "params" => params };
        if !session.is_empty() {
            msg.set("sessionId", session);
        }
        self.writer.lock().unwrap().send_text(&msg.to_string()).is_ok()
    }

    /// Fire a command without waiting for its response.
    pub fn send(&self, session: &str, method: &str, params: Value) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.write(id, session, method, params);
    }

    pub fn call(&self, session: &str, method: &str, params: Value, timeout: Duration) -> Result<Value, CdpErr> {
        self.call_c(session, method, params, timeout, &|| false)
    }

    /// Like `call`, but gives up with `Cancelled` once `cancel()` is true
    /// (used to stop waiting when a JavaScript dialog blocks the page).
    pub fn call_c(
        &self,
        session: &str,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Value, CdpErr> {
        if self.is_dead() {
            return Err(CdpErr::Dead);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if !self.write(id, session, method, params) {
            self.pending.lock().unwrap().remove(&id);
            return Err(CdpErr::Dead);
        }
        let end = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(m)) => return Err(CdpErr::Remote(m)),
                Err(RecvTimeoutError::Disconnected) => return Err(CdpErr::Dead),
                Err(RecvTimeoutError::Timeout) => {
                    let out = if cancel() {
                        Some(CdpErr::Cancelled)
                    } else if Instant::now() >= end {
                        Some(CdpErr::Timeout)
                    } else {
                        None
                    };
                    if let Some(e) = out {
                        self.pending.lock().unwrap().remove(&id);
                        return Err(e);
                    }
                }
            }
        }
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        self.writer.lock().unwrap().shutdown();
    }
}
