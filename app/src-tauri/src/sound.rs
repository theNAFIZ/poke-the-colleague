//! Ringing sound played through the default output device — i.e. the headphones
//! the colleague is wearing, which is the whole point.
//!
//! The tone is synthesised at start-up, so there is no audio asset to ship.
//! Audio runs on its own thread because the device handle is not `Send` on
//! every platform.

use std::num::NonZero;
use std::sync::mpsc;
use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::source::Source;
use tracing::warn;

const RATE: u32 = 44_100;
/// Safety net: never ring longer than this even if nobody stops us.
const MAX_RING: Duration = Duration::from_secs(90);

enum Cmd {
    Play { volume: f32 },
    Stop,
}

#[derive(Clone)]
pub struct Sound {
    tx: mpsc::Sender<Cmd>,
}

impl Sound {
    pub fn spawn() -> Sound {
        let (tx, rx) = mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("poke-audio".into())
            .spawn(move || audio_thread(rx))
            .expect("spawn audio thread");
        Sound { tx }
    }

    pub fn play(&self, volume: f32) {
        let _ = self.tx.send(Cmd::Play { volume: volume.clamp(0.0, 1.0) });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

fn audio_thread(rx: mpsc::Receiver<Cmd>) {
    let samples = ring_pattern();
    // (device handle, player) while ringing. Opened lazily so a missing or
    // changed audio device (headphones plugged in later) is picked up.
    let mut active: Option<(rodio::MixerDeviceSink, rodio::Player)> = None;
    loop {
        let cmd = if active.is_some() {
            match rx.recv_timeout(MAX_RING) {
                Ok(c) => c,
                Err(mpsc::RecvTimeoutError::Timeout) => Cmd::Stop,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(c) => c,
                Err(_) => return,
            }
        };
        match cmd {
            Cmd::Stop => {
                if let Some((_, player)) = active.take() {
                    player.stop();
                }
            }
            Cmd::Play { volume } => {
                if let Some((_, player)) = &active {
                    player.set_volume(volume);
                    continue;
                }
                match rodio::DeviceSinkBuilder::open_default_sink() {
                    Ok(mut handle) => {
                        handle.log_on_drop(false);
                        let player = rodio::Player::connect_new(handle.mixer());
                        player.set_volume(volume);
                        let source = SamplesBuffer::new(NonZero::new(1).unwrap(), NonZero::new(RATE).unwrap(), samples.clone());
                        player.append(source.repeat_infinite());
                        active = Some((handle, player));
                    }
                    Err(e) => warn!("cannot open audio output: {e}"),
                }
            }
        }
    }
}

/// Two-tone "ding-dong ding-dong", then a pause; ~2 s, looped.
fn ring_pattern() -> Vec<f32> {
    let mut out = Vec::new();
    let tone = |freq: f32, secs: f32, out: &mut Vec<f32>| {
        let n = (secs * RATE as f32) as usize;
        let attack = (0.005 * RATE as f32) as usize;
        let release = (0.04 * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = if i < attack {
                i as f32 / attack as f32
            } else if i > n - release {
                (n - i) as f32 / release as f32
            } else {
                1.0
            };
            let w = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.75
                + (2.0 * std::f32::consts::PI * freq * 2.0 * t).sin() * 0.25;
            out.push(w * env * 0.6);
        }
    };
    let silence = |secs: f32, out: &mut Vec<f32>| out.extend(std::iter::repeat_n(0.0, (secs * RATE as f32) as usize));
    for _ in 0..2 {
        tone(987.77, 0.14, &mut out); // B5
        silence(0.04, &mut out);
        tone(1318.51, 0.22, &mut out); // E6
        silence(0.18, &mut out);
    }
    silence(0.9, &mut out);
    out
}
