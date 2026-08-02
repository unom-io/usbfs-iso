//! `tone` — play a sine wave. The only check that distinguishes "no errors reported" from
//! "actually sounds right".
//!
//! `--channels` matters more than it looks. On a DualSense the four channels are speaker left,
//! speaker right, left voice coil, right voice coil — so `--channels 2,3` drives the haptic
//! actuators alone, and a low `--hz` is what you can actually feel. Feeding all four at once makes
//! it impossible to tell which actuator is responding.

use std::f32::consts::TAU;
use std::time::Duration;

use crate::cli::{self, Args, Result};

pub fn run(args: &Args) -> Result<()> {
    let dev = cli::open(args)?;
    let blob = dev.raw_descriptors()?;
    let function = uac_host::parse(&blob)?;
    let stream = cli::pick_output_stream(&function, args)?;
    let rate = cli::pick_rate(stream, args)?;

    if stream.format() != uac_host::Format::S16Le {
        return Err(format!(
            "the tone generator only writes S16_LE; this stream carries {}",
            stream.format()
        )
        .into());
    }

    let channels = stream.channels() as usize;
    let targets: Vec<usize> = match &args.channels {
        Some(list) => {
            if let Some(&bad) = list.iter().find(|&&c| c >= channels) {
                return Err(format!("channel {bad} does not exist (stream has {channels})").into());
            }
            list.clone()
        }
        None => (0..channels).collect(),
    };

    let opts = uac_host::OpenOptions {
        depth: cli::depth(args),
        ..Default::default()
    };
    let mut playback = stream.open_with(&dev, stream.format(), rate, opts)?;

    let hz = args.hz.unwrap_or(200.0);
    let seconds = args.seconds.unwrap_or(5);
    println!(
        "playing {hz} Hz for {seconds} s into channels {targets:?} of {channels}, at {rate} Hz, \
         {} us in flight",
        playback.schedule().in_flight_us()
    );

    // One millisecond of audio per iteration: small enough that the write loop is genuinely
    // interleaved with the bus rather than dumping a buffer and sleeping.
    let frames_per_chunk = (rate as usize / 1000).max(1);
    let mut chunk = vec![0i16; frames_per_chunk * channels];
    let mut phase = 0.0f32;
    let step = TAU * hz / rate as f32;
    // -6 dBFS: loud enough to hear and feel, quiet enough not to clip a voice coil.
    let amplitude = 16_384.0f32;

    let total_frames = rate as u64 * seconds;
    let mut written = 0u64;
    while written < total_frames {
        for frame in chunk.chunks_mut(channels) {
            let sample = (phase.sin() * amplitude) as i16;
            phase += step;
            if phase >= TAU {
                phase -= TAU;
            }
            frame.fill(0);
            for &c in &targets {
                frame[c] = sample;
            }
        }
        playback.write_interleaved(&chunk)?;
        written += frames_per_chunk as u64;
    }

    playback.drain(Duration::from_millis(500))?;
    let stats = playback.stats();
    println!(
        "done: {} frames, {} urbs, {} underruns, {} short bytes, {} packet errors, {} urb errors",
        playback.frames_written(),
        stats.urbs_completed,
        stats.underruns,
        stats.short_bytes,
        stats.packet_errors,
        stats.urb_errors
    );
    if stats.underruns > 0 || stats.short_bytes > 0 {
        println!("  -> the stream had holes; raise --depth and try again");
    }
    Ok(())
}
