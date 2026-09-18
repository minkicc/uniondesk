//! Clipboard synchronisation.
//!
//! The clipboard is polled rather than hooked. Every platform exposes a
//! "changed" notification, but each one has different failure modes around
//! apps that keep the clipboard open, and a 350 ms poll costs nothing while
//! being far more predictable. Content is identified by a hash, which also
//! stops a payload we just applied from being echoed back to its sender.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, trace, warn};

use ud_core::protocol::{Blob, ClipboardPayload};

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard unavailable: {0}")]
    Unavailable(String),

    #[error("image could not be decoded: {0}")]
    Image(String),

    #[error("the clipboard worker is not running")]
    WorkerGone,
}

pub type Result<T, E = ClipboardError> = std::result::Result<T, E>;

/// What the watcher reports.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardEvent {
    /// Local clipboard content changed.
    Changed(ClipboardPayload),
    /// The watcher could not read the clipboard; usually transient.
    Error(String),
}

/// Limits applied before content is handed to the transport.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub text: bool,
    pub images: bool,
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            text: true,
            images: true,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

enum Command {
    Apply(ClipboardPayload),
    SetLimits(Limits),
    Shutdown,
}

/// Owns the clipboard worker thread.
pub struct ClipboardWatcher {
    commands: Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardWatcher {
    /// Starts watching. Nothing is reported until the content actually differs
    /// from what was already on the clipboard at startup.
    pub fn start(
        interval: Duration,
        limits: Limits,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<ClipboardEvent>)> {
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("uniondesk-clipboard".into())
            .spawn(move || worker(commands_rx, events_tx, interval, limits))
            .map_err(|e| ClipboardError::Unavailable(e.to_string()))?;
        Ok((
            ClipboardWatcher {
                commands: commands_tx,
                thread: Some(thread),
            },
            events_rx,
        ))
    }

    /// Writes remote content into the local clipboard without reporting it back.
    pub fn apply(&self, payload: ClipboardPayload) -> Result<()> {
        self.commands
            .send(Command::Apply(payload))
            .map_err(|_| ClipboardError::WorkerGone)
    }

    pub fn set_limits(&self, limits: Limits) -> Result<()> {
        self.commands
            .send(Command::SetLimits(limits))
            .map_err(|_| ClipboardError::WorkerGone)
    }

    pub fn shutdown(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(
    commands: Receiver<Command>,
    events: UnboundedSender<ClipboardEvent>,
    interval: Duration,
    mut limits: Limits,
) {
    // Another process can hold the clipboard open, so keep retrying rather than
    // giving up on the first failure.
    let mut clipboard = loop {
        match open() {
            Ok(clipboard) => break clipboard,
            Err(err) => {
                let _ = events.send(ClipboardEvent::Error(err.to_string()));
                match commands.recv_timeout(Duration::from_secs(2)) {
                    Ok(Command::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return
                    }
                    Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }
    };

    let mut last = digest_of_snapshot(&snapshot(&mut clipboard, limits));
    let mut next_poll = Instant::now() + interval;
    let slice = Duration::from_millis(25);

    loop {
        match commands.recv_timeout(slice) {
            Ok(Command::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return
            }
            Ok(Command::Apply(payload)) => {
                if let Err(err) = write(&mut clipboard, &payload) {
                    warn!(error = %err, "could not write the clipboard");
                    let _ = events.send(ClipboardEvent::Error(err.to_string()));
                } else {
                    // Remember it so the next poll does not relay it back.
                    last = digest_of_snapshot(&Some(payload));
                }
            }
            Ok(Command::SetLimits(updated)) => {
                limits = updated;
                last = digest_of_snapshot(&snapshot(&mut clipboard, limits));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }

        if Instant::now() < next_poll {
            continue;
        }
        next_poll = Instant::now() + interval;

        let current = snapshot(&mut clipboard, limits);
        let digest = digest_of_snapshot(&current);
        if digest != last {
            last = digest;
            if let Some(payload) = current {
                trace!(kind = payload.kind(), "clipboard changed");
                if events.send(ClipboardEvent::Changed(payload)).is_err() {
                    return;
                }
            }
        }
    }
}

fn open() -> Result<arboard::Clipboard> {
    arboard::Clipboard::new().map_err(|e| ClipboardError::Unavailable(e.to_string()))
}

/// Reads the clipboard, preferring text because it is both cheaper to compare
/// and the common case. Images are only considered when no text is present.
fn snapshot(clipboard: &mut arboard::Clipboard, limits: Limits) -> Option<ClipboardPayload> {
    if limits.text {
        match clipboard.get_text() {
            Ok(text) if !text.is_empty() => {
                if text.len() > limits.max_bytes {
                    debug!(bytes = text.len(), "clipboard text exceeds the size limit");
                } else {
                    return Some(ClipboardPayload::Text { text });
                }
            }
            Ok(_) => {}
            Err(err) => trace!(error = %err, "clipboard has no text"),
        }
    }
    if limits.images {
        match clipboard.get_image() {
            Ok(image) => {
                if let Ok(png) = encode_png(image.width, image.height, &image.bytes) {
                    if png.len() <= limits.max_bytes {
                        return Some(ClipboardPayload::Image {
                            width: image.width as u32,
                            height: image.height as u32,
                            png: Blob::new(png),
                        });
                    }
                    debug!(bytes = png.len(), "clipboard image exceeds the size limit");
                }
            }
            Err(err) => trace!(error = %err, "clipboard has no image"),
        }
    }
    None
}

fn write(clipboard: &mut arboard::Clipboard, payload: &ClipboardPayload) -> Result<()> {
    match payload {
        ClipboardPayload::Text { text } => clipboard
            .set_text(text.clone())
            .map_err(|e| ClipboardError::Unavailable(e.to_string())),
        ClipboardPayload::Image { png, .. } => {
            let (width, height, rgba) = decode_png(&png.0)?;
            clipboard
                .set_image(arboard::ImageData {
                    width,
                    height,
                    bytes: rgba.into(),
                })
                .map_err(|e| ClipboardError::Unavailable(e.to_string()))
        }
        ClipboardPayload::Clear => clipboard
            .clear()
            .map_err(|e| ClipboardError::Unavailable(e.to_string())),
        // Reserved: file lists need platform specific clipboard formats which
        // are not wired up yet. Drag and drop covers the same use case today.
        ClipboardPayload::Files { paths } => {
            debug!(count = paths.len(), "file list clipboard payload ignored");
            Ok(())
        }
    }
}

fn digest_of_snapshot(payload: &Option<ClipboardPayload>) -> Option<[u8; 16]> {
    let payload = payload.as_ref()?;
    let mut hasher = Sha256::new();
    match payload {
        ClipboardPayload::Text { text } => {
            hasher.update([0u8]);
            hasher.update(text.as_bytes());
        }
        ClipboardPayload::Image { width, height, png } => {
            hasher.update([1u8]);
            hasher.update(width.to_le_bytes());
            hasher.update(height.to_le_bytes());
            hasher.update(&png.0);
        }
        ClipboardPayload::Files { paths } => {
            hasher.update([2u8]);
            for path in paths {
                hasher.update(path.as_bytes());
                hasher.update([0u8]);
            }
        }
        ClipboardPayload::Clear => hasher.update([3u8]),
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    Some(out)
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ClipboardError::Image(e.to_string()))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| ClipboardError::Image(e.to_string()))?;
    }
    Ok(out)
}

/// Decodes a PNG back into the RGBA8 buffer arboard wants.
fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|e| ClipboardError::Image(e.to_string()))?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|e| ClipboardError::Image(e.to_string()))?;
    buffer.truncate(info.buffer_size());
    let (width, height) = (info.width as usize, info.height as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => {
            let mut expanded = Vec::with_capacity(width * height * 4);
            for pixel in buffer.chunks_exact(3) {
                expanded.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            expanded
        }
        other => {
            return Err(ClipboardError::Image(format!(
                "unsupported PNG colour type {other:?}"
            )))
        }
    };
    Ok((width, height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip_preserves_pixels() {
        let pixels: Vec<u8> = (0..(4 * 3 * 4)).map(|i| (i * 7) as u8).collect();
        let encoded = encode_png(4, 3, &pixels).unwrap();
        let (width, height, decoded) = decode_png(&encoded).unwrap();
        assert_eq!((width, height), (4, 3));
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn digests_distinguish_content_and_are_stable() {
        let a = Some(ClipboardPayload::Text { text: "hello".into() });
        let b = Some(ClipboardPayload::Text { text: "hello ".into() });
        assert_eq!(digest_of_snapshot(&a), digest_of_snapshot(&a));
        assert_ne!(digest_of_snapshot(&a), digest_of_snapshot(&b));
        assert_eq!(digest_of_snapshot(&None), None);
    }

    #[test]
    fn text_and_image_with_the_same_body_do_not_collide() {
        let text = Some(ClipboardPayload::Text {
            text: "abc".into(),
        });
        let image = Some(ClipboardPayload::Image {
            width: 1,
            height: 1,
            png: Blob::new(b"abc".to_vec()),
        });
        assert_ne!(digest_of_snapshot(&text), digest_of_snapshot(&image));
    }
}
