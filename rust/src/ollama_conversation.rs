//! Opt-in Windows chat adapter. The operator owns the separately installed
//! Ollama daemon; Neura owns only its cancellable request, never that process.
use crate::ollama_latency::{RequestTiming, TIMING_FIELDS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex, OnceLock},
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::local_model::{bounded_json, memory_admitted, ENDPOINT, LOCAL_MODEL};
const MAX_REQUEST: usize = 16_384;
const MAX_RESPONSE: usize = 32_768;
const MAX_REPLY: usize = 8_192;
/// Streaming sends one small NDJSON object per token, so the wire total is
/// larger than the reply. Both the total and every single line stay bounded.
const MAX_STREAM: usize = 524_288;
const MAX_STREAM_LINE: usize = 16_384;
const READINESS_FIELDS: usize = 7;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    role: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    system: String,
    messages: Vec<Turn>,
    /// Optional, user-selected conversation profile. Absent means `compact`,
    /// the original limits, so existing callers keep identical behavior.
    #[serde(default)]
    profile: Option<String>,
    /// Optional, user-selected "keep warm": bounded model residency between
    /// replies. Absent or false keeps the original `keep_alive: 0`.
    #[serde(default)]
    warm: Option<bool>,
    /// Optional incremental delivery. When true the adapter reads Ollama's
    /// NDJSON stream and exposes the growing text through
    /// `neura_ollama_partial`, under the same validation, bounds and
    /// cancellation as a complete reply. Screening stays with the caller,
    /// which must check text before it is displayed or spoken.
    #[serde(default)]
    stream: Option<bool>,
}

/// Bounded conversation profiles. Each larger profile raises both the native
/// context window and its own RAM admission floor, so it is refused rather
/// than silently degraded on a machine that cannot hold it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Compact,
    Standard,
    Extended,
}

impl Profile {
    fn parse(value: Option<&str>) -> Result<Self, ()> {
        match value {
            None | Some("compact") => Ok(Self::Compact),
            Some("standard") => Ok(Self::Standard),
            Some("extended") => Ok(Self::Extended),
            Some(_) => Err(()),
        }
    }
    /// Whole recent turns retained, in UTF-8 bytes and in count.
    fn context_bytes(self) -> usize {
        match self {
            Self::Compact => 1_500,
            Self::Standard => 4_000,
            Self::Extended => 8_000,
        }
    }
    fn max_turns(self) -> usize {
        match self {
            Self::Compact => 8,
            Self::Standard => 12,
            Self::Extended => 16,
        }
    }
    fn num_ctx(self) -> u32 {
        match self {
            Self::Compact => 2_048,
            Self::Standard => 4_096,
            Self::Extended => 8_192,
        }
    }
    fn num_predict(self) -> u32 {
        match self {
            Self::Compact => 256,
            Self::Standard => 512,
            Self::Extended => 1_024,
        }
    }
    /// Compact keeps the measured Windows floor. Larger windows add KV-cache
    /// memory, so they require more headroom. Device measurement remains
    /// required before these floors are relaxed.
    pub(crate) fn admits(self, total_mib: u64, available_mib: u64) -> bool {
        if !crate::local_model::memory_values_admitted(total_mib, available_mib) {
            return false;
        }
        match self {
            Self::Compact => true,
            Self::Standard => available_mib >= 4_096,
            Self::Extended => total_mib >= 15_360 && available_mib >= 8_192,
        }
    }
}

/// Residency while the user explicitly keeps the model warm. Ollama unloads
/// the model by itself after this idle period; no Neura process holds it.
const WARM_KEEP_ALIVE: &str = "3m";

#[cfg(test)]
fn prepare(raw: &[u8]) -> Result<Value, ()> {
    prepare_with_profile(raw).map(|(payload, _)| payload)
}

fn prepare_with_profile(raw: &[u8]) -> Result<(Value, Profile), ()> {
    let request: Request = serde_json::from_slice(raw).map_err(|_| ())?;
    let profile = Profile::parse(request.profile.as_deref())?;
    if request.system.is_empty()
        || request.system.len() > 6_500
        || request.messages.is_empty()
        || request.messages.len() > 64
        || request.messages.last().map(|t| t.role.as_str()) != Some("user")
        || request
            .messages
            .iter()
            .any(|t| !matches!(t.role.as_str(), "user" | "assistant"))
    {
        return Err(());
    }
    // Retain whole recent turns, never truncate the charter or newest message.
    // A per-profile byte budget complements Ollama's hard token context limit.
    let mut retained = Vec::new();
    let mut bytes = 0;
    for turn in request.messages.into_iter().rev().take(profile.max_turns()) {
        if bytes + turn.content.len() > profile.context_bytes() {
            if retained.is_empty() {
                return Err(());
            }
            break;
        }
        bytes += turn.content.len();
        retained.push(turn);
    }
    retained.reverse();
    while retained.first().is_some_and(|t| t.role != "user") {
        retained.remove(0);
    }
    let mut messages = vec![json!({"role":"system", "content": request.system})];
    messages.extend(retained.into_iter().map(|t| json!(t)));
    let keep_alive = if request.warm == Some(true) {
        json!(WARM_KEEP_ALIVE)
    } else {
        json!(0)
    };
    Ok((
        json!({"model": LOCAL_MODEL, "messages": messages,
        "stream": request.stream == Some(true), "think": false,
        "keep_alive": keep_alive, "options": {"num_ctx":profile.num_ctx(),
        "num_predict":profile.num_predict(), "num_thread":2,"temperature":0.6}}),
        profile,
    ))
}

#[cfg(test)]
async fn inference(endpoint: &str, payload: Value) -> Result<String, ()> {
    timed_inference(
        endpoint,
        payload,
        &Mutex::new(RequestTiming::default()),
        &Partial::default(),
    )
    .await
}

/// Growing streamed text for one request. Zeroized on replacement and drop.
type Partial = Mutex<Zeroizing<Vec<u8>>>;

/// Validate one streamed or complete chat object and append its content.
/// Returns whether Ollama marked the reply done.
fn accept_chunk(value: &Value, text: &mut Zeroizing<String>) -> Result<bool, ()> {
    let message = &value["message"];
    if value.get("remote_host").is_some()
        || value.get("remote_model").is_some()
        || value.get("error").is_some()
        || message["role"] != "assistant"
        || message
            .get("tool_calls")
            .is_some_and(|v| !v.as_array().is_some_and(|a| a.is_empty()))
    {
        return Err(());
    }
    let content = message["content"].as_str().ok_or(())?;
    if text.len() + content.len() > MAX_REPLY {
        return Err(());
    }
    text.push_str(content);
    Ok(value["done"] == true)
}

async fn read_stream(mut response: reqwest::Response, partial: &Partial) -> Result<String, ()> {
    if !response.status().is_success() {
        return Err(());
    }
    let mut pending = Zeroizing::new(Vec::new());
    let mut text = Zeroizing::new(String::new());
    let mut total = 0usize;
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        total += chunk.len();
        if total > MAX_STREAM {
            return Err(());
        }
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line = Zeroizing::new(pending.drain(..=end).collect::<Vec<u8>>());
            if line.len() > MAX_STREAM_LINE {
                return Err(());
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = serde_json::from_slice(&line).map_err(|_| ())?;
            let done = accept_chunk(&value, &mut text)?;
            *partial.lock().unwrap_or_else(|p| p.into_inner()) =
                Zeroizing::new(text.as_bytes().to_vec());
            if done {
                if text.trim().is_empty() {
                    return Err(());
                }
                return Ok(text.to_string());
            }
        }
        if pending.len() > MAX_STREAM_LINE {
            return Err(());
        }
    }
    // The stream ended without a final `done` object: incomplete, not a reply.
    Err(())
}

async fn timed_inference(
    endpoint: &str,
    payload: Value,
    timing: &Mutex<RequestTiming>,
    partial: &Partial,
) -> Result<String, ()> {
    let client = crate::local_model::LoopbackClient::new(endpoint, Duration::from_secs(90))?;
    // Fail before submitting conversation text if this tag is a remote model
    // or has been replaced with a different family/size/quantization.
    crate::local_model::verify_pinned_model(&client)
        .await
        .map_err(|_| ())?;
    timing
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .preflight_done();
    let streaming = payload["stream"] == true;
    let response = client
        .post_json("/api/chat", &payload)
        .send()
        .await
        .map_err(|_| ())?;
    if streaming {
        return read_stream(response, partial).await;
    }
    let response = bounded_json(response, MAX_RESPONSE).await?;
    let mut text = Zeroizing::new(String::new());
    if !accept_chunk(&response, &mut text)? || text.trim().is_empty() {
        return Err(());
    }
    Ok(text.to_string())
}

async fn cancellable_inference(
    endpoint: &str,
    payload: Value,
    timing: &Mutex<RequestTiming>,
    partial: &Partial,
    receiver: &mut watch::Receiver<bool>,
) -> Result<String, ()> {
    tokio::select! { biased;
        _ = receiver.changed() => Err(()),
        result = tokio::time::timeout(
            Duration::from_secs(90),
            timed_inference(endpoint, payload, timing, partial),
        ) => result.map_err(|_| ())?,
    }
}

fn readiness_snapshot(
    model: crate::local_model::ModelReadiness,
    memory: Option<(u64, u64)>,
) -> [u64; READINESS_FIELDS] {
    let (total, available) = memory.unwrap_or_default();
    let admission = match memory {
        None => 0,
        Some((total, available))
            if crate::local_model::memory_values_admitted(total, available) =>
        {
            1
        }
        Some(_) => 2,
    };
    [
        1,
        model.daemon,
        model.version,
        model.model,
        admission,
        total,
        available,
    ]
}

async fn cancellable_readiness(
    endpoint: &str,
    receiver: &mut watch::Receiver<bool>,
) -> Result<[u64; READINESS_FIELDS], ()> {
    tokio::select! { biased;
        _ = receiver.changed() => Err(()),
        result = tokio::time::timeout(Duration::from_secs(10), async {
            let client = crate::local_model::LoopbackClient::new(endpoint, Duration::from_secs(3))?;
            let model = crate::local_model::inspect_pinned_model(&client).await;
            Ok(readiness_snapshot(model, crate::local_model::memory_snapshot()))
        }) => result.map_err(|_| ())?,
    }
}

enum Completion {
    Reply(String),
    Readiness([u64; READINESS_FIELDS]),
}

#[derive(Default)]
struct State {
    generation: i64,
    foreground: bool,
    blocked: bool,
    status: i32,
    reply: Option<Zeroizing<Vec<u8>>>,
    readiness: Option<[u64; READINESS_FIELDS]>,
    timing: Option<(i64, Arc<Mutex<RequestTiming>>)>,
    partial: Option<Arc<Partial>>,
}
struct Active {
    cancel: watch::Sender<bool>,
    worker: JoinHandle<()>,
}
#[derive(Default)]
struct Session {
    state: Arc<Mutex<State>>,
    active: Option<Active>,
}
impl Session {
    fn cancel(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, timing)) = &state.timing {
            timing.lock().unwrap_or_else(|p| p.into_inner()).cancel();
        }
        state.generation += 1;
        state.reply = None;
        state.partial = None;
        state.readiness = None;
        state.status = -2;
        if let Some(active) = &self.active {
            active.cancel.send_replace(true);
        }
    }
    fn begin(&mut self, payload: Value, profile: Profile) -> i64 {
        self.begin_checked(payload, || {
            memory_admitted()
                && crate::local_model::memory_snapshot()
                    .is_some_and(|(total, available)| profile.admits(total, available))
        })
    }
    fn begin_checked(&mut self, payload: Value, admit_memory: impl FnOnce() -> bool) -> i64 {
        self.begin_work(Some(payload), admit_memory)
    }
    fn begin_readiness(&mut self) -> i64 {
        // Metadata inspection never loads weights, even when memory is low.
        self.begin_work(None, || true)
    }
    fn begin_work(&mut self, payload: Option<Value>, admit_memory: impl FnOnce() -> bool) -> i64 {
        {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.blocked || !state.foreground {
                return -2;
            }
        }
        if !admit_memory() {
            return -4;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|a| !a.worker.is_finished())
        {
            return -3;
        }
        if let Some(active) = self.active.take() {
            let _ = active.worker.join();
        }
        self.cancel();
        let timing = payload
            .as_ref()
            .map(|_| Arc::new(Mutex::new(RequestTiming::default())));
        let partial = payload.as_ref().map(|_| Arc::new(Partial::default()));
        let generation = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.status = 0;
            state.partial = partial.clone();
            state.timing = timing
                .as_ref()
                .map(|value| (state.generation, value.clone()));
            state.generation
        };
        let (cancel, mut receiver) = watch::channel(false);
        let state = self.state.clone();
        let worker = std::thread::Builder::new()
            .name("neura-ollama".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| ())?;
                    tracing::subscriber::with_default(
                        tracing::subscriber::NoSubscriber::default(),
                        || {
                            runtime.block_on(async {
                                match payload {
                                    Some(payload) => cancellable_inference(
                                        ENDPOINT,
                                        payload,
                                        timing.as_ref().ok_or(())?,
                                        partial.as_deref().ok_or(())?,
                                        &mut receiver,
                                    )
                                    .await
                                    .map(Completion::Reply),
                                    None => cancellable_readiness(ENDPOINT, &mut receiver)
                                        .await
                                        .map(Completion::Readiness),
                                }
                            })
                        },
                    )
                }));
                let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(timing) = timing {
                    timing
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .worker_done(matches!(&result, Ok(Ok(_))));
                }
                if generation != state.generation || state.blocked || !state.foreground {
                    return;
                }
                // The complete reply supersedes the partial view.
                state.partial = None;
                match result {
                    Ok(Ok(Completion::Reply(text))) => {
                        state.reply = Some(Zeroizing::new(text.into_bytes()));
                        state.status = 1;
                    }
                    Ok(Ok(Completion::Readiness(snapshot))) => {
                        state.readiness = Some(snapshot);
                        state.status = 1;
                    }
                    _ => state.status = -1,
                }
            });
        match worker {
            Ok(worker) => {
                self.active = Some(Active { cancel, worker });
                generation
            }
            Err(_) => {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.status = -1;
                if let Some((_, timing)) = &state.timing {
                    timing
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .worker_done(false);
                }
                -1
            }
        }
    }
}
fn session() -> &'static Mutex<Session> {
    static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(Session::default()))
}

/// # Safety
/// `data` must point to `len` readable bytes for this synchronous call.
#[no_mangle]
pub unsafe extern "C" fn neura_ollama_begin(data: *const u8, len: usize) -> i64 {
    if data.is_null() || len == 0 || len > MAX_REQUEST {
        return -1;
    }
    // SAFETY: the native owner supplies its live std::string allocation.
    let raw = unsafe { std::slice::from_raw_parts(data, len) };
    let Ok((payload, profile)) = prepare_with_profile(raw) else {
        return -1;
    };
    session()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .begin(payload, profile)
}

/// Explicit, argument-free metadata inspection. Shares the cancellable worker,
/// foreground ownership, busy rejection and latched safety block with chat.
#[no_mangle]
pub extern "C" fn neura_ollama_readiness_begin() -> i64 {
    session()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .begin_readiness()
}

/// # Safety
/// `buffer` must point to `capacity` aligned, writable u64 elements.
#[no_mangle]
pub unsafe extern "C" fn neura_ollama_readiness_poll(
    id: i64,
    buffer: *mut u64,
    capacity: usize,
) -> i32 {
    if buffer.is_null() || capacity < READINESS_FIELDS {
        return -1;
    }
    let session = session().lock().unwrap_or_else(|p| p.into_inner());
    let mut state = session.state.lock().unwrap_or_else(|p| p.into_inner());
    if id != state.generation || state.blocked || !state.foreground {
        return -2;
    }
    if state.status != 1 {
        return state.status;
    }
    let Some(snapshot) = state.readiness.take() else {
        return -1;
    };
    // SAFETY: the caller supplies a separate aligned output buffer of sufficient size.
    unsafe {
        std::ptr::copy_nonoverlapping(snapshot.as_ptr(), buffer, READINESS_FIELDS);
    }
    state.status = -2;
    READINESS_FIELDS as i32
}
/// # Safety
/// `buffer` must point to `capacity` writable bytes for this synchronous call.
#[no_mangle]
pub unsafe extern "C" fn neura_ollama_poll(id: i64, buffer: *mut u8, capacity: usize) -> i32 {
    let session = session().lock().unwrap_or_else(|p| p.into_inner());
    let mut state = session.state.lock().unwrap_or_else(|p| p.into_inner());
    if id != state.generation || state.blocked || !state.foreground {
        return -2;
    }
    if state.status != 1 {
        return state.status;
    }
    let Some(reply) = state.reply.as_ref() else {
        return -1;
    };
    if buffer.is_null() || capacity < reply.len() {
        return -1;
    }
    let len = reply.len();
    // SAFETY: caller provides a separate, sufficiently sized output buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(reply.as_ptr(), buffer, len);
    }
    if let Some((_, timing)) = &state.timing {
        timing.lock().unwrap_or_else(|p| p.into_inner()).delivered();
    }
    state.reply = None;
    state.status = -2;
    len as i32
}
/// Copy the text streamed so far for request `id`, without consuming it.
/// Returns its byte length (0 before the first token), -5 once the complete
/// reply is ready for `neura_ollama_poll`, -1 on failure or a too-small
/// buffer, and -2 when stopped, replaced, blocked or backgrounded. The
/// caller must screen this text before displaying or speaking any of it.
/// # Safety
/// `buffer` must point to `capacity` writable bytes for this synchronous call.
#[no_mangle]
pub unsafe extern "C" fn neura_ollama_partial(id: i64, buffer: *mut u8, capacity: usize) -> i32 {
    let session = session().lock().unwrap_or_else(|p| p.into_inner());
    let state = session.state.lock().unwrap_or_else(|p| p.into_inner());
    if id != state.generation || state.blocked || !state.foreground {
        return -2;
    }
    match state.status {
        0 => {}
        1 => return -5,
        status => return status,
    }
    let Some(partial) = state.partial.as_ref() else {
        return 0;
    };
    let text = partial.lock().unwrap_or_else(|p| p.into_inner());
    if buffer.is_null() || capacity < text.len() {
        return -1;
    }
    // SAFETY: caller provides a separate, sufficiently sized output buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
    }
    text.len() as i32
}
#[no_mangle]
pub extern "C" fn neura_ollama_cancel(id: i64) {
    let mut session = session().lock().unwrap_or_else(|p| p.into_inner());
    let current = session
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .generation;
    if id == current {
        session.cancel();
    }
}

/// Read the latest numeric request timing without starting or waiting for work.
/// A cancelled request keeps its original id solely for a foreground read.
/// # Safety
/// `buffer` must point to `capacity` aligned, writable u64 elements.
#[no_mangle]
pub unsafe extern "C" fn neura_ollama_timing(id: i64, buffer: *mut u64, capacity: usize) -> i32 {
    if buffer.is_null() || capacity < TIMING_FIELDS {
        return 0;
    }
    let session = session().lock().unwrap_or_else(|p| p.into_inner());
    let state = session.state.lock().unwrap_or_else(|p| p.into_inner());
    if state.blocked || !state.foreground {
        return 0;
    }
    let Some((request_id, timing)) = &state.timing else {
        return 0;
    };
    if *request_id != id {
        return 0;
    }
    let values = timing.lock().unwrap_or_else(|p| p.into_inner()).snapshot();
    // SAFETY: caller supplies a separate, aligned buffer with sufficient space.
    unsafe {
        std::ptr::copy_nonoverlapping(values.as_ptr(), buffer, TIMING_FIELDS);
    }
    TIMING_FIELDS as i32
}

#[no_mangle]
pub extern "C" fn neura_ollama_clear_timing() {
    session()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .timing = None;
}
#[no_mangle]
pub extern "C" fn neura_ollama_foreground(value: i32) {
    let mut session = session().lock().unwrap_or_else(|p| p.into_inner());
    session
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .foreground = value == 1;
    if value != 1 {
        session.cancel();
        session
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .timing = None;
    }
}
#[no_mangle]
pub extern "C" fn neura_ollama_block() {
    let mut session = session().lock().unwrap_or_else(|p| p.into_inner());
    session
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .blocked = true;
    session.cancel();
    session
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .timing = None;
}

/// Join the request worker before the owning DLL handle can be released.
#[no_mangle]
pub extern "C" fn neura_ollama_shutdown() {
    let mut session = session().lock().unwrap_or_else(|p| p.into_inner());
    session
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .blocked = true;
    session.cancel();
    session
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .timing = None;
    if let Some(active) = session.active.take() {
        let _ = active.worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixes_model_and_resource_limits_and_preserves_system() {
        let payload = prepare(
            br#"{"system":"Neura charter","messages":[{"role":"user","content":"Hello"}]}"#,
        )
        .unwrap();
        assert_eq!(payload["model"], LOCAL_MODEL);
        assert_eq!(payload["messages"][0]["content"], "Neura charter");
        assert_eq!(payload["options"]["num_ctx"], 2048);
        assert_eq!(payload["keep_alive"], 0);
        assert_eq!(payload["think"], false);
        assert!(payload.get("tools").is_none());
    }
    #[test]
    fn profiles_scale_context_and_reply_limits_only_when_selected() {
        let request = |extra: &str| {
            format!(
                r#"{{"system":"Neura charter","messages":[{{"role":"user","content":"Hello"}}]{extra}}}"#
            )
        };
        for (extra, ctx, predict, profile) in [
            ("", 2048, 256, Profile::Compact),
            (r#","profile":"compact""#, 2048, 256, Profile::Compact),
            (r#","profile":"standard""#, 4096, 512, Profile::Standard),
            (r#","profile":"extended""#, 8192, 1024, Profile::Extended),
        ] {
            let (payload, parsed) = prepare_with_profile(request(extra).as_bytes()).unwrap();
            assert_eq!(parsed, profile);
            assert_eq!(payload["options"]["num_ctx"], ctx);
            assert_eq!(payload["options"]["num_predict"], predict);
            assert_eq!(payload["keep_alive"], 0);
            assert!(payload.get("tools").is_none());
        }
        for bad in [
            r#","profile":"huge""#,
            r#","profile":7"#,
            r#","warm":"yes""#,
        ] {
            assert!(prepare(request(bad).as_bytes()).is_err());
        }
    }
    #[test]
    fn warm_is_explicit_and_bounded() {
        let raw = |warm: &str| {
            format!(
                r#"{{"system":"x","messages":[{{"role":"user","content":"x"}}],"warm":{warm}}}"#
            )
        };
        assert_eq!(prepare(raw("true").as_bytes()).unwrap()["keep_alive"], "3m");
        assert_eq!(prepare(raw("false").as_bytes()).unwrap()["keep_alive"], 0);
    }
    #[test]
    fn larger_profiles_retain_more_whole_turns_but_never_truncate() {
        let turns: Vec<Value> = (0..31)
            .map(|i| {
                json!({"role": if i % 2 == 0 {"user"} else {"assistant"},
                "content": format!("{i} {}", "x".repeat(400))})
            })
            .collect();
        let count = |profile: &str| {
            let raw = json!({"system":"x","messages":turns,"profile":profile}).to_string();
            let payload = prepare(raw.as_bytes()).unwrap();
            let messages = payload["messages"].as_array().unwrap().clone();
            assert_eq!(messages[1]["role"], "user");
            assert!(messages[1..]
                .iter()
                .all(|m| m["content"].as_str().unwrap().len() > 400));
            messages.len() - 1
        };
        let (compact, standard, extended) =
            (count("compact"), count("standard"), count("extended"));
        assert!(compact <= 8 && compact < standard && standard <= 12);
        assert!(standard < extended && extended <= 16);
        // A newest turn over the selected profile's budget is refused whole.
        let big = json!({"system":"x","profile":"standard",
            "messages":[{"role":"user","content":"x".repeat(4_001)}]});
        assert!(prepare(big.to_string().as_bytes()).is_err());
    }
    #[test]
    fn larger_profiles_require_more_memory_headroom() {
        assert!(Profile::Compact.admits(8_192, 3_584));
        assert!(!Profile::Compact.admits(8_192, 3_000));
        assert!(!Profile::Standard.admits(8_192, 4_000));
        assert!(Profile::Standard.admits(8_192, 4_096));
        assert!(!Profile::Extended.admits(8_192, 7_000));
        assert!(!Profile::Extended.admits(16_384, 6_000));
        assert!(Profile::Extended.admits(16_384, 8_192));
    }
    #[test]
    fn rejects_tools_images_endpoints_and_large_current_turn() {
        for raw in [
            r#"{"system":"x","messages":[{"role":"tool","content":"x"}]}"#,
            r#"{"system":"x","messages":[{"role":"user","content":"x","images":[]}]}"#,
            r#"{"system":"x","messages":[{"role":"user","content":"x"}],"endpoint":"https://example.com"}"#,
        ] {
            assert!(prepare(raw.as_bytes()).is_err());
        }
        assert!(prepare(
            json!({"system":"x","messages":[{"role":"user","content":"x".repeat(1501)}]})
                .to_string()
                .as_bytes()
        )
        .is_err());
    }
    #[test]
    fn background_and_block_prevent_admission_and_clear_reply() {
        let mut s = Session::default();
        assert_eq!(s.begin(json!({}), Profile::Compact), -2);
        assert_eq!(s.begin_readiness(), -2);
        {
            let mut state = s.state.lock().unwrap();
            state.foreground = true;
            state.blocked = true;
            state.reply = Some(Zeroizing::new(b"private".to_vec()));
            state.readiness = Some([1, 1, 1, 1, 1, 8192, 4096]);
        }
        s.cancel();
        assert!(s.state.lock().unwrap().reply.is_none());
        assert!(s.state.lock().unwrap().readiness.is_none());
        assert_eq!(s.begin(json!({}), Profile::Compact), -2);
        assert_eq!(s.begin_readiness(), -2);
        assert!(s.active.is_none());
    }

    #[test]
    fn readiness_preserves_memory_thresholds_and_unknown_is_not_admitted() {
        for (memory, expected) in [
            (None, [0, 0, 0]),
            (Some((7168, 3584)), [1, 7168, 3584]),
            (Some((7167, 4096)), [2, 7167, 4096]),
            (Some((8192, 3583)), [2, 8192, 3583]),
        ] {
            let model = crate::local_model::ModelReadiness {
                daemon: 1,
                version: 1,
                model: 1,
            };
            let report = readiness_snapshot(model, memory);
            assert_eq!(&report[..4], &[1, 1, 1, 1]);
            assert_eq!(&report[4..], &expected);
        }
    }

    #[test]
    fn readiness_cannot_replace_an_active_worker() {
        let mut s = Session::default();
        s.state.lock().unwrap().foreground = true;
        let (cancel, _) = watch::channel(false);
        let (release, gate) = std::sync::mpsc::channel();
        s.active = Some(Active {
            cancel,
            worker: std::thread::spawn(move || {
                let _ = gate.recv();
            }),
        });
        assert_eq!(s.begin_readiness(), -3);
        release.send(()).unwrap();
        s.active.take().unwrap().worker.join().unwrap();
    }

    #[tokio::test]
    async fn readiness_cancel_drops_a_pending_metadata_request() {
        use axum::{routing::post, Json};
        let entered = Arc::new(tokio::sync::Notify::new());
        let seen = entered.clone();
        let router = crate::local_model::tests::metadata_router().route(
            "/api/show",
            post(move || {
                let seen = seen.clone();
                async move {
                    seen.notify_one();
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Json(crate::local_model::tests::pinned_info())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (cancel, mut receiver) = watch::channel(false);
        let worker =
            tokio::spawn(async move { cancellable_readiness(&endpoint, &mut receiver).await });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        cancel.send_replace(true);
        assert!(tokio::time::timeout(Duration::from_millis(200), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        server.abort();
    }

    #[test]
    fn memory_rejection_creates_neither_worker_nor_timing() {
        let mut s = Session::default();
        s.state.lock().unwrap().foreground = true;
        assert_eq!(s.begin_checked(json!({}), || false), -4);
        assert!(s.active.is_none());
        assert!(s.state.lock().unwrap().timing.is_none());
    }

    #[test]
    fn stale_cancel_cannot_wipe_a_replacement_and_revoked_reply_cannot_be_read() {
        {
            let s = session().lock().unwrap();
            let mut state = s.state.lock().unwrap();
            state.generation = 99;
            state.foreground = true;
            state.blocked = false;
            state.status = 1;
            state.reply = Some(Zeroizing::new(b"reply".to_vec()));
            let mut timing = RequestTiming::default();
            timing.preflight_done();
            timing.worker_done(true);
            state.timing = Some((99, Arc::new(Mutex::new(timing))));
        }
        neura_ollama_cancel(98);
        let mut buffer = [0u8; 8];
        // SAFETY: buffer is live and its advertised capacity is its size.
        assert_eq!(
            unsafe { neura_ollama_poll(98, buffer.as_mut_ptr(), buffer.len()) },
            -2
        );
        assert!(session()
            .lock()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .reply
            .is_some());
        neura_ollama_cancel(99);
        // SAFETY: buffer is live and its advertised capacity is its size.
        assert_eq!(
            unsafe { neura_ollama_poll(99, buffer.as_mut_ptr(), buffer.len()) },
            -2
        );
        assert_eq!(buffer, [0; 8]);
        assert!(session()
            .lock()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .reply
            .is_none());
        let mut values = [0u64; TIMING_FIELDS];
        // SAFETY: all timing calls below advertise this live aligned array.
        unsafe {
            assert_eq!(
                neura_ollama_timing(98, values.as_mut_ptr(), values.len()),
                0
            );
            assert_eq!(
                neura_ollama_timing(99, std::ptr::null_mut(), values.len()),
                0
            );
            assert_eq!(neura_ollama_timing(99, values.as_mut_ptr(), 1), 0);
            assert_eq!(
                neura_ollama_timing(99, values.as_mut_ptr(), values.len()),
                7
            );
        }
        assert_eq!(values[1], 3); // Cancellation of ready reply, no delivery.
        assert_eq!(values[4], u64::MAX);
        neura_ollama_foreground(0);
        neura_ollama_foreground(1);
        // SAFETY: same live output array; backgrounding erased the record.
        assert_eq!(
            unsafe { neura_ollama_timing(99, values.as_mut_ptr(), values.len()) },
            0
        );
        {
            let s = session().lock().unwrap();
            s.state.lock().unwrap().timing =
                Some((100, Arc::new(Mutex::new(RequestTiming::default()))));
        }
        neura_ollama_block();
        assert!(session()
            .lock()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .timing
            .is_none());
        assert_eq!(neura_ollama_readiness_begin(), -2);
        let mut readiness = [0u64; READINESS_FIELDS];
        // SAFETY: each call below advertises this live aligned output array.
        unsafe {
            assert_eq!(
                neura_ollama_readiness_poll(100, std::ptr::null_mut(), readiness.len()),
                -1
            );
            assert_eq!(
                neura_ollama_readiness_poll(100, readiness.as_mut_ptr(), 1),
                -1
            );
            assert_eq!(
                neura_ollama_readiness_poll(100, readiness.as_mut_ptr(), readiness.len()),
                -2
            );
        }
        assert_eq!(readiness, [0; READINESS_FIELDS]);
        // Reset the test fixture without reopening a real blocked session.
        *session().lock().unwrap() = Session::default();
    }

    fn local_info() -> Value {
        crate::local_model::tests::pinned_info()
    }
    fn payload() -> Value {
        prepare(br#"{"system":"Neura charter","messages":[{"role":"user","content":"Hello"}]}"#)
            .unwrap()
    }
    async fn mock(
        info: Value,
        answer: Value,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{routing::post, Json};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = crate::local_model::tests::metadata_router()
            .route("/api/show", post(move || async move { Json(info) }))
            .route(
                "/api/chat",
                post(move |Json(request): Json<Value>| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(request["model"], LOCAL_MODEL);
                    assert_eq!(request["keep_alive"], 0);
                    assert_eq!(request["think"], false);
                    assert!(request.get("tools").is_none());
                    Json(answer)
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (endpoint, calls, task)
    }
    #[tokio::test]
    async fn local_http_conversation_and_remote_model_rejection() {
        use std::sync::atomic::Ordering;
        let answer = json!({"done":true,"message":{"role":"assistant","content":"Hi, I'm Neura."}});
        let (endpoint, calls, task) = mock(local_info(), answer.clone()).await;
        assert_eq!(
            inference(&endpoint, payload()).await.unwrap(),
            "Hi, I'm Neura."
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
        let mut info = local_info();
        info["remote_host"] = json!("https://ollama.com");
        let (endpoint, calls, task) = mock(info, answer).await;
        assert!(inference(&endpoint, payload()).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.abort();
    }

    #[tokio::test]
    async fn cancelling_stalled_inference_finishes_without_delivery() {
        use axum::{routing::post, Json};
        let (reached, mut observed) = watch::channel(false);
        let router = crate::local_model::tests::metadata_router()
            .route("/api/show", post(|| async { Json(local_info()) }))
            .route(
                "/api/chat",
                post(move || async move {
                    reached.send_replace(true);
                    std::future::pending::<Json<Value>>().await
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let timing = Arc::new(Mutex::new(RequestTiming::default()));
        let worker_timing = timing.clone();
        let (cancel, mut receiver) = watch::channel(false);
        let request = tokio::spawn(async move {
            let result = cancellable_inference(
                &endpoint,
                payload(),
                &worker_timing,
                &Partial::default(),
                &mut receiver,
            )
            .await;
            worker_timing.lock().unwrap().worker_done(result.is_ok());
            result
        });
        // Synchronize on entry to inference, never on an arbitrary sleep.
        tokio::time::timeout(Duration::from_secs(2), observed.changed())
            .await
            .unwrap()
            .unwrap();
        timing.lock().unwrap().cancel();
        cancel.send_replace(true);
        assert!(tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        let values = timing.lock().unwrap().snapshot();
        assert_eq!(values[1], 3);
        assert_ne!(values[2], u64::MAX);
        assert_ne!(values[3], u64::MAX);
        assert_eq!(values[4], u64::MAX);
        assert_ne!(values[5], u64::MAX);
        server.abort();
    }
    #[tokio::test]
    async fn rejects_tool_calls_incomplete_and_oversized_replies() {
        for answer in [
            json!({"done":true,"message":{"role":"assistant","content":"x","tool_calls":[{}]}}),
            json!({"done":false,"message":{"role":"assistant","content":"x"}}),
            json!({"done":true,"message":{"role":"assistant","content":"x".repeat(MAX_REPLY+1)}}),
            json!({"done":true,"message":{"role":"assistant","content":"x".repeat(MAX_RESPONSE+1)}}),
        ] {
            let (endpoint, _, task) = mock(local_info(), answer).await;
            assert!(inference(&endpoint, payload()).await.is_err());
            task.abort();
        }
    }
    async fn stream_mock(body: String) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Json};
        let router = crate::local_model::tests::metadata_router()
            .route("/api/show", post(|| async { Json(local_info()) }))
            .route(
                "/api/chat",
                post(move |Json(request): Json<Value>| async move {
                    assert_eq!(request["stream"], true);
                    assert!(request.get("tools").is_none());
                    body
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (endpoint, task)
    }
    fn stream_payload() -> Value {
        prepare(br#"{"system":"x","messages":[{"role":"user","content":"Hi"}],"stream":true}"#)
            .unwrap()
    }
    fn line(content: &str, done: bool) -> String {
        format!(
            "{}\n",
            json!({"done":done,"message":{"role":"assistant","content":content}})
        )
    }
    #[test]
    fn streaming_is_explicit() {
        let raw = |extra: &str| {
            format!(r#"{{"system":"x","messages":[{{"role":"user","content":"x"}}]{extra}}}"#)
        };
        assert_eq!(prepare(raw("").as_bytes()).unwrap()["stream"], false);
        assert_eq!(
            prepare(raw(r#","stream":false"#).as_bytes()).unwrap()["stream"],
            false
        );
        assert_eq!(
            prepare(raw(r#","stream":true"#).as_bytes()).unwrap()["stream"],
            true
        );
        assert!(prepare(raw(r#","stream":"yes""#).as_bytes()).is_err());
    }
    #[tokio::test]
    async fn streamed_reply_accumulates_and_exposes_partial_text() {
        let body = [
            line("Hi, ", false),
            line("I'm Neura.", false),
            line("", true),
        ]
        .concat();
        let (endpoint, task) = stream_mock(body).await;
        let partial = Partial::default();
        let text = timed_inference(
            &endpoint,
            stream_payload(),
            &Mutex::new(RequestTiming::default()),
            &partial,
        )
        .await
        .unwrap();
        assert_eq!(text, "Hi, I'm Neura.");
        assert_eq!(&partial.lock().unwrap()[..], b"Hi, I'm Neura.");
        task.abort();
    }
    #[tokio::test]
    async fn partial_text_grows_before_the_stream_finishes() {
        use axum::{body::Body, routing::post, Json};
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(4);
        let body = Arc::new(Mutex::new(Some(receiver)));
        let router = crate::local_model::tests::metadata_router()
            .route("/api/show", post(|| async { Json(local_info()) }))
            .route(
                "/api/chat",
                post(move || {
                    let receiver = body.lock().unwrap().take().unwrap();
                    async move { Body::from_stream(tokio_stream_from(receiver)) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let partial = Arc::new(Partial::default());
        let worker_partial = partial.clone();
        let request = tokio::spawn(async move {
            timed_inference(
                &endpoint,
                stream_payload(),
                &Mutex::new(RequestTiming::default()),
                &worker_partial,
            )
            .await
        });
        let wait_for = |expected: &'static [u8]| {
            let partial = partial.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while partial.lock().unwrap()[..] != *expected {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
            }
        };
        sender.send(Ok(line("Hi. ", false))).await.unwrap();
        wait_for(b"Hi. ").await;
        assert!(!request.is_finished());
        sender.send(Ok(line("More.", false))).await.unwrap();
        wait_for(b"Hi. More.").await;
        assert!(!request.is_finished());
        sender.send(Ok(line("", true))).await.unwrap();
        assert_eq!(request.await.unwrap().unwrap(), "Hi. More.");
        server.abort();
    }
    fn tokio_stream_from(
        mut receiver: tokio::sync::mpsc::Receiver<Result<String, std::io::Error>>,
    ) -> impl futures_util::Stream<Item = Result<String, std::io::Error>> {
        futures_util::stream::poll_fn(move |cx| receiver.poll_recv(cx))
    }
    #[tokio::test]
    async fn streamed_reply_rejects_unsafe_incomplete_and_oversized_streams() {
        let tool =
            json!({"done":false,"message":{"role":"assistant","content":"x","tool_calls":[{}]}});
        let remote = json!({"done":false,"remote_model":"x",
            "message":{"role":"assistant","content":"x"}});
        let long_line = format!(
            "{}\n",
            json!({"done":false,"message":{"role":"assistant",
                "content":"x".repeat(MAX_STREAM_LINE)}})
        );
        for body in [
            // Never finished.
            line("Hi", false),
            // Finished but empty.
            line(" ", true),
            // Tool calls and remote models are refused mid-stream.
            format!("{}{tool}\n{}", line("Hi", false), line("", true)),
            format!("{remote}\n{}", line("", true)),
            // Malformed object.
            format!("{}not json\n{}", line("Hi", false), line("", true)),
            // Over the reply cap across many small chunks.
            [
                (0..=MAX_REPLY / 512)
                    .map(|_| line(&"x".repeat(512), false))
                    .collect::<String>(),
                line("", true),
            ]
            .concat(),
            // A single line over the line cap.
            long_line,
        ] {
            let (endpoint, task) = stream_mock(body).await;
            assert!(timed_inference(
                &endpoint,
                stream_payload(),
                &Mutex::new(RequestTiming::default()),
                &Partial::default(),
            )
            .await
            .is_err());
            task.abort();
        }
    }
    #[test]
    fn partial_view_is_cleared_by_cancel() {
        let mut s = Session::default();
        {
            let mut state = s.state.lock().unwrap();
            state.foreground = true;
            state.status = 0;
            let partial = Arc::new(Partial::default());
            *partial.lock().unwrap() = Zeroizing::new(b"Hello.".to_vec());
            state.partial = Some(partial);
        }
        assert!(s.state.lock().unwrap().partial.is_some());
        s.cancel();
        let state = s.state.lock().unwrap();
        assert!(state.partial.is_none());
        assert_eq!(state.status, -2);
    }
    #[tokio::test]
    async fn rejects_advertised_oversize_without_waiting_for_body() {
        use axum::{body::Body, http::Response, routing::get, Router};
        let router = Router::new().route(
            "/",
            get(|| async {
                let body = Body::from_stream(futures_util::stream::pending::<
                    Result<String, std::io::Error>,
                >());
                Response::builder()
                    .header("content-length", "1000000")
                    .body(body)
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let response = crate::http_client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(endpoint)
            .send()
            .await
            .unwrap();
        let result =
            tokio::time::timeout(Duration::from_secs(2), bounded_json(response, MAX_RESPONSE))
                .await;
        task.abort();
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn never_follows_redirects() {
        use axum::{response::Redirect, routing::post};
        let router = crate::local_model::tests::metadata_router().route(
            "/api/show",
            post(|| async { Redirect::temporary("http://127.0.0.1:1/forbidden") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        assert!(inference(&endpoint, payload()).await.is_err());
        task.abort();
    }
}
