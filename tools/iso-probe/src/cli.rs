//! Argument handling, device lookup, and the `list` / `dump` subcommands.

use std::fmt::Write as _;
use std::path::PathBuf;

use uac_host::{AudioFunction, UacVersion};
use usbfs_iso::descriptor::{Direction, TransferType};
use usbfs_iso::{Depth, Speed, UsbFsDevice};

/// Anything that went wrong at the command-line level.
pub type Error = Box<dyn std::error::Error>;
pub type Result<T> = std::result::Result<T, Error>;

const USAGE: &str = "\
iso-probe — usbfs isochronous harness

USAGE:
  iso-probe list
  iso-probe dump   --device <VID:PID | PATH> [--out FILE]
  iso-probe spike  --device <VID:PID | PATH> [--interface N] [--alt N]
  iso-probe sweep  --device <VID:PID | PATH> [--seconds N] [--rate HZ] [--depths LIST]
                   [--packets LIST] [--policies LIST]
  iso-probe tone   --device <VID:PID | PATH> [--seconds N] [--rate HZ] [--hz FREQ]
                   [--depth MS] [--channels LIST]

OPTIONS:
  --device      054c:0ce6, or a path like /dev/bus/usb/001/004
  --interface   audio streaming interface (default: the first playback stream found)
  --alt         alternate setting (default: the one carrying the endpoint)
  --seconds     duration (default: 5 for sweep steps, 5 for tone)
  --rate        sample rate in Hz (default: the stream's highest advertised, capped at 48000)
  --hz          tone frequency (default: 200, low enough to feel on a voice coil)
  --depth       in-flight milliseconds for `tone` (default: 8)
  --depths      sweep depths in URBs      (default: 2,3,4,6,8,12,20,40,80)
  --packets     sweep packets per URB     (default: 1,2,4,8)
  --policies    default,audio,fifo        (default: default,audio)
  --channels    which channels the tone goes to, 0-based (default: all)

NOTES:
  On desktop Linux you need write access to the device node (a udev rule, or run as root).
  On Android the node is unreachable by path — use the android-tone example app, which gets the
  same descriptor from UsbDeviceConnection.
";

/// Parsed command line.
pub struct Args {
    pub device: Option<String>,
    pub interface: Option<u8>,
    pub alt: Option<u8>,
    pub seconds: Option<u64>,
    pub rate: Option<u32>,
    pub hz: Option<f32>,
    pub depth_ms: Option<u32>,
    pub depths: Option<Vec<usize>>,
    pub packets: Option<Vec<usize>>,
    pub policies: Option<Vec<String>>,
    pub channels: Option<Vec<usize>>,
    pub out: Option<PathBuf>,
}

pub fn run() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let Some(command) = argv.next() else {
        print!("{USAGE}");
        return Ok(());
    };
    if command == "-h" || command == "--help" || command == "help" {
        print!("{USAGE}");
        return Ok(());
    }

    let args = parse_args(argv)?;
    match command.as_str() {
        "list" => list(),
        "dump" => dump(&args),
        "spike" => crate::spike::run(&args),
        "sweep" => crate::sweep::run(&args),
        "tone" => crate::tone::run(&args),
        other => {
            print!("{USAGE}");
            Err(format!("unknown command \"{other}\"").into())
        }
    }
}

fn parse_args(argv: impl Iterator<Item = String>) -> Result<Args> {
    let mut args = Args {
        device: None,
        interface: None,
        alt: None,
        seconds: None,
        rate: None,
        hz: None,
        depth_ms: None,
        depths: None,
        packets: None,
        policies: None,
        channels: None,
        out: None,
    };
    let mut argv = argv.peekable();
    while let Some(flag) = argv.next() {
        let mut value = || {
            argv.next()
                .ok_or_else(|| -> Error { format!("{flag} needs a value").into() })
        };
        match flag.as_str() {
            "--device" | "-d" => args.device = Some(value()?),
            "--interface" => args.interface = Some(value()?.parse()?),
            "--alt" => args.alt = Some(value()?.parse()?),
            "--seconds" => args.seconds = Some(value()?.parse()?),
            "--rate" => args.rate = Some(value()?.parse()?),
            "--hz" => args.hz = Some(value()?.parse()?),
            "--depth" => args.depth_ms = Some(value()?.parse()?),
            "--depths" => args.depths = Some(parse_usize_list(&value()?)?),
            "--packets" => args.packets = Some(parse_usize_list(&value()?)?),
            "--channels" => args.channels = Some(parse_usize_list(&value()?)?),
            "--policies" => {
                args.policies = Some(value()?.split(',').map(|s| s.trim().to_owned()).collect())
            }
            "--out" | "-o" => args.out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown option \"{other}\"").into()),
        }
    }
    Ok(args)
}

fn parse_usize_list(s: &str) -> Result<Vec<usize>> {
    s.split(',')
        .map(|p| p.trim().parse::<usize>().map_err(Into::into))
        .collect()
}

/// Every usbfs device node on the system.
fn device_nodes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(buses) = std::fs::read_dir("/dev/bus/usb") else {
        return out;
    };
    for bus in buses.flatten() {
        let Ok(devices) = std::fs::read_dir(bus.path()) else {
            continue;
        };
        for dev in devices.flatten() {
            out.push(dev.path());
        }
    }
    out.sort();
    out
}

/// Resolve `--device` to an open handle: a path opens directly, `VID:PID` scans the bus.
pub fn open(args: &Args) -> Result<UsbFsDevice> {
    let spec = args
        .device
        .as_deref()
        .ok_or("--device is required (VID:PID or a /dev/bus/usb path)")?;

    if spec.contains('/') {
        return Ok(UsbFsDevice::open(spec)?);
    }
    let (vid, pid) = spec
        .split_once(':')
        .ok_or("--device must be VID:PID (hex) or a path")?;
    let vid = u16::from_str_radix(vid.trim_start_matches("0x"), 16)?;
    let pid = u16::from_str_radix(pid.trim_start_matches("0x"), 16)?;

    for path in device_nodes() {
        let Ok(dev) = UsbFsDevice::open(&path) else {
            continue;
        };
        let Ok(d) = dev.device_descriptor() else {
            continue;
        };
        if d.vendor_id == vid && d.product_id == pid {
            eprintln!("using {}", path.display());
            return Ok(dev);
        }
    }
    Err(format!(
        "no device {vid:04x}:{pid:04x} found. Is it plugged in, and do you have write access to \
         /dev/bus/usb (udev rule, or run as root)?"
    )
    .into())
}

/// The playback stream the caller asked for, or the first usable one.
pub fn pick_output_stream<'a>(
    function: &'a AudioFunction,
    args: &Args,
) -> Result<&'a uac_host::AudioStream> {
    let mut candidates: Vec<_> = function.output_streams().collect();
    if let Some(i) = args.interface {
        candidates.retain(|s| s.interface() == i);
    }
    if let Some(a) = args.alt {
        candidates.retain(|s| s.alt_setting() == a);
    }
    // Prefer the richest stream: more channels first, then the wider format. On a DualSense that
    // is the 4-channel setting, and picking a 2-channel one would silently leave the voice coils
    // (channels 3 and 4) unfed.
    candidates.sort_by_key(|s| {
        (
            std::cmp::Reverse(s.channels()),
            std::cmp::Reverse(s.format().bytes_per_sample()),
        )
    });
    candidates
        .first()
        .copied()
        .ok_or_else(|| "no playback stream matches".into())
}

/// Pick a sample rate: the caller's, else the highest advertised, capped at 48 kHz.
pub fn pick_rate(stream: &uac_host::AudioStream, args: &Args) -> Result<u32> {
    if let Some(r) = args.rate {
        return Ok(r);
    }
    match stream.rates().max() {
        Some(r) => Ok(r.min(48_000)),
        None => Err("this stream's rates are unresolved; pass --rate".into()),
    }
}

/// Default in-flight depth for the streaming subcommands.
pub fn depth(args: &Args) -> Depth {
    Depth::Millis(args.depth_ms.unwrap_or(8))
}

fn list() -> Result<()> {
    let nodes = device_nodes();
    if nodes.is_empty() {
        println!("no usbfs device nodes under /dev/bus/usb");
        return Ok(());
    }
    for path in nodes {
        let dev = match UsbFsDevice::open(&path) {
            Ok(d) => d,
            Err(e) => {
                println!("{}  <cannot open: {e}>", path.display());
                continue;
            }
        };
        let Ok(desc) = dev.device_descriptor() else {
            continue;
        };
        let speed = dev.speed().unwrap_or(Speed::Unknown);
        println!(
            "{}  {:04x}:{:04x}  {:?}-speed",
            path.display(),
            desc.vendor_id,
            desc.product_id,
            speed
        );

        let Ok(blob) = dev.raw_descriptors() else {
            continue;
        };
        let Ok(functions) = uac_host::parse_all(&blob) else {
            continue;
        };
        for f in &functions {
            println!(
                "    audio function on interface {} ({})",
                f.control_interface(),
                match f.version() {
                    UacVersion::Uac1 => "UAC1",
                    UacVersion::Uac2 => "UAC2",
                }
            );
            for s in f.streams() {
                let dir = match s.direction() {
                    Direction::Out => "out",
                    Direction::In => "in ",
                };
                let mut line = String::new();
                let _ = write!(
                    line,
                    "      {dir} if{}/alt{} {}ch {} {} ep 0x{:02x} {:?} {} B/interval bInterval {}",
                    s.interface(),
                    s.alt_setting(),
                    s.channels(),
                    s.format(),
                    s.rates(),
                    s.endpoint().address,
                    s.sync_type(),
                    s.endpoint().bytes_per_interval(),
                    s.endpoint().interval,
                );
                if let Some(fb) = s.feedback_endpoint() {
                    let _ = write!(line, " feedback 0x{fb:02x}");
                }
                // What the stream would actually do on this bus, derived rather than assumed.
                if let Ok(interval_us) = usbfs_iso::packet_interval_us(speed, s.endpoint().interval)
                {
                    let _ = write!(line, "  -> {interval_us} us/packet");
                }
                println!("{line}");
            }
        }

        // Anything isochronous that is not audio is still interesting to this crate's users.
        for (iface, eps) in usbfs_iso::descriptor::interfaces(&blob) {
            for ep in eps {
                if ep.transfer_type() == TransferType::Isochronous && iface.class != 0x01 {
                    println!(
                        "      iso (class 0x{:02x}) if{}/alt{} ep 0x{:02x} {} B/interval",
                        iface.class,
                        iface.number,
                        iface.alt_setting,
                        ep.address,
                        ep.bytes_per_interval()
                    );
                }
            }
        }
    }
    Ok(())
}

fn dump(args: &Args) -> Result<()> {
    let dev = open(args)?;
    let blob = dev.raw_descriptors()?;
    match &args.out {
        Some(path) => {
            std::fs::write(path, &blob)?;
            eprintln!("wrote {} bytes to {}", blob.len(), path.display());
        }
        None => {
            // Rust source, so a capture can be pasted straight over the synthesised fixture.
            println!(
                "// {} bytes from {:?}",
                blob.len(),
                dev.device_descriptor()?
            );
            println!("pub static DESCRIPTORS: &[u8] = &[");
            for chunk in blob.chunks(12) {
                let row: Vec<String> = chunk.iter().map(|b| format!("0x{b:02x}")).collect();
                println!("    {},", row.join(", "));
            }
            println!("];");
        }
    }
    Ok(())
}
