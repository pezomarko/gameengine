//! The browser's `WebTransport` (WEB.md 2.1): a session, its datagrams and its streams, bound
//! by hand so that the `.wasm` carries neither a QUIC stack nor web-sys's unstable bindings.

use gm_net::control::WebAddr;
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Datagrams the browser keeps for us between two frames (WEB.md 2.1): a frame at 60 Hz sees
/// one or two snapshots, a stalled page must not hold a second of them.
const INCOMING_DATAGRAMS: u32 = 16;
/// Datagrams the browser queues for the wire: a frame writes one input per tick it stepped
/// (eight at most), in one task, and the browser sends them only once the task is over.
/// Chromium defaults the queue to **one** and drops the oldest past it, so a slow frame's
/// inputs but the last were thrown away before they left and the zone starved (its 64 Hz
/// saw 15 inputs a second from a page at 15 fps).
const OUTGOING_DATAGRAMS: u32 = 16;
/// Datagrams older than this are dropped by the browser on either side, milliseconds.
const DATAGRAM_MAX_AGE_MS: f64 = 250.0;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = WebTransport)]
    type JsWebTransport;
    #[wasm_bindgen(constructor, js_class = "WebTransport", catch)]
    fn new(url: &str, options: &JsValue) -> Result<JsWebTransport, JsValue>;
    #[wasm_bindgen(method, getter)]
    fn ready(this: &JsWebTransport) -> Promise;
    #[wasm_bindgen(method, getter)]
    fn closed(this: &JsWebTransport) -> Promise;
    #[wasm_bindgen(method, getter)]
    fn datagrams(this: &JsWebTransport) -> JsValue;
    #[wasm_bindgen(method, js_name = createBidirectionalStream)]
    fn create_bidirectional_stream(this: &JsWebTransport) -> Promise;
    #[wasm_bindgen(method)]
    fn close(this: &JsWebTransport);

    /// A `ReadableStreamDefaultReader`.
    type JsReader;
    #[wasm_bindgen(method)]
    fn read(this: &JsReader) -> Promise;
    #[wasm_bindgen(method)]
    fn cancel(this: &JsReader) -> Promise;

    /// A `WritableStreamDefaultWriter`.
    type JsWriter;
    #[wasm_bindgen(method)]
    fn write(this: &JsWriter, chunk: &Uint8Array) -> Promise;
    #[wasm_bindgen(method, js_name = close)]
    fn close_writer(this: &JsWriter) -> Promise;
}

pub fn js_text(e: &JsValue) -> String {
    if let Some(s) = e.as_string() {
        return s;
    }
    if let Ok(m) = Reflect::get(e, &"message".into())
        && let Some(s) = m.as_string()
    {
        return s;
    }
    format!("{e:?}")
}

fn get(obj: &JsValue, name: &str) -> Result<JsValue, String> {
    Reflect::get(obj, &name.into()).map_err(|e| js_text(&e))
}

fn call0(obj: &JsValue, name: &str) -> Result<JsValue, String> {
    let f: Function = get(obj, name)?
        .dyn_into()
        .map_err(|_| format!("{name} is not a function"))?;
    f.call0(obj).map_err(|e| js_text(&e))
}

/// A promise whose rejection nobody waits for must still be handled, or the console fills
/// with "uncaught (in promise)" when a session ends.
fn detach(p: Promise) {
    thread_local! {
        static NOOP: Closure<dyn FnMut(JsValue)> = Closure::new(|_| {});
    }
    NOOP.with(|noop| {
        let _ = p.catch(noop);
    });
}

/// One WebTransport session.
pub struct Session {
    wt: JsWebTransport,
    datagram_writer: JsWriter,
}

impl Session {
    /// Open a session and wait until it is established.
    pub async fn connect(addr: &WebAddr) -> Result<Session, String> {
        let options = Object::new();
        let set = |k: &str, v: JsValue| {
            let _ = Reflect::set(&options, &k.into(), &v);
        };
        // One session per connection (our budgets and the fixed window are per connection),
        // and no fallback to a transport without datagrams.
        set("allowPooling", JsValue::FALSE);
        set("requireUnreliable", JsValue::TRUE);
        set("congestionControl", "low-latency".into());
        if let Some(hash) = &addr.cert_sha256 {
            let entry = Object::new();
            let _ = Reflect::set(&entry, &"algorithm".into(), &"sha-256".into());
            let _ = Reflect::set(&entry, &"value".into(), &Uint8Array::from(&hash[..]));
            set("serverCertificateHashes", Array::of1(&entry).into());
        }
        let wt = JsWebTransport::new(&addr.url, &options).map_err(|e| js_text(&e))?;
        JsFuture::from(wt.ready())
            .await
            .map_err(|e| format!("connecting to {}: {}", addr.url, js_text(&e)))?;
        let datagrams = wt.datagrams();
        // The queue between the network and the page: short and young, so that a stalled
        // page comes back to the present instead of replaying the past. Chromium names the
        // limit `incomingMaxBufferedDatagrams` and defaults it to one.
        for (k, v) in [
            ("incomingMaxAge", JsValue::from(DATAGRAM_MAX_AGE_MS)),
            ("outgoingMaxAge", JsValue::from(DATAGRAM_MAX_AGE_MS)),
            ("incomingHighWaterMark", JsValue::from(INCOMING_DATAGRAMS)),
            ("outgoingHighWaterMark", JsValue::from(OUTGOING_DATAGRAMS)),
            (
                "incomingMaxBufferedDatagrams",
                JsValue::from(INCOMING_DATAGRAMS),
            ),
        ] {
            let _ = Reflect::set(&datagrams, &k.into(), &v);
        }
        let writable = get(&datagrams, "writable")?;
        let datagram_writer: JsWriter = call0(&writable, "getWriter")?.unchecked_into();
        Ok(Session {
            wt,
            datagram_writer,
        })
    }

    /// Send one datagram; it is the browser's to drop.
    pub fn send_datagram(&self, bytes: &[u8]) {
        detach(self.datagram_writer.write(&Uint8Array::from(bytes)));
    }

    /// The reader of incoming datagrams; take it once.
    pub fn datagram_reader(&self) -> Result<Reader, String> {
        let readable = get(&self.wt.datagrams(), "readable")?;
        Reader::new(&readable)
    }

    pub async fn open_bi(&self) -> Result<(Writer, Reader), String> {
        let stream = JsFuture::from(self.wt.create_bidirectional_stream())
            .await
            .map_err(|e| js_text(&e))?;
        let writer: JsWriter = call0(&get(&stream, "writable")?, "getWriter")?.unchecked_into();
        let reader = Reader::new(&get(&stream, "readable")?)?;
        Ok((Writer { writer }, reader))
    }

    /// Resolves when the session has ended, with the reason as far as the browser tells it.
    pub async fn closed(&self) -> String {
        match JsFuture::from(self.wt.closed()).await {
            Ok(info) => {
                let reason = get(&info, "reason").ok().and_then(|r| r.as_string());
                match reason {
                    Some(r) if !r.is_empty() => format!("connection closed: {r}"),
                    _ => "connection closed".into(),
                }
            }
            Err(e) => format!("connection lost: {}", js_text(&e)),
        }
    }

    pub fn close(&self) {
        self.wt.close();
    }
}

/// The reading side of a stream, or the incoming datagrams.
pub struct Reader {
    reader: JsReader,
    /// Stream bytes read and not yet consumed.
    buf: Vec<u8>,
    done: bool,
}

impl Reader {
    fn new(readable: &JsValue) -> Result<Reader, String> {
        Ok(Reader {
            reader: call0(readable, "getReader")?.unchecked_into(),
            buf: Vec::new(),
            done: false,
        })
    }

    /// The next chunk as the browser hands it over: one whole datagram, or some bytes of a
    /// stream. `None` at the end.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.done {
            return Ok(None);
        }
        let result = match JsFuture::from(self.reader.read()).await {
            Ok(r) => r,
            Err(e) => {
                // An errored stream needs no cancel.
                self.done = true;
                return Err(js_text(&e));
            }
        };
        if get(&result, "done")?.is_truthy() {
            self.done = true;
            return Ok(None);
        }
        let value: Uint8Array = get(&result, "value")?
            .dyn_into()
            .map_err(|_| "the stream did not yield bytes".to_string())?;
        Ok(Some(value.to_vec()))
    }

    /// Exactly `n` bytes of a stream; `None` when the stream ended cleanly before the first.
    pub async fn read_exact(&mut self, n: usize) -> Result<Option<Vec<u8>>, String> {
        while self.buf.len() < n {
            match self.chunk().await? {
                Some(bytes) => self.buf.extend_from_slice(&bytes),
                None if self.buf.is_empty() => return Ok(None),
                None => return Err("the stream ended in the middle of a message".into()),
            }
        }
        let rest = self.buf.split_off(n);
        Ok(Some(std::mem::replace(&mut self.buf, rest)))
    }

    /// One length-prefixed message (PROTOCOL.md 8 framing); `None` at a clean end.
    pub async fn frame(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(len) = self.read_exact(2).await? else {
            return Ok(None);
        };
        let len = u16::from_be_bytes([len[0], len[1]]) as usize;
        match self.read_exact(len).await? {
            Some(payload) => Ok(Some(payload)),
            None if len == 0 => Ok(Some(Vec::new())),
            None => Err("the stream ended in the middle of a message".into()),
        }
    }
}

impl Drop for Reader {
    /// A reader given up before its stream ended (a timed-out download, an answer too
    /// large) tells the peer to stop sending; otherwise the browser keeps buffering.
    fn drop(&mut self) {
        if !self.done {
            detach(self.reader.cancel());
        }
    }
}

/// The writing side of a stream. Writes queue in order inside the browser.
pub struct Writer {
    writer: JsWriter,
}

impl Writer {
    /// Queue bytes without waiting for them to leave.
    pub fn write_detached(&self, bytes: &[u8]) {
        detach(self.writer.write(&Uint8Array::from(bytes)));
    }

    pub async fn write(&self, bytes: &[u8]) -> Result<(), String> {
        JsFuture::from(self.writer.write(&Uint8Array::from(bytes)))
            .await
            .map(|_| ())
            .map_err(|e| js_text(&e))
    }

    /// Finish the stream (a clean FIN) once everything queued has been sent.
    pub fn finish(&self) {
        detach(self.writer.close_writer());
    }
}
