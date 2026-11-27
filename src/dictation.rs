//! On-device speech-to-text dictation through Apple's Speech and `AVFAudio` frameworks
//! (`docs/review-assist.md`, "Dictation").
//!
//! Pure logic ([`Rollover`] and [`level`]) lives outside the framework bindings so it can be
//! unit tested without a microphone or an app bundle. The macOS bindings live in the private
//! `macos` submodule, which is the crate's second allowed `unsafe` module (approved by the owner
//! on 2026-11-25).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

/// Events pushed from native dictation through the event loop to `window.postil.dictation`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum Event {
    /// Lifecycle or permission state change.
    State {
        state: State,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Normalized microphone input level in `0.0..=1.0`, emitted at ~25 Hz while listening.
    Level { level: f32 },
    /// Partial or final transcript for the current dictation session.
    Transcript {
        text: String,
        #[serde(rename = "isFinal")]
        is_final: bool,
    },
    /// A recoverable error (missing bundle, no on-device model, audio device failure).
    Error { message: String },
}

/// The dictation engine's state as reported to the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum State {
    /// Not recording; the audio engine is released.
    Idle,
    /// Checking or requesting permissions and starting the audio engine.
    Starting,
    /// Actively capturing audio and streaming it to on-device recognition.
    Listening,
    /// Audio capture has stopped; waiting briefly for the final transcript.
    Stopping,
    /// Microphone or speech recognition permission was denied by the user.
    Denied,
    /// Speech recognition is restricted by a system profile or Screen Time.
    Restricted,
}

/// Options accepted from the page when starting dictation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct StartOptions {
    /// Domain vocabulary hints (`contextualStrings`): open file name, symbols in view, node
    /// labels. Capped before being passed to Speech.
    pub(crate) context: Vec<String>,
}

/// Maximum number of contextual hint strings passed to `SFSpeechRecognitionRequest`.
const MAX_CONTEXT_STRINGS: usize = 100;
/// Maximum byte length of a single contextual hint string.
const MAX_CONTEXT_LEN: usize = 128;

impl StartOptions {
    /// Parses the optional JSON suffix after `dictation:start:`.
    pub(crate) fn parse_json(json: &str) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default)]
            context: Vec<String>,
        }
        let raw: Raw = serde_json::from_str(json).ok()?;
        let context = raw
            .context
            .into_iter()
            .map(|item| item.trim().to_owned())
            .filter(|item| !item.is_empty() && item.len() <= MAX_CONTEXT_LEN)
            .take(MAX_CONTEXT_STRINGS)
            .collect();
        Some(Self { context })
    }
}

/// Internal callbacks marshaled from Speech/AVAudio/timer threads back to the main event loop,
/// so all Objective-C engine/task state stays single-threaded on the main thread.
#[derive(Debug)]
pub(crate) enum Internal {
    /// Microphone permission prompt completed.
    #[cfg(target_os = "macos")]
    MicPermission { session: u64, granted: bool },
    /// Speech recognition permission prompt completed.
    #[cfg(target_os = "macos")]
    SpeechPermission { session: u64, status: isize },
    /// A recognition callback delivered text, finality, or an error.
    #[cfg(target_os = "macos")]
    Recognition {
        session: u64,
        task: u64,
        text: Option<String>,
        is_final: bool,
        error: Option<String>,
    },
    /// `AVAudioEngineConfigurationChangeNotification` fired while listening.
    #[cfg(target_os = "macos")]
    ConfigChanged { session: u64 },
    /// Periodic rollover check triggered from the audio tap.
    #[cfg(target_os = "macos")]
    RolloverTick { session: u64 },
    /// Graceful stop deadline expired before Speech delivered a final result.
    #[cfg(target_os = "macos")]
    StopTimeout { session: u64 },
}

/// Callback sinks used by [`Dictation`] to post events to the page and internal completions
/// back to the main event loop.
pub(crate) struct Callbacks {
    pub(crate) on_event: Arc<dyn Fn(Event) + Send + Sync>,
    pub(crate) on_internal: Arc<dyn Fn(Internal) + Send + Sync>,
}

impl fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Callbacks").finish_non_exhaustive()
    }
}

// ---- Pure audio level and rollover logic ---------------------------------------------------

/// Hard limit before Apple's ~60 s per-task recognition cutoff.
pub(crate) const HARD_ROLLOVER: Duration = Duration::from_secs(55);
/// Soft limit after which even a brief pause triggers a fresh task.
pub(crate) const SOFT_ROLLOVER: Duration = Duration::from_secs(40);
/// Brief pause sufficient to roll over once past [`SOFT_ROLLOVER`].
pub(crate) const SOFT_PAUSE: Duration = Duration::from_millis(600);
/// Minimum task age before a natural pause rolls over early.
pub(crate) const MIN_PAUSE_AGE: Duration = Duration::from_secs(10);
/// Natural pause duration that triggers a fresh task after [`MIN_PAUSE_AGE`].
pub(crate) const NATURAL_PAUSE: Duration = Duration::from_secs(2);
/// Maximum trailing/leading words inspected when deduplicating a rollover seam.
const MAX_SEAM_WORDS: usize = 6;

/// Computes a normalized RMS level in `0.0..=1.0` from mono float32 PCM samples.
///
/// Non-finite samples are ignored so malformed buffers cannot produce `NaN`.
pub(crate) fn level(samples: &[f32]) -> f32 {
    level_strided(samples, 1)
}

/// Computes a normalized RMS level in `0.0..=1.0` stepping through `samples` by `stride`
/// (1 for deinterleaved mono, channel count for interleaved buffers).
pub(crate) fn level_strided(samples: &[f32], stride: usize) -> f32 {
    if stride == 0 {
        return 0.0;
    }
    let mut sum_sq: f64 = 0.0;
    let mut count: usize = 0;
    for &sample in samples.iter().step_by(stride) {
        if sample.is_finite() {
            let clamped = f64::from(sample.clamp(-1.0, 1.0));
            sum_sq += clamped * clamped;
            count += 1;
        }
    }
    if count == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "audio buffer frame counts are a few thousand samples, exact in f64"
    )]
    let rms = (sum_sq / count as f64).sqrt();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "rms is clamped to 0.0..=1.0, so f64 -> f32 is lossless in range"
    )]
    (rms.clamp(0.0, 1.0) as f32)
}

/// Tracks multi-task dictation text across recognition rollovers and decides when to start a
/// fresh recognition task.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Rollover {
    /// Finalized text accumulated from previous tasks in this session.
    committed: String,
    /// Latest partial transcript from the active task.
    current: String,
}

impl Rollover {
    /// Creates an empty rollover accumulator for a new dictation session.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns whether the active recognition task should roll over to a fresh task given how
    /// long the task has been running (`task_age`) and how long speech has been silent or
    /// unchanged (`silence_age`).
    pub(crate) fn should_rollover(&self, task_age: Duration, silence_age: Duration) -> bool {
        if task_age >= HARD_ROLLOVER {
            return true;
        }
        if task_age >= SOFT_ROLLOVER && silence_age >= SOFT_PAUSE {
            return true;
        }
        task_age >= MIN_PAUSE_AGE && silence_age >= NATURAL_PAUSE && !self.current.trim().is_empty()
    }

    /// Records a partial transcript from the active task and returns the full session text.
    pub(crate) fn on_partial(&mut self, text: &str) -> String {
        text.trim().clone_into(&mut self.current);
        join_seam(&self.committed, &self.current)
    }

    /// Records a final transcript from the active task, commits it, and returns the full
    /// session text.
    pub(crate) fn on_final(&mut self, text: &str) -> String {
        let joined = join_seam(&self.committed, text);
        self.committed.clone_from(&joined);
        self.current.clear();
        joined
    }

    /// Commits the current partial before switching to a fresh recognition task.
    pub(crate) fn begin_rollover(&mut self) {
        if !self.current.is_empty() {
            self.committed = join_seam(&self.committed, &self.current);
            self.current.clear();
        }
    }

    /// Returns the combined text currently held (committed plus any uncommitted partial).
    pub(crate) fn text(&self) -> String {
        join_seam(&self.committed, &self.current)
    }
}

/// Joins `committed` text from an earlier task with `next` text from a new task, removing
/// duplicated words spoken across the rollover seam.
pub(crate) fn join_seam(committed: &str, next: &str) -> String {
    let committed = committed.trim();
    let next = next.trim();
    if committed.is_empty() {
        return next.to_owned();
    }
    if next.is_empty() {
        return committed.to_owned();
    }

    let left: Vec<&str> = committed.split_whitespace().collect();
    let right: Vec<&str> = next.split_whitespace().collect();
    let max_overlap = left.len().min(right.len()).min(MAX_SEAM_WORDS);

    let mut overlap = 0;
    for width in (1..=max_overlap).rev() {
        let left_tail = &left[left.len() - width..];
        let right_head = &right[..width];
        if left_tail.iter().zip(right_head.iter()).all(|(a, b)| {
            let na = normalize_word(a);
            !na.is_empty() && na == normalize_word(b)
        }) {
            overlap = width;
            break;
        }
    }

    let tail = &right[overlap..];
    if tail.is_empty() {
        committed.to_owned()
    } else {
        let mut out = String::with_capacity(committed.len() + 1 + next.len());
        out.push_str(committed);
        out.push(' ');
        out.push_str(&tail.join(" "));
        out
    }
}

/// Normalizes a single whitespace-delimited token for seam comparison: strips surrounding
/// non-alphanumeric punctuation (including Unicode smart quotes/dashes) and lowercases.
fn normalize_word(word: &str) -> String {
    word.trim_matches(|ch: char| !ch.is_alphanumeric())
        .to_lowercase()
}

// ---- Public preflight check (used by tests/cli_errors.rs) ----------------------------------

/// Verifies that this process is running inside a macOS app bundle with both privacy usage
/// descriptions (`NSMicrophoneUsageDescription` and `NSSpeechRecognitionUsageDescription`).
///
/// Calling macOS audio or speech APIs from a bare binary without an `Info.plist` causes TCC to
/// terminate the process immediately. Checking first lets Postil refuse cleanly with a message.
///
/// # Errors
///
/// Returns a human-readable explanation when the process is not running from `Postil.app` (or
/// off macOS).
pub fn preflight_bundle() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::check_bundle()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("Dictation is only available on macOS.".to_owned())
    }
}

// ---- Controller ----------------------------------------------------------------------------

/// Native dictation controller owned by the window's event loop (`shell.rs`).
pub(crate) struct Dictation {
    #[cfg(target_os = "macos")]
    inner: macos::Controller,
    #[cfg(not(target_os = "macos"))]
    callbacks: Callbacks,
}

impl fmt::Debug for Dictation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dictation").finish_non_exhaustive()
    }
}

impl Dictation {
    /// Creates an idle dictation controller that reports events through `callbacks`.
    pub(crate) fn new(callbacks: Callbacks) -> Self {
        Self {
            #[cfg(target_os = "macos")]
            inner: macos::Controller::new(callbacks),
            #[cfg(not(target_os = "macos"))]
            callbacks,
        }
    }

    /// Starts dictation with optional vocabulary hints. Ignored if already active.
    pub(crate) fn start(&mut self, options: StartOptions) {
        #[cfg(target_os = "macos")]
        self.inner.start(options);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = options;
            (self.callbacks.on_event)(Event::Error {
                message: "Dictation is only available on macOS.".to_owned(),
            });
        }
    }

    /// Stops recording gracefully: releases the microphone immediately, finishes the active
    /// recognition task, delivers any pending final transcript, then transitions to `Idle`.
    pub(crate) fn stop(&mut self) {
        #[cfg(target_os = "macos")]
        self.inner.stop();
    }

    /// Handles the app window losing focus: stops only if actively recording (`Listening`), so
    /// macOS TCC permission alerts shown during `Starting` do not abort the start flow.
    pub(crate) fn on_focus_lost(&mut self) {
        #[cfg(target_os = "macos")]
        self.inner.on_focus_lost();
    }

    /// Cancels recording immediately, discarding any pending partial and releasing the engine.
    pub(crate) fn cancel(&mut self) {
        #[cfg(target_os = "macos")]
        self.inner.cancel();
    }

    /// Handles an internal completion marshaled back to the main event loop.
    pub(crate) fn handle_internal(&mut self, msg: Internal) {
        #[cfg(target_os = "macos")]
        self.inner.handle_internal(msg);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = msg;
        }
    }
}

// ---- macOS Speech + AVFAudio implementation ------------------------------------------------

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "Apple Speech and AVFAudio frameworks via objc2-speech, objc2-avf-audio, and block2; \
              allowed by the owner on 2026-11-25 (docs/review-assist.md, Dictation)"
)]
mod macos {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool, ProtocolObject};
    use objc2_avf_audio::{
        AVAudioApplication, AVAudioApplicationRecordPermission, AVAudioEngine,
        AVAudioEngineConfigurationChangeNotification, AVAudioNode, AVAudioPCMBuffer, AVAudioTime,
    };
    use objc2_foundation::{
        NSArray, NSBundle, NSError, NSNotification, NSNotificationCenter, NSString,
    };
    use objc2_speech::{
        SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
        SFSpeechRecognitionTaskHint, SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
    };

    use super::{Callbacks, Event, Internal, Rollover, StartOptions, State, level, level_strided};

    /// Minimum interval between `Event::Level` updates (~25 Hz).
    const LEVEL_INTERVAL_MS: u64 = 40;
    /// Interval between rollover tick checks sent from the audio tap.
    const ROLLOVER_TICK_MS: u64 = 500;
    /// Maximum wait for Speech to deliver a final transcript after `stop()`.
    const STOP_TIMEOUT: Duration = Duration::from_millis(1500);
    /// RMS threshold above which audio counts as active speech rather than silence.
    const SPEECH_THRESHOLD: f32 = 0.015;
    /// Buffer frame size requested from `AVAudioInputNode::installTapOnBus`.
    const TAP_BUFFER_FRAMES: u32 = 1024;

    /// Wrapper allowing `SFSpeechAudioBufferRecognitionRequest` to be fed from `CoreAudio`'s tap
    /// thread. Apple documents `appendAudioPCMBuffer:` as thread-safe and intended to be called
    /// directly from the `AVAudioEngine` input tap block.
    struct SendRequest(Retained<SFSpeechAudioBufferRecognitionRequest>);

    // SAFETY: `SFSpeechAudioBufferRecognitionRequest` is documented by Apple to accept
    // `appendAudioPCMBuffer:` calls from the real-time audio tap thread while controlled from
    // the main thread. `ActiveSession` keeps its own retained clone on the main thread, and
    // Objective-C retain/release is atomic; the main thread only calls `endAudio()` after
    // `TapShared::swap_request` acquires the mutex and removes the tap's reference, establishing
    // a happens-after edge so `endAudio()` never races with `appendAudioPCMBuffer:`.
    unsafe impl Send for SendRequest {}

    /// Shared handle between the main thread and the audio tap closure.
    struct TapShared {
        request: Mutex<Option<SendRequest>>,
        started_at: Instant,
        last_level_ms: AtomicU64,
        last_speech_ms: AtomicU64,
        last_tick_ms: AtomicU64,
        stopped: AtomicBool,
    }

    impl TapShared {
        fn new(request: Retained<SFSpeechAudioBufferRecognitionRequest>) -> Self {
            Self {
                request: Mutex::new(Some(SendRequest(request))),
                started_at: Instant::now(),
                last_level_ms: AtomicU64::new(0),
                last_speech_ms: AtomicU64::new(0),
                last_tick_ms: AtomicU64::new(0),
                stopped: AtomicBool::new(false),
            }
        }

        fn elapsed_ms(&self) -> u64 {
            u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
        }

        fn silence_age(&self) -> Duration {
            let now = self.elapsed_ms();
            let last = self.last_speech_ms.load(Ordering::Relaxed);
            Duration::from_millis(now.saturating_sub(last))
        }

        fn swap_request(&self, next: Option<Retained<SFSpeechAudioBufferRecognitionRequest>>) {
            if let Ok(mut guard) = self.request.lock() {
                *guard = next.map(SendRequest);
            }
        }
    }

    /// Active audio + speech session, held strictly on the main thread.
    struct ActiveSession {
        engine: Retained<AVAudioEngine>,
        recognizer: Retained<SFSpeechRecognizer>,
        request: Retained<SFSpeechAudioBufferRecognitionRequest>,
        task: Retained<SFSpeechRecognitionTask>,
        config_observer: Retained<ProtocolObject<dyn objc2::runtime::NSObjectProtocol>>,
        tap: Arc<TapShared>,
        context: Vec<String>,
        task_id: u64,
        task_started: Instant,
        rollover: Rollover,
        tap_installed: bool,
    }

    pub(super) struct Controller {
        callbacks: Callbacks,
        state: State,
        session_id: u64,
        pending_options: StartOptions,
        active: Option<ActiveSession>,
    }

    impl Controller {
        pub(super) fn new(callbacks: Callbacks) -> Self {
            Self {
                callbacks,
                state: State::Idle,
                session_id: 0,
                pending_options: StartOptions::default(),
                active: None,
            }
        }

        pub(super) fn start(&mut self, options: StartOptions) {
            // A second start while already starting, listening, or stopping is ignored.
            if matches!(
                self.state,
                State::Starting | State::Listening | State::Stopping
            ) {
                return;
            }
            if let Err(message) = check_bundle() {
                self.emit(Event::Error { message });
                return;
            }
            self.session_id = self.session_id.wrapping_add(1);
            self.pending_options = options;
            self.set_state(State::Starting, None);
            self.check_mic_permission();
        }

        pub(super) fn stop(&mut self) {
            match self.state {
                State::Starting => {
                    self.session_id = self.session_id.wrapping_add(1);
                    self.set_state(State::Idle, None);
                }
                State::Listening => {
                    let Some(active) = &mut self.active else {
                        self.set_state(State::Idle, None);
                        return;
                    };
                    stop_audio_capture(active);
                    // SAFETY: `endAudio` is called on a valid `SFSpeechAudioBufferRecognitionRequest`
                    // after the audio tap has been removed so no further buffers are appended.
                    unsafe {
                        active.request.endAudio();
                    }
                    // SAFETY: `finish` asks the live `SFSpeechRecognitionTask` to deliver its
                    // final result before ending.
                    unsafe {
                        active.task.finish();
                    }
                    self.set_state(State::Stopping, None);

                    let session = self.session_id;
                    let on_internal = Arc::clone(&self.callbacks.on_internal);
                    let spawned = thread::Builder::new()
                        .name("dictation-stop".to_owned())
                        .spawn(move || {
                            thread::sleep(STOP_TIMEOUT);
                            on_internal(Internal::StopTimeout { session });
                        });
                    drop(spawned);
                }
                State::Idle | State::Stopping | State::Denied | State::Restricted => {}
            }
        }

        pub(super) fn on_focus_lost(&mut self) {
            // Only stop when actively capturing audio; macOS TCC permission dialogs steal
            // window focus during `State::Starting` and must not abort the permission flow.
            if self.state == State::Listening {
                self.stop();
            }
        }

        pub(super) fn cancel(&mut self) {
            if matches!(self.state, State::Idle | State::Denied | State::Restricted) {
                return;
            }
            self.session_id = self.session_id.wrapping_add(1);
            if let Some(mut active) = self.active.take() {
                teardown_session(&mut active);
            }
            self.set_state(State::Idle, None);
        }

        pub(super) fn handle_internal(&mut self, msg: Internal) {
            match msg {
                Internal::MicPermission { session, granted } => {
                    if session != self.session_id || self.state != State::Starting {
                        return;
                    }
                    if granted {
                        self.check_speech_permission();
                    } else {
                        self.set_state(State::Denied, Some(MIC_DENIED_MSG.to_owned()));
                    }
                }
                Internal::SpeechPermission { session, status } => {
                    if session != self.session_id || self.state != State::Starting {
                        return;
                    }
                    self.on_speech_status(SFSpeechRecognizerAuthorizationStatus(status));
                }
                Internal::Recognition {
                    session,
                    task,
                    text,
                    is_final,
                    error,
                } => {
                    if session != self.session_id {
                        return;
                    }
                    self.on_recognition(task, text, is_final, error);
                }
                Internal::ConfigChanged { session } => {
                    if session != self.session_id || self.state != State::Listening {
                        return;
                    }
                    self.on_config_changed();
                }
                Internal::RolloverTick { session } => {
                    if session != self.session_id || self.state != State::Listening {
                        return;
                    }
                    self.maybe_rollover();
                }
                Internal::StopTimeout { session } => {
                    if session != self.session_id || self.state != State::Stopping {
                        return;
                    }
                    if let Some(mut active) = self.active.take() {
                        let text = active.rollover.text();
                        teardown_session(&mut active);
                        if !text.is_empty() {
                            self.emit(Event::Transcript {
                                text,
                                is_final: true,
                            });
                        }
                    }
                    self.set_state(State::Idle, None);
                }
            }
        }

        fn emit(&self, event: Event) {
            (self.callbacks.on_event)(event);
        }

        fn set_state(&mut self, state: State, message: Option<String>) {
            self.state = state;
            self.emit(Event::State { state, message });
        }

        fn check_mic_permission(&mut self) {
            // SAFETY: `AVAudioApplication::sharedInstance` returns the process singleton.
            let app = unsafe { AVAudioApplication::sharedInstance() };
            // SAFETY: `recordPermission` is a read-only getter on the singleton.
            let perm = unsafe { app.recordPermission() };
            if perm == AVAudioApplicationRecordPermission::Granted {
                self.check_speech_permission();
            } else if perm == AVAudioApplicationRecordPermission::Denied {
                self.set_state(State::Denied, Some(MIC_DENIED_MSG.to_owned()));
            } else {
                let session = self.session_id;
                let on_internal = Arc::clone(&self.callbacks.on_internal);
                let handler = RcBlock::new(move |granted: Bool| {
                    on_internal(Internal::MicPermission {
                        session,
                        granted: granted.as_bool(),
                    });
                });
                // SAFETY: `handler` is a valid heap-allocated `RcBlock` matching the Objective-C
                // completion signature; `AVAudioApplication` retains it for the prompt's lifetime.
                unsafe {
                    AVAudioApplication::requestRecordPermissionWithCompletionHandler(&handler);
                }
            }
        }

        fn check_speech_permission(&mut self) {
            // SAFETY: `authorizationStatus` is a stateless class method on `SFSpeechRecognizer`.
            let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
            if status == SFSpeechRecognizerAuthorizationStatus::NotDetermined {
                let session = self.session_id;
                let on_internal = Arc::clone(&self.callbacks.on_internal);
                let handler =
                    RcBlock::new(move |new_status: SFSpeechRecognizerAuthorizationStatus| {
                        on_internal(Internal::SpeechPermission {
                            session,
                            status: new_status.0,
                        });
                    });
                // SAFETY: `handler` is a valid `RcBlock` retained by the Speech framework until
                // the authorization dialog finishes.
                unsafe {
                    SFSpeechRecognizer::requestAuthorization(&handler);
                }
            } else {
                self.on_speech_status(status);
            }
        }

        fn on_speech_status(&mut self, status: SFSpeechRecognizerAuthorizationStatus) {
            if status == SFSpeechRecognizerAuthorizationStatus::Authorized {
                self.begin_listening();
            } else if status == SFSpeechRecognizerAuthorizationStatus::Denied {
                self.set_state(State::Denied, Some(SPEECH_DENIED_MSG.to_owned()));
            } else if status == SFSpeechRecognizerAuthorizationStatus::Restricted {
                self.set_state(State::Restricted, Some(SPEECH_RESTRICTED_MSG.to_owned()));
            } else {
                self.set_state(State::Idle, None);
            }
        }

        fn begin_listening(&mut self) {
            let alloc = SFSpeechRecognizer::alloc();
            // SAFETY: `SFSpeechRecognizer::init` is the failable designated initializer for the
            // system locale; returning `None` on an unsupported locale avoids `new()` panicking.
            let Some(recognizer) = (unsafe { SFSpeechRecognizer::init(alloc) }) else {
                self.emit(Event::Error {
                    message: NO_ON_DEVICE_MSG.to_owned(),
                });
                self.set_state(State::Idle, None);
                return;
            };
            // SAFETY: `supportsOnDeviceRecognition` is a read-only property on the recognizer.
            let on_device = unsafe { recognizer.supportsOnDeviceRecognition() };
            if !on_device {
                self.emit(Event::Error {
                    message: NO_ON_DEVICE_MSG.to_owned(),
                });
                self.set_state(State::Idle, None);
                return;
            }
            // SAFETY: `isAvailable` is a read-only property on the recognizer.
            let available = unsafe { recognizer.isAvailable() };
            if !available {
                self.emit(Event::Error {
                    message: "Speech recognition is temporarily unavailable.".to_owned(),
                });
                self.set_state(State::Idle, None);
                return;
            }

            let context = self.pending_options.context.clone();
            let request = build_request(&context);
            let task_id = 1;
            let task = start_task(
                &recognizer,
                &request,
                self.session_id,
                task_id,
                Arc::clone(&self.callbacks.on_internal),
            );

            // SAFETY: `AVAudioEngine::new` creates a fresh audio engine on the main thread.
            let engine = unsafe { AVAudioEngine::new() };
            let tap = Arc::new(TapShared::new(Retained::clone(&request)));

            if let Err(message) = install_tap_and_start(
                &engine,
                Arc::clone(&tap),
                self.session_id,
                Arc::clone(&self.callbacks.on_event),
                Arc::clone(&self.callbacks.on_internal),
            ) {
                // SAFETY: `cancel` aborts the task we just created because audio failed to start.
                unsafe {
                    task.cancel();
                }
                self.emit(Event::Error { message });
                self.set_state(State::Idle, None);
                return;
            }

            let config_observer = observe_config_change(
                &engine,
                self.session_id,
                Arc::clone(&self.callbacks.on_internal),
            );

            self.active = Some(ActiveSession {
                engine,
                recognizer,
                request,
                task,
                config_observer,
                tap,
                context,
                task_id,
                task_started: Instant::now(),
                rollover: Rollover::new(),
                tap_installed: true,
            });
            self.set_state(State::Listening, None);
        }

        fn on_recognition(
            &mut self,
            task_id: u64,
            text: Option<String>,
            is_final: bool,
            error: Option<String>,
        ) {
            let Some(active) = &mut self.active else {
                return;
            };
            if task_id != active.task_id {
                return;
            }

            let mut emitted_final = false;
            if let Some(text) = text {
                let combined = if is_final {
                    active.rollover.on_final(&text)
                } else {
                    active.rollover.on_partial(&text)
                };
                if !combined.is_empty() {
                    let final_flag = is_final && self.state == State::Stopping;
                    emitted_final = final_flag;
                    self.emit(Event::Transcript {
                        text: combined,
                        is_final: final_flag,
                    });
                }
            }

            if self.state == State::Stopping {
                if is_final || error.is_some() {
                    if let Some(mut active) = self.active.take() {
                        if !emitted_final {
                            let flushed = active.rollover.text();
                            if !flushed.is_empty() {
                                self.emit(Event::Transcript {
                                    text: flushed,
                                    is_final: true,
                                });
                            }
                        }
                        teardown_session(&mut active);
                    }
                    self.set_state(State::Idle, None);
                }
                return;
            }

            if is_final {
                // Speech ended the task on its own (e.g. long silence): start a fresh task so
                // continuous dictation keeps going until the user stops it.
                self.rollover_task();
            } else if let Some(message) = error {
                if let Some(mut active) = self.active.take() {
                    teardown_session(&mut active);
                }
                self.emit(Event::Error { message });
                self.set_state(State::Idle, None);
            }
        }

        fn maybe_rollover(&mut self) {
            let Some(active) = &self.active else {
                return;
            };
            let task_age = active.task_started.elapsed();
            let silence_age = active.tap.silence_age();
            if active.rollover.should_rollover(task_age, silence_age) {
                self.rollover_task();
            }
        }

        fn rollover_task(&mut self) {
            let Some(active) = &mut self.active else {
                return;
            };
            active.rollover.begin_rollover();
            let next_request = build_request(&active.context);
            let next_task_id = active.task_id.wrapping_add(1);
            let next_task = start_task(
                &active.recognizer,
                &next_request,
                self.session_id,
                next_task_id,
                Arc::clone(&self.callbacks.on_internal),
            );
            active
                .tap
                .swap_request(Some(Retained::clone(&next_request)));

            let old_request = std::mem::replace(&mut active.request, next_request);
            let old_task = std::mem::replace(&mut active.task, next_task);
            active.task_id = next_task_id;
            active.task_started = Instant::now();

            // SAFETY: `old_request` is no longer referenced by the audio tap after `swap_request`.
            unsafe {
                old_request.endAudio();
            }
            // SAFETY: `old_task` is superseded; cancelling releases its resources cleanly and its
            // stale callbacks are ignored by `task_id`.
            unsafe {
                old_task.cancel();
            }
        }

        fn on_config_changed(&mut self) {
            let Some(active) = &mut self.active else {
                return;
            };
            stop_audio_capture(active);
            active.tap.stopped.store(false, Ordering::Release);
            if let Err(message) = install_tap_and_start(
                &active.engine,
                Arc::clone(&active.tap),
                self.session_id,
                Arc::clone(&self.callbacks.on_event),
                Arc::clone(&self.callbacks.on_internal),
            ) {
                if let Some(mut active) = self.active.take() {
                    teardown_session(&mut active);
                }
                self.emit(Event::Error {
                    message: format!("Audio input changed and could not be restarted: {message}"),
                });
                self.set_state(State::Idle, None);
            } else {
                active.tap_installed = true;
                // Start a fresh recognition request/task for the new hardware input format and
                // install it into `active.tap` (which `stop_audio_capture` cleared to `None`).
                self.rollover_task();
            }
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            if let Some(mut active) = self.active.take() {
                teardown_session(&mut active);
            }
        }
    }

    const MIC_DENIED_MSG: &str = "Microphone access is denied. Enable Postil in System Settings > Privacy & Security > Microphone.";
    const SPEECH_DENIED_MSG: &str = "Speech recognition is denied. Enable Postil in System Settings > Privacy & Security > Speech Recognition.";
    const SPEECH_RESTRICTED_MSG: &str = "Speech recognition is restricted on this Mac.";
    const NO_ON_DEVICE_MSG: &str = "On-device speech recognition is not available for the system language. Download it in System Settings > Keyboard > Dictation.";

    /// Checks `NSBundle::mainBundle()` for a bundle identifier and both privacy usage
    /// descriptions so a bare binary (`cargo run`) refuses dictation instead of being killed by
    /// macOS TCC.
    pub(super) fn check_bundle() -> Result<(), String> {
        let bundle = NSBundle::mainBundle();
        if bundle.bundleIdentifier().is_none() {
            return Err(
                "Dictation requires Postil.app (a bare binary has no Info.plist; run scripts/bundle.sh)."
                    .to_owned(),
            );
        }
        let mic_key = NSString::from_str("NSMicrophoneUsageDescription");
        let speech_key = NSString::from_str("NSSpeechRecognitionUsageDescription");
        if bundle.objectForInfoDictionaryKey(&mic_key).is_none()
            || bundle.objectForInfoDictionaryKey(&speech_key).is_none()
        {
            return Err(
                "Postil.app is missing its microphone or speech usage descriptions in Info.plist."
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn build_request(context: &[String]) -> Retained<SFSpeechAudioBufferRecognitionRequest> {
        // SAFETY: `SFSpeechAudioBufferRecognitionRequest::new` allocates a fresh request.
        let request = unsafe { SFSpeechAudioBufferRecognitionRequest::new() };
        // SAFETY: Setting `requiresOnDeviceRecognition` forces local recognition; the network is
        // never used.
        unsafe {
            request.setRequiresOnDeviceRecognition(true);
        }
        // SAFETY: Partial results let the widget show words as they are spoken.
        unsafe {
            request.setShouldReportPartialResults(true);
        }
        // SAFETY: `Dictation` hint tunes punctuation and phrasing for free-form text.
        unsafe {
            request.setTaskHint(SFSpeechRecognitionTaskHint::Dictation);
        }
        if !context.is_empty() {
            let strings: Vec<Retained<NSString>> =
                context.iter().map(|s| NSString::from_str(s)).collect();
            let array = NSArray::from_retained_slice(&strings);
            // SAFETY: `array` is a valid `NSArray<NSString>` copied by the request.
            unsafe {
                request.setContextualStrings(&array);
            }
        }
        request
    }

    fn start_task(
        recognizer: &SFSpeechRecognizer,
        request: &SFSpeechAudioBufferRecognitionRequest,
        session: u64,
        task_id: u64,
        on_internal: Arc<dyn Fn(Internal) + Send + Sync>,
    ) -> Retained<SFSpeechRecognitionTask> {
        let handler = RcBlock::new(
            move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
                let (text, is_final) = if let Some(result_ref) = NonNull::new(result) {
                    // SAFETY: `result` is non-null and valid for the duration of the Speech
                    // framework callback.
                    let result_obj = unsafe { result_ref.as_ref() };
                    // SAFETY: `bestTranscription` returns a retained `SFTranscription`.
                    let transcription = unsafe { result_obj.bestTranscription() };
                    // SAFETY: `formattedString` returns a retained `NSString`.
                    let formatted = unsafe { transcription.formattedString() };
                    // SAFETY: `isFinal` is a read-only boolean property.
                    let final_flag = unsafe { result_obj.isFinal() };
                    (Some(formatted.to_string()), final_flag)
                } else {
                    (None, false)
                };
                let error_text = if let Some(err_ref) = NonNull::new(error) {
                    // SAFETY: `error` is non-null and valid for the duration of the callback.
                    let err_obj = unsafe { err_ref.as_ref() };
                    Some(err_obj.localizedDescription().to_string())
                } else {
                    None
                };
                on_internal(Internal::Recognition {
                    session,
                    task: task_id,
                    text,
                    is_final,
                    error: error_text,
                });
            },
        );
        // SAFETY: `recognizer`, `request`, and `handler` are valid Objective-C objects; the
        // framework retains the block for the task's lifetime.
        unsafe { recognizer.recognitionTaskWithRequest_resultHandler(request, &handler) }
    }

    fn install_tap_and_start(
        engine: &AVAudioEngine,
        tap: Arc<TapShared>,
        session: u64,
        on_event: Arc<dyn Fn(Event) + Send + Sync>,
        on_internal: Arc<dyn Fn(Internal) + Send + Sync>,
    ) -> Result<(), String> {
        // SAFETY: `inputNode` returns the engine's input node singleton.
        let input = unsafe { engine.inputNode() };
        let node: &AVAudioNode = &input;
        // SAFETY: Bus 0 is the standard input bus on `AVAudioInputNode`.
        let format = unsafe { node.outputFormatForBus(0) };
        // SAFETY: `channelCount` is a read-only property on `AVAudioFormat`.
        let channels = unsafe { format.channelCount() };
        // SAFETY: `sampleRate` is a read-only property on `AVAudioFormat`.
        let sample_rate = unsafe { format.sampleRate() };
        if channels == 0 || sample_rate <= 0.0 {
            return Err("No active microphone input was found.".to_owned());
        }

        let block = RcBlock::new(
            move |buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
                if tap.stopped.load(Ordering::Acquire) {
                    return;
                }
                // SAFETY: CoreAudio passes a valid `AVAudioPCMBuffer` for the duration of the tap
                // callback.
                let pcm = unsafe { buffer.as_ref() };
                if let Ok(guard) = tap.request.try_lock()
                    && let Some(req) = guard.as_ref()
                {
                    // SAFETY: `appendAudioPCMBuffer` is thread-safe and copies the PCM buffer.
                    unsafe {
                        req.0.appendAudioPCMBuffer(pcm);
                    }
                }

                let rms = pcm_level(pcm);
                let now_ms = tap.elapsed_ms();
                if rms >= SPEECH_THRESHOLD {
                    tap.last_speech_ms.store(now_ms, Ordering::Relaxed);
                }
                let prev_level = tap.last_level_ms.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev_level) >= LEVEL_INTERVAL_MS
                    && tap
                        .last_level_ms
                        .compare_exchange(prev_level, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    on_event(Event::Level { level: rms });
                }
                let prev_tick = tap.last_tick_ms.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev_tick) >= ROLLOVER_TICK_MS
                    && tap
                        .last_tick_ms
                        .compare_exchange(prev_tick, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    on_internal(Internal::RolloverTick { session });
                }
            },
        );
        let block_ptr = RcBlock::as_ptr(&block);
        // SAFETY: `installTapOnBus_bufferSize_format_block` copies the block onto the heap; the
        // tap is removed before the engine is dropped.
        unsafe {
            node.installTapOnBus_bufferSize_format_block(
                0,
                TAP_BUFFER_FRAMES,
                Some(&format),
                block_ptr,
            );
        }
        // SAFETY: `prepare` pre-allocates audio engine resources on the main thread.
        unsafe {
            engine.prepare();
        }
        // SAFETY: `startAndReturnError` starts audio capture and reports any OSStatus error.
        let started = unsafe { engine.startAndReturnError() };
        if let Err(error) = started {
            // SAFETY: Removing the tap cleans up after a failed start.
            unsafe {
                node.removeTapOnBus(0);
            }
            return Err(error.localizedDescription().to_string());
        }
        Ok(())
    }

    fn pcm_level(pcm: &AVAudioPCMBuffer) -> f32 {
        // SAFETY: `frameLength` is a read-only count of valid frames in the buffer.
        let frames = unsafe { pcm.frameLength() } as usize;
        if frames == 0 {
            return 0.0;
        }
        // SAFETY: `format` returns the buffer's `AVAudioFormat`.
        let format = unsafe { pcm.format() };
        // SAFETY: `channelCount` is the number of channel pointers in `floatChannelData`.
        let channels = unsafe { format.channelCount() };
        if channels == 0 {
            return 0.0;
        }
        // SAFETY: `floatChannelData` returns null when the buffer is not float32 PCM, or a
        // pointer to `channels` channel pointers each covering `frameLength` frames with `stride`.
        let channel_ptrs = unsafe { pcm.floatChannelData() };
        if channel_ptrs.is_null() {
            return 0.0;
        }
        // SAFETY: `stride` is the sample spacing for channel 0 (1 when deinterleaved).
        let stride = unsafe { pcm.stride() };
        let Some(total) = frames.checked_mul(stride) else {
            return 0.0;
        };
        if stride == 0 || total == 0 {
            return 0.0;
        }
        // SAFETY: `channel_ptrs` is non-null and `channels >= 1`, so index 0 is valid.
        let ch0 = unsafe { *channel_ptrs };
        if !ch0.as_ptr().is_aligned() {
            return 0.0;
        }
        // SAFETY: `ch0` is aligned and points to `frames * stride` contiguous `f32` samples valid
        // for this callback.
        let samples = unsafe { std::slice::from_raw_parts(ch0.as_ptr(), total) };
        if stride == 1 {
            level(samples)
        } else {
            level_strided(samples, stride)
        }
    }

    fn observe_config_change(
        engine: &AVAudioEngine,
        session: u64,
        on_internal: Arc<dyn Fn(Internal) + Send + Sync>,
    ) -> Retained<ProtocolObject<dyn objc2::runtime::NSObjectProtocol>> {
        let center = NSNotificationCenter::defaultCenter();
        let block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            on_internal(Internal::ConfigChanged { session });
        });
        let engine_obj: &AnyObject = engine;
        // SAFETY: `AVAudioEngineConfigurationChangeNotification` is an immutable framework static.
        let name = unsafe { AVAudioEngineConfigurationChangeNotification };
        // SAFETY: `addObserverForName_object_queue_usingBlock` retains `block` until the returned
        // observer token is removed via `removeObserver` in `teardown_session`.
        unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                Some(engine_obj),
                None,
                &block,
            )
        }
    }

    fn stop_audio_capture(active: &mut ActiveSession) {
        active.tap.stopped.store(true, Ordering::Release);
        active.tap.swap_request(None);
        // SAFETY: `stop` halts the audio engine on the main thread.
        unsafe {
            active.engine.stop();
        }
        if active.tap_installed {
            // SAFETY: `inputNode` returns the engine's input node.
            let input = unsafe { active.engine.inputNode() };
            let node: &AVAudioNode = &input;
            // SAFETY: Removing the tap on bus 0 balances `installTapOnBus_bufferSize_format_block`.
            unsafe {
                node.removeTapOnBus(0);
            }
            active.tap_installed = false;
        }
    }

    fn teardown_session(active: &mut ActiveSession) {
        stop_audio_capture(active);
        let center = NSNotificationCenter::defaultCenter();
        let observer: &AnyObject = active.config_observer.as_ref();
        // SAFETY: `config_observer` was returned by `addObserverForName_object_queue_usingBlock`
        // and has not been removed yet.
        unsafe {
            center.removeObserver(observer);
        }
        // SAFETY: `cancel` ends the recognition task and releases its handler block.
        unsafe {
            active.task.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_reports_silence_full_scale_and_ignores_non_finite() {
        assert_eq!(level(&[]), 0.0);
        assert_eq!(level(&[0.0; 256]), 0.0);
        assert!((level(&[1.0; 256]) - 1.0).abs() < 1e-6);
        assert!((level(&[-1.0; 256]) - 1.0).abs() < 1e-6);
        assert!((level(&[0.5; 64]) - 0.5).abs() < 1e-6);
        // Out-of-range and non-finite samples never produce NaN or exceed 1.0.
        assert_eq!(level(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY]), 0.0);
        assert!((level(&[f32::NAN, 2.0, -2.0]) - 1.0).abs() < 1e-6);
        // Strided (interleaved stereo) reads only channel 0.
        assert!((level_strided(&[0.5, 0.0, 0.5, 0.0], 2) - 0.5).abs() < 1e-6);
        assert_eq!(level_strided(&[1.0, 1.0], 0), 0.0);
    }

    #[test]
    fn rollover_triggers_on_a_pause_or_the_sixty_second_limit() {
        let mut rollover = Rollover::new();
        // Too early for a natural pause.
        assert!(!rollover.should_rollover(Duration::from_secs(5), Duration::from_secs(3)));
        // Natural pause with no spoken text yet does not churn tasks.
        assert!(!rollover.should_rollover(Duration::from_secs(15), Duration::from_secs(3)));
        // Natural pause after spoken text rolls over.
        rollover.on_partial("check the lock");
        assert!(rollover.should_rollover(Duration::from_secs(15), Duration::from_secs(2)));
        assert!(!rollover.should_rollover(Duration::from_secs(15), Duration::from_secs(1)));
        // Soft limit + brief pause rolls over.
        assert!(rollover.should_rollover(Duration::from_secs(40), Duration::from_millis(600)));
        assert!(!rollover.should_rollover(Duration::from_secs(40), Duration::from_millis(200)));
        // Hard limit rolls over even during continuous speech.
        assert!(rollover.should_rollover(Duration::from_secs(55), Duration::ZERO));
    }

    #[test]
    fn rollover_seam_deduplicates_words_spoken_across_tasks() {
        assert_eq!(
            join_seam("check the lock", "the lock before returning"),
            "check the lock before returning"
        );
        // Case and trailing punctuation across the seam.
        assert_eq!(
            join_seam("Hello, world.", "world next sentence"),
            "Hello, world. next sentence"
        );
        // Standalone punctuation tokens must not match each other as empty words.
        assert_eq!(join_seam("one -", ", two"), "one - , two");
        // Unicode smart punctuation and case across the seam.
        assert_eq!(join_seam("“Café”", "café au lait"), "“Café” au lait");
        // Disjoint tasks join with a single space.
        assert_eq!(
            join_seam("first part", "second part"),
            "first part second part"
        );
        // Complete duplicate suffix adds nothing.
        assert_eq!(join_seam("already said", "said"), "already said");
        // Empty sides.
        assert_eq!(join_seam("", "only new"), "only new");
        assert_eq!(join_seam("only old", ""), "only old");

        // Full multi-task lifecycle through Rollover.
        let mut rollover = Rollover::new();
        assert_eq!(rollover.on_partial("check the"), "check the");
        assert_eq!(rollover.on_partial("check the lock"), "check the lock");
        rollover.begin_rollover();
        assert_eq!(
            rollover.on_partial("the lock before returning"),
            "check the lock before returning"
        );
        assert_eq!(
            rollover.on_final("the lock before returning it"),
            "check the lock before returning it"
        );
        assert_eq!(rollover.text(), "check the lock before returning it");
    }

    #[test]
    fn start_options_filters_and_caps_contextual_strings() {
        let parsed = StartOptions::parse_json(r#"{"context":[" src/run.rs ", "", "Rollover"]}"#)
            .expect("valid json");
        assert_eq!(parsed.context, vec!["src/run.rs", "Rollover"]);
        assert!(StartOptions::parse_json("not json").is_none());
    }

    #[test]
    fn starting_dictation_in_a_bare_binary_emits_an_error_and_does_not_crash() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let mut dictation = Dictation::new(Callbacks {
            on_event: Arc::new(move |event| sink.lock().unwrap().push(event)),
            on_internal: Arc::new(|_| {}),
        });
        dictation.start(StartOptions::default());
        dictation.stop();
        dictation.cancel();

        let received = events.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert!(
            matches!(&received[0], Event::Error { message } if message.contains("Postil.app") || message.contains("macOS")),
            "{received:?}"
        );
    }
}
