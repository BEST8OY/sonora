use std::ffi::c_void;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use music::Track;
use tokio::sync::mpsc;

use crate::{Cover, Io, Playback, PlaybackEvent, PlaybackState, Queue, Repeat, Sonora, join};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod mpris;
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
mod souvlaki;

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
use self::souvlaki::Controls;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use mpris::Controls;

const BUS_NAME: &str = "sonora";
const DISPLAY_NAME: &str = "Sonora";
const ARTWORK: &str = "artwork";

struct Attached {
    _remote: Entity<Remote>,
}

impl Global for Attached {}

/// A request from the desktop's media widget or media keys, in the terms `Playback` and
/// `Queue` understand.
enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Seek(Duration),
    Forward(Duration),
    Back(Duration),
    Volume(f64),
    /// Only MPRIS carries shuffle and repeat, souvlaki has neither.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Shuffle(bool),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Repeat(Repeat),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Raise,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Quit,
}

/// Publishes what plays to the system media controls and carries their requests back. The
/// window handle is only read on Windows, where the controls hang off the window.
pub fn attach(hwnd: Option<*mut c_void>, cx: &mut App) {
    if cx.has_global::<Attached>() {
        return;
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    let controls = match Controls::new(hwnd, sender, cx) {
        Ok(controls) => controls,
        Err(error) => {
            return log::warn!("remote: cannot reach the system media controls: {error:#}");
        }
    };

    let sonora = Sonora::global(cx);
    let playback = sonora.playback.clone();
    let queue = sonora.queue.clone();
    let cover = sonora.cover.clone();
    let io = Io::global(cx);
    let remote = cx.new(|cx| Remote::new(controls, receiver, playback, queue, cover, io, cx));
    remote.update(cx, |remote, cx| remote.publish(cx));
    cx.set_global(Attached { _remote: remote });
}

pub struct Remote {
    controls: Controls,
    playback: Entity<Playback>,
    queue: Entity<Queue>,
    cover: Entity<Cover>,
    io: Io,
    shown: Option<String>,
    source: Option<String>,
    reported: Option<(PlaybackState, bool, bool)>,
    at: Duration,
    /// When `at` was published, so the next position can be checked against steady playback.
    stamp: Instant,
    volume: Option<f32>,
    shuffle: Option<bool>,
    repeat: Option<Repeat>,
    artwork: Option<Task<()>>,
    _events: Task<()>,
}

impl Remote {
    fn new(
        controls: Controls,
        mut receiver: mpsc::UnboundedReceiver<Command>,
        playback: Entity<Playback>,
        queue: Entity<Queue>,
        cover: Entity<Cover>,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        let _events = cx.spawn(async move |this, cx| {
            while let Some(command) = receiver.recv().await {
                if this.update(cx, |this, cx| this.act(command, cx)).is_err() {
                    break;
                }
            }
        });

        cx.observe(&playback, |this, _, cx| this.publish(cx))
            .detach();
        cx.subscribe(&playback, |this, _, event, cx| match event {
            PlaybackEvent::Seeked(at) => this.on_seeked(*at, cx),
            _ => {}
        })
        .detach();
        cx.observe(&queue, |this, _, cx| this.publish(cx)).detach();
        // the album art resolves after the track it belongs to, so republish when it lands
        cx.observe(&cover, |this, _, cx| this.publish(cx)).detach();

        Self {
            controls,
            playback,
            queue,
            cover,
            io,
            shown: None,
            source: None,
            reported: None,
            at: Duration::ZERO,
            stamp: Instant::now(),
            volume: None,
            shuffle: None,
            repeat: None,
            artwork: None,
            _events,
        }
    }

    fn on_seeked(&mut self, at: Duration, cx: &mut Context<Self>) {
        let playback = self.playback.read(cx);
        let state = playback.state().clone();
        let wants_playing = playback.wants_playing();
        let is_seeking = playback.is_seeking();
        self.reported = Some((state.clone(), wants_playing, is_seeking));
        self.at = at;
        self.stamp = Instant::now();
        self.controls
            .set_playback(&state, at, wants_playing, is_seeking);
        self.controls.seeked(at);
    }

    fn act(&mut self, command: Command, cx: &mut Context<Self>) {
        match command {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Command::Raise => {
                cx.activate(true);
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Command::Quit => {
                cx.quit();
            }
            Command::Play => {
                self.playback.update(cx, |playback, cx| playback.resume(cx));
            }
            Command::Pause => {
                self.playback.update(cx, |playback, cx| playback.pause(cx));
            }
            Command::Toggle => {
                self.playback
                    .update(cx, |playback, cx| playback.toggle_play(cx));
            }
            Command::Next => {
                self.playback.update(cx, |playback, cx| playback.next(cx));
            }
            Command::Previous => {
                self.playback
                    .update(cx, |playback, cx| playback.previous(cx));
            }
            Command::Seek(at) => {
                self.playback
                    .update(cx, |playback, cx| playback.seek(at, cx));
            }
            Command::Forward(step) => {
                self.playback.update(cx, |playback, cx| {
                    shift(playback, playback.live_position().saturating_add(step), cx)
                });
            }
            Command::Back(step) => {
                self.playback.update(cx, |playback, cx| {
                    shift(playback, playback.live_position().saturating_sub(step), cx)
                });
            }
            Command::Volume(level) => {
                self.playback
                    .update(cx, |playback, cx| playback.set_volume(level as f32, cx));
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Command::Repeat(repeat) => {
                self.playback
                    .update(cx, |playback, cx| playback.set_repeat(repeat, cx));
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Command::Shuffle(on) => {
                self.queue.update(cx, |queue, cx| queue.set_shuffle(on, cx));
            }
        }
    }

    fn publish(&mut self, cx: &mut Context<Self>) {
        let playback = self.playback.read(cx);
        let queue = self.queue.read(cx);
        let state = playback.state().clone();
        let wants_playing = playback.wants_playing();
        let is_seeking = playback.is_seeking();
        let at = playback.live_position();
        let track = playback.track().cloned();
        let volume = playback.volume();
        let repeat = playback.repeat();
        let shuffle = queue.shuffle();

        let can_play = track.is_some() || queue.has_next() || !queue.is_empty();
        let can_pause = track.is_some();
        let can_seek = track.as_ref().is_some_and(|t| t.duration > Duration::ZERO);
        let can_go_next = queue.has_next();
        let can_go_previous = queue.has_previous();

        self.controls
            .set_capabilities(can_play, can_pause, can_seek, can_go_next, can_go_previous);

        if self.volume != Some(volume) {
            self.volume = Some(volume);
            self.controls.set_volume(volume.into());
        }
        if self.shuffle != Some(shuffle) {
            self.shuffle = Some(shuffle);
            self.controls.set_shuffle(shuffle);
        }
        if self.repeat != Some(repeat) {
            self.repeat = Some(repeat);
            self.controls.set_repeat(repeat);
        }

        let id = track.as_ref().and_then(|track| track.id.clone());
        let cover = track.as_ref().and_then(|track| self.artwork_url(track, cx));
        let moved = id != self.shown;
        if moved || cover != self.source {
            self.shown = id;
            self.source = cover.clone();
            self.artwork = None;
            let remote = cover.as_deref().is_some_and(is_remote);
            match (moved, remote) {
                // a remote cover follows once it sits in the cache, and a sharper one for the
                // track already on show leaves the published thumbnail up until the file lands
                (false, true) => {}
                // anything else is a file already
                _ => self
                    .controls
                    .describe(track.as_ref(), cover.as_deref().filter(|_| !remote)),
            }
            if let (Some(track), Some(url), true) = (track, cover, remote) {
                self.artwork = Some(self.fetch_artwork(track, url, cx));
            }
        }

        let looped = !moved && self.shown.is_some() && at < self.at;
        let current = (state.clone(), wants_playing, is_seeking);
        let status_changed = self.reported.as_ref() != Some(&current);
        if !status_changed && !looped && self.at.as_secs() == at.as_secs() {
            return;
        }
        self.reported = Some(current);
        self.at = at;
        self.stamp = Instant::now();

        self.controls
            .set_playback(&state, at, wants_playing, is_seeking);
        if looped {
            self.controls.seeked(at);
        }
    }
}

impl Remote {
    /// The cover to publish: the album art `Cover` resolves once it arrives, the thumbnail the
    /// track carries until then. Spotify ships a 64px thumbnail with a track, which is plenty
    /// for a list row and blurry in a desktop widget that draws it several times that size.
    fn artwork_url(&self, track: &Track, cx: &App) -> Option<String> {
        track
            .album_id
            .as_deref()
            .and_then(|album| self.cover.read(cx).large_for(album))
            .map(str::to_owned)
            .or_else(|| track.cover.clone())
    }

    /// Brings a remote cover into the cache and republishes the track with the file. The
    /// platform widget would otherwise download it itself, on its own thread, and on macOS a
    /// download that fails there takes the process down.
    fn fetch_artwork(&self, track: Track, url: String, cx: &mut Context<Self>) -> Task<()> {
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let fetched = join(io.spawn(async move { artwork(&url).await })).await;
            let path = match fetched {
                Ok(path) => path,
                Err(error) => return log::debug!("remote: cannot fetch the cover: {error:#}"),
            };
            this.update(cx, |this, _| {
                if this.shown == track.id {
                    let cover = format!("file://{}", path.display());
                    this.controls.describe(Some(&track), Some(&cover));
                }
            })
            .ok();
        })
    }
}

fn is_remote(cover: &str) -> bool {
    cover.starts_with("http://") || cover.starts_with("https://")
}

/// The cached file for a cover url, downloaded on first sight. The bytes have to decode as an
/// image before they are kept, so the widget never opens something it cannot draw.
async fn artwork(url: &str) -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sonora")
        .join(ARTWORK);
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());
    for format in [
        image::ImageFormat::Jpeg,
        image::ImageFormat::Png,
        image::ImageFormat::WebP,
    ] {
        let candidate = dir.join(&key).with_extension(extension(format));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    let bytes = reqwest::get(url)
        .await
        .context("cannot request the cover")?
        .error_for_status()
        .context("the cover request was refused")?
        .bytes()
        .await
        .context("cannot read the cover")?;
    let format = image::guess_format(&bytes).context("cannot tell the cover format")?;
    image::load_from_memory_with_format(&bytes, format).context("cannot decode the cover")?;

    std::fs::create_dir_all(&dir).context("cannot create the artwork cache")?;
    let path = dir.join(&key).with_extension(extension(format));
    std::fs::write(&path, &bytes).context("cannot store the cover")?;
    Ok(path)
}

fn extension(format: image::ImageFormat) -> &'static str {
    format.extensions_str().first().copied().unwrap_or("img")
}

/// Seeks to `target`, held inside the current track.
fn shift(playback: &mut Playback, target: Duration, cx: &mut Context<Playback>) {
    let end = playback
        .track()
        .map(|track| track.duration)
        .unwrap_or(target);
    playback.seek(target.min(end), cx);
}
