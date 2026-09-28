/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Browser Worker media backend.
//!
//! Container parsing and codec implementations belong to the embedding
//! browser. This backend streams encoded response bytes to the Worker host,
//! which demuxes with Mediabunny and decodes with WebCodecs. Decoded frames
//! return through the existing Servo video and audio renderer traits.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use servo_base::generic_channel::GenericCallback;
use servo_media::audio::context::{AudioContext, AudioContextOptions};
use servo_media::audio::sink::AudioSinkError;
use servo_media::player::audio::AudioRenderer;
use servo_media::player::context::PlayerGLContext;
use servo_media::player::metadata::Metadata;
use servo_media::player::video::{Buffer, VideoFrame, VideoFrameData, VideoFrameRenderer};
use servo_media::player::{PlaybackState, Player, PlayerError, PlayerEvent, StreamType};
use servo_media::streams::capture::MediaTrackConstraintSet;
use servo_media::streams::device_monitor::MediaDeviceMonitor;
use servo_media::streams::registry::MediaStreamId;
use servo_media::streams::{MediaOutput, MediaSocket, MediaStreamType};
use servo_media::webrtc::{WebRtcController, WebRtcSignaller};
use servo_media::{
    Backend, BackendInit, ClientContextId, MediaInstance, MediaInstanceError, SupportsMediaType,
};
use servo_media_dummy::DummyBackend;

const MEDIA_CREATE: u32 = 0;
const MEDIA_SET_CONTENT_TYPE: u32 = 1;
const MEDIA_PUSH_DATA: u32 = 2;
const MEDIA_END_OF_STREAM: u32 = 3;
const MEDIA_PLAY: u32 = 4;
const MEDIA_PAUSE: u32 = 5;
const MEDIA_STOP: u32 = 6;
const MEDIA_SEEK: u32 = 7;
const MEDIA_SET_MUTED: u32 = 8;
const MEDIA_SET_VOLUME: u32 = 9;
const MEDIA_SET_RATE: u32 = 10;
const MEDIA_DESTROY: u32 = 11;
const MEDIA_SET_INPUT_SIZE: u32 = 12;
const MEDIA_SET_SEEKABLE: u32 = 13;
const MEDIA_SET_BUFFERING: u32 = 14;

const EVENT_METADATA: u32 = 0;
const EVENT_STATE: u32 = 1;
const EVENT_END_OF_STREAM: u32 = 2;
const EVENT_ENOUGH_DATA: u32 = 3;
const EVENT_NEED_DATA: u32 = 4;
const EVENT_POSITION: u32 = 5;
const EVENT_ERROR: u32 = 6;
const EVENT_DURATION: u32 = 7;

static NEXT_PLAYER_ID: AtomicUsize = AtomicUsize::new(1);
static PLAYER_CALLBACKS: OnceLock<Mutex<HashMap<usize, Weak<Mutex<PlayerCallbacks>>>>> =
    OnceLock::new();

struct PlayerCallbacks {
    sender: GenericCallback<PlayerEvent>,
    video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    player_state: Weak<Mutex<PlayerState>>,
}

fn callback_registry() -> &'static Mutex<HashMap<usize, Weak<Mutex<PlayerCallbacks>>>> {
    PLAYER_CALLBACKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn callbacks(id: usize) -> Option<Arc<Mutex<PlayerCallbacks>>> {
    let mut registry = callback_registry().lock().unwrap();
    let callback = registry.get(&id).and_then(Weak::upgrade);
    if callback.is_none() {
        registry.remove(&id);
    }
    callback
}

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// Copies a media command into the browser Worker host. The host must not
    /// call back into Servo from this synchronous import; it queues async work.
    fn worker_media_command(
        operation: u32,
        player_id: u32,
        value: f64,
        data: *const u8,
        data_len: usize,
    ) -> i32;
}

fn command(operation: u32, id: usize, value: f64, data: &[u8]) -> i32 {
    let Ok(id) = u32::try_from(id) else {
        return -1;
    };
    // SAFETY: the host import copies `data` before this call returns. It does
    // not retain a pointer into the WebAssembly memory.
    unsafe { worker_media_command(operation, id, value, data.as_ptr(), data.len()) }
}

pub struct WorkerMediaBackend;

impl BackendInit for WorkerMediaBackend {
    fn init() -> Box<dyn Backend> {
        Box::new(Self)
    }
}

impl Backend for WorkerMediaBackend {
    fn create_player(
        &self,
        _context_id: &ClientContextId,
        stream_type: StreamType,
        sender: GenericCallback<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        _gl_context: Box<dyn PlayerGLContext>,
    ) -> Arc<Mutex<dyn Player>> {
        let id = NEXT_PLAYER_ID.fetch_add(1, Ordering::Relaxed);
        let player_state = Arc::new(Mutex::new(PlayerState::default()));
        let callbacks = Arc::new(Mutex::new(PlayerCallbacks {
            sender,
            video_renderer,
            audio_renderer,
            player_state: Arc::downgrade(&player_state),
        }));
        callback_registry()
            .lock()
            .unwrap()
            .insert(id, Arc::downgrade(&callbacks));
        let player = Arc::new(Mutex::new(WorkerMediaPlayer {
            id,
            callbacks,
            state: player_state,
        }));
        // Bit 0 indicates video rendering; bit 1 indicates audio routed into
        // Servo's MediaElementAudioSourceNode instead of directly to the host.
        let callbacks = player.lock().unwrap().callbacks.clone();
        let flags = {
            let callbacks = callbacks.lock().unwrap();
            u32::from(callbacks.video_renderer.is_some())
                | (u32::from(callbacks.audio_renderer.is_some()) << 1)
                | (u32::from(stream_type == StreamType::Seekable) << 2)
        };
        if command(MEDIA_CREATE, id, f64::from(flags), &[]) != 0 {
            log::error!("The Worker host rejected media player creation");
        }
        player
    }

    fn create_audiostream(&self) -> MediaStreamId {
        <DummyBackend as Backend>::create_audiostream(&DummyBackend)
    }

    fn create_videostream(&self) -> MediaStreamId {
        <DummyBackend as Backend>::create_videostream(&DummyBackend)
    }

    fn create_stream_output(&self) -> Box<dyn MediaOutput> {
        <DummyBackend as Backend>::create_stream_output(&DummyBackend)
    }

    fn create_stream_and_socket(
        &self,
        ty: MediaStreamType,
    ) -> (Box<dyn MediaSocket>, MediaStreamId) {
        <DummyBackend as Backend>::create_stream_and_socket(&DummyBackend, ty)
    }

    fn create_audioinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        <DummyBackend as Backend>::create_audioinput_stream(&DummyBackend, set)
    }

    fn create_videoinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        <DummyBackend as Backend>::create_videoinput_stream(&DummyBackend, set)
    }

    fn create_audio_context(
        &self,
        id: &ClientContextId,
        options: AudioContextOptions,
    ) -> Result<Arc<Mutex<AudioContext>>, AudioSinkError> {
        <DummyBackend as Backend>::create_audio_context(&DummyBackend, id, options)
    }

    fn create_webrtc(&self, signaller: Box<dyn WebRtcSignaller>) -> WebRtcController {
        WebRtcController::new::<DummyBackend>(signaller)
    }

    fn can_play_type(&self, media_type: &str) -> SupportsMediaType {
        let mime = media_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if matches!(
            mime.as_str(),
            "audio/mp4"
                | "audio/mpeg"
                | "audio/ogg"
                | "audio/wav"
                | "audio/wave"
                | "audio/x-wav"
                | "audio/webm"
                | "audio/flac"
                | "audio/aac"
                | "video/mp4"
                | "video/webm"
                | "video/ogg"
                | "video/quicktime"
                | "video/x-matroska"
        ) {
            SupportsMediaType::Maybe
        } else {
            SupportsMediaType::No
        }
    }

    fn get_device_monitor(&self) -> Box<dyn MediaDeviceMonitor> {
        <DummyBackend as Backend>::get_device_monitor(&DummyBackend)
    }
}

struct PlayerState {
    paused: bool,
    muted: bool,
    volume: f64,
    rate: f64,
    seekable: bool,
    initial_data_requested: bool,
    duration: Option<f64>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            paused: true,
            muted: false,
            volume: 1.0,
            rate: 1.0,
            seekable: false,
            initial_data_requested: false,
            duration: None,
        }
    }
}

struct WorkerMediaPlayer {
    id: usize,
    callbacks: Arc<Mutex<PlayerCallbacks>>,
    state: Arc<Mutex<PlayerState>>,
}

impl WorkerMediaPlayer {
    fn send(&self, op: u32, value: f64, data: &[u8]) -> Result<(), PlayerError> {
        match command(op, self.id, value, data) {
            0 => Ok(()),
            1 if op == MEDIA_PUSH_DATA => Err(PlayerError::EnoughData),
            _ => Err(PlayerError::Backend(
                "Worker media host rejected command".to_owned(),
            )),
        }
    }

    fn request_initial_data(&self) -> Result<(), PlayerError> {
        {
            let mut state = self.state.lock().unwrap();
            if state.initial_data_requested {
                return Ok(());
            }
            state.initial_data_requested = true;
        }

        // Wait until HTMLMediaElement has received response headers and made
        // its fetch context available before unlocking its initially-locked
        // input queue. Sending this from create_player can race that setup.
        let event_sender = self.callbacks.lock().unwrap().sender.clone();
        if let Err(error) = event_sender.send(PlayerEvent::NeedData) {
            self.state.lock().unwrap().initial_data_requested = false;
            return Err(PlayerError::Backend(format!(
                "Could not request initial Worker media data: {error:?}"
            )));
        }
        Ok(())
    }
}

impl Drop for WorkerMediaPlayer {
    fn drop(&mut self) {
        callback_registry().lock().unwrap().remove(&self.id);
        let _ = command(MEDIA_DESTROY, self.id, 0.0, &[]);
    }
}

impl MediaInstance for WorkerMediaPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(val).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.pause().map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        self.play().map_err(|_| MediaInstanceError)
    }
}

impl Player for WorkerMediaPlayer {
    fn play(&self) -> Result<(), PlayerError> {
        self.send(MEDIA_PLAY, 0.0, &[])?;
        self.state.lock().unwrap().paused = false;
        Ok(())
    }

    fn pause(&self) -> Result<(), PlayerError> {
        self.send(MEDIA_PAUSE, 0.0, &[])?;
        self.state.lock().unwrap().paused = true;
        Ok(())
    }

    fn paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn stop(&self) -> Result<(), PlayerError> {
        self.send(MEDIA_STOP, 0.0, &[])?;
        self.state.lock().unwrap().paused = true;
        Ok(())
    }

    fn seek(&self, time: f64) -> Result<(), PlayerError> {
        if !self.state.lock().unwrap().seekable {
            return Err(PlayerError::NonSeekableStream);
        }
        self.send(MEDIA_SEEK, time, &[])
    }

    fn seekable(&self) -> Vec<Range<f64>> {
        let state = self.state.lock().unwrap();
        if state.seekable {
            state.duration.map_or_else(Vec::new, |end| vec![0.0..end])
        } else {
            Vec::new()
        }
    }

    fn set_mute(&self, muted: bool) -> Result<(), PlayerError> {
        self.send(MEDIA_SET_MUTED, f64::from(muted), &[])?;
        self.state.lock().unwrap().muted = muted;
        Ok(())
    }

    fn muted(&self) -> bool {
        self.state.lock().unwrap().muted
    }

    fn set_volume(&self, volume: f64) -> Result<(), PlayerError> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(PlayerError::Backend("Invalid media volume".to_owned()));
        }
        self.send(MEDIA_SET_VOLUME, volume, &[])?;
        self.state.lock().unwrap().volume = volume;
        Ok(())
    }

    fn volume(&self) -> f64 {
        self.state.lock().unwrap().volume
    }

    fn set_input_size(&self, size: u64) -> Result<(), PlayerError> {
        self.send(MEDIA_SET_INPUT_SIZE, size as f64, &[])
    }

    fn set_seekable(&self, seekable: bool) -> Result<(), PlayerError> {
        self.send(MEDIA_SET_SEEKABLE, f64::from(seekable), &[])?;
        self.state.lock().unwrap().seekable = seekable;
        self.request_initial_data()
    }

    fn set_download_buffering_enabled(&self, enabled: bool) -> Result<(), PlayerError> {
        self.send(MEDIA_SET_BUFFERING, f64::from(enabled), &[])
    }

    fn set_playback_rate(&self, rate: f64) -> Result<(), PlayerError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(PlayerError::Backend("Invalid playback rate".to_owned()));
        }
        self.send(MEDIA_SET_RATE, rate, &[])?;
        self.state.lock().unwrap().rate = rate;
        Ok(())
    }

    fn playback_rate(&self) -> f64 {
        self.state.lock().unwrap().rate
    }

    fn set_content_type(&self, content_type: String) -> Result<(), PlayerError> {
        self.send(MEDIA_SET_CONTENT_TYPE, 0.0, content_type.as_bytes())
    }

    fn push_data(&self, data: Vec<u8>) -> Result<(), PlayerError> {
        self.send(MEDIA_PUSH_DATA, 0.0, &data)
    }

    fn end_of_stream(&self) -> Result<(), PlayerError> {
        self.send(MEDIA_END_OF_STREAM, 0.0, &[])
    }

    fn buffered(&self) -> Vec<Range<f64>> {
        Vec::new()
    }

    fn set_stream(&self, _stream: &MediaStreamId, _only_stream: bool) -> Result<(), PlayerError> {
        Err(PlayerError::SetStreamFailed)
    }

    fn render_use_gl(&self) -> bool {
        false
    }

    fn set_audio_track(&self, _stream_index: i32, _enabled: bool) -> Result<(), PlayerError> {
        Ok(())
    }

    fn set_video_track(&self, _stream_index: i32, _enabled: bool) -> Result<(), PlayerError> {
        Ok(())
    }
}

struct RawFrame(Vec<u8>);

impl Buffer for RawFrame {
    fn to_vec(&self) -> Option<VideoFrameData> {
        Some(VideoFrameData::Raw(Arc::new(self.0.clone())))
    }
}

/// Deliver an asynchronously decoded BGRA video frame to Servo's normal
/// paint pipeline. This is called only after the Wasm host import has returned.
pub fn decoded_video_frame(id: usize, width: u32, height: u32, bytes: &[u8]) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    let (Ok(width_i32), Ok(height_i32)) = (i32::try_from(width), i32::try_from(height)) else {
        return false;
    };
    let Some(expected_len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return false;
    };
    if expected_len != bytes.len() {
        return false;
    }
    let Some(callbacks) = callbacks(id) else {
        return false;
    };
    let Some(renderer) = callbacks.lock().unwrap().video_renderer.clone() else {
        return false;
    };
    let Some(frame) = VideoFrame::new(width_i32, height_i32, Arc::new(RawFrame(bytes.to_vec())))
    else {
        return false;
    };
    renderer.lock().unwrap().render(frame);
    let sender = callbacks.lock().unwrap().sender.clone();
    let _ = sender.send(PlayerEvent::VideoFrameUpdated);
    true
}

/// Deliver planar float32 audio decoded by WebCodecs. Audio for a
/// MediaElementAudioSourceNode is returned to Servo's graph; ordinary media
/// output is transferred by the host to its device AudioContext.
pub fn decoded_audio_frame(id: usize, channels: u32, _sample_rate: u32, bytes: &[u8]) -> bool {
    let Some(bytes_per_frame) = (channels as usize).checked_mul(std::mem::size_of::<f32>()) else {
        return false;
    };
    if channels == 0 || bytes.len() % bytes_per_frame != 0 {
        return false;
    }
    let Some(callbacks) = callbacks(id) else {
        return false;
    };
    let Some(renderer) = callbacks.lock().unwrap().audio_renderer.clone() else {
        return false;
    };
    let samples = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect::<Vec<_>>();
    let frames_per_channel = samples.len() / channels as usize;
    for channel in 0..channels as usize {
        let data = (0..frames_per_channel)
            .map(|frame| samples[channel * frames_per_channel + frame])
            .collect::<Vec<_>>();
        renderer
            .lock()
            .unwrap()
            .render(Box::new(data), 1u32 << channel);
    }
    true
}

/// Forward a media lifecycle event from the Worker decoder to the Servo
/// HTMLMediaElement task source.
pub fn media_event(id: usize, kind: u32, value0: f64, value1: f64, data: &[u8]) -> bool {
    let Some(callbacks) = callbacks(id) else {
        return false;
    };
    let event = match kind {
        EVENT_METADATA => {
            let format = String::from_utf8_lossy(data).into_owned();
            let callbacks = callbacks.lock().unwrap();
            let Some(player_state) = callbacks.player_state.upgrade() else {
                return false;
            };
            let mut player_state = player_state.lock().unwrap();
            player_state.duration = (value0.is_finite() && value0 >= 0.0)
                .then(|| Duration::try_from_secs_f64(value0).ok())
                .flatten()
                .map(|duration| duration.as_secs_f64());
            let is_seekable = player_state.seekable && player_state.duration.is_some();
            drop(player_state);
            drop(callbacks);
            PlayerEvent::MetadataUpdated(Metadata {
                duration: (value0.is_finite() && value0 >= 0.0)
                    .then(|| Duration::try_from_secs_f64(value0).ok())
                    .flatten(),
                width: (value1 as u64 >> 32) as u32,
                height: value1 as u64 as u32,
                format,
                is_seekable,
                video_tracks: Vec::new(),
                audio_tracks: Vec::new(),
                is_live: false,
                title: None,
            })
        },
        EVENT_STATE => {
            let state = match value0 as u32 {
                0 => PlaybackState::Paused,
                1 => PlaybackState::Playing,
                2 => PlaybackState::Buffering,
                _ => PlaybackState::Stopped,
            };
            PlayerEvent::StateChanged(state)
        },
        EVENT_END_OF_STREAM => PlayerEvent::EndOfStream,
        EVENT_ENOUGH_DATA => PlayerEvent::EnoughData,
        EVENT_NEED_DATA => PlayerEvent::NeedData,
        EVENT_POSITION => PlayerEvent::PositionChanged(value0),
        EVENT_ERROR => PlayerEvent::Error(String::from_utf8_lossy(data).into_owned()),
        EVENT_DURATION => {
            let duration = (value0.is_finite() && value0 >= 0.0)
                .then(|| Duration::try_from_secs_f64(value0).ok())
                .flatten();
            if let Some(player_state) = callbacks.lock().unwrap().player_state.upgrade() {
                player_state.lock().unwrap().duration =
                    duration.map(|duration| duration.as_secs_f64());
            }
            PlayerEvent::DurationChanged(duration)
        },
        _ => return false,
    };
    callbacks.lock().unwrap().sender.send(event).is_ok()
}
