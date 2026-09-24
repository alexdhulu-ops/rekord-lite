use clap::Parser;
use std::path::Path;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;
use walkdir::WalkDir;
use aubio::{OnsetMode, Tempo};
use biquad::*;

/// rekord-lite: A tool to export music for Pioneer XDJ-XZ
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The directory containing the music files to scan
    #[arg(short, long)]
    dir: String,
}

fn main() {
    let args = Args::parse();
    let root_path = Path::new(&args.dir);

    if !root_path.exists() || !root_path.is_dir() {
        eprintln!("Error: Directory '{}' does not exist.", args.dir);
        return;
    }

    println!("Scanning directory: {}", args.dir);

    for entry in WalkDir::new(root_path).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
                let ext_lower = extension.to_lowercase();
                if ext_lower == "wav" || ext_lower == "aiff" || ext_lower == "aif" || ext_lower == "flac" {
                    println!("\nFound file: {}", path.display());
                    process_file(path);
                }
            }
        }
    }
}

fn process_file(path: &Path) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("  -> Failed to open file: {}", e);
            return;
        }
    };

    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();

    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let meta_opts: MetadataOptions = Default::default();
    let fmt_opts: FormatOptions = Default::default();

    let probed = match symphonia::default::get_probe().format(&hint, mss, &fmt_opts, &meta_opts) {
        Ok(probed) => probed,
        Err(e) => {
            eprintln!("  -> Failed to probe media: {}", e);
            return;
        }
    };

    let mut format = probed.format;
    let metadata = format.metadata();

    if let Some(metadata_rev) = metadata.current() {
        for tag in metadata_rev.tags() {
            if let Some(std_key) = tag.std_key {
                match std_key {
                    StandardTagKey::Artist => println!("  -> Artist: {}", tag.value),
                    StandardTagKey::Album => println!("  -> Album: {}", tag.value),
                    StandardTagKey::TrackTitle => println!("  -> Title: {}", tag.value),
                    StandardTagKey::Genre => println!("  -> Genre: {}", tag.value),
                    StandardTagKey::Date => println!("  -> Date: {}", tag.value),
                    _ => {}
                }
            }
        }
    } else {
        println!("  -> No metadata found.");
    }

    let track = match format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL) {
        Some(t) => t,
        None => {
            eprintln!("  -> No supported audio tracks found.");
            return;
        }
    };

    let dec_opts: DecoderOptions = Default::default();
    let mut decoder = match symphonia::default::get_codecs().make(&track.codec_params, &dec_opts) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("  -> Unsupported codec: {}", e);
            return;
        }
    };

    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);

    // Audio Analysis & BPM Setup
    // Increase window and hop size for better stability in electronic music tempo extraction
    let win_size = 2048;
    let hop_size = 512;
    // SpecFlux is good, but Energy often works well when paired with Low Pass filter
    let mut tempo = Tempo::new(OnsetMode::SpecFlux, win_size, hop_size, sample_rate).unwrap();

    // Biquad Low Pass Filter setup to isolate the kicks (e.g. < 200 Hz)
    let f0 = 200.0.hz();
    let fs = (sample_rate as f32).hz();
    // Quality factor for Butterworth
    let q_value = Q_BUTTERWORTH_F32;

    let coeffs = match Coefficients::<f32>::from_params(Type::LowPass, fs, f0, q_value) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  -> Failed to create filter coeffs: {:?}", e);
            return;
        }
    };

    let mut filter = DirectForm1::<f32>::new(coeffs);

    let mut total_rms = 0.0;
    let mut total_peak = 0.0;
    let mut blocks = 0;

    let mut aubio_buffer = vec![];

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::ResetRequired) => {
                unimplemented!();
            }
            Err(Error::IoError(_)) => {
                break;
            }
            Err(err) => {
                eprintln!("  -> Decode error: {}", err);
                break;
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let mut sample_buf = symphonia::core::audio::SampleBuffer::<f32>::new(
                    decoded.capacity() as u64,
                    spec
                );
                sample_buf.copy_interleaved_ref(decoded);

                let samples = sample_buf.samples();
                let channels = spec.channels.count();

                let mut local_rms_sq = 0.0;
                let mut local_peak = 0.0_f32;

                // Process mono / left channel for BPM and analysis
                for chunk in samples.chunks(channels) {
                    let s = chunk[0];

                    // Original sample is used for RMS and peak calculations
                    local_rms_sq += (s * s) as f64;
                    if s.abs() > local_peak {
                        local_peak = s.abs();
                    }

                    // Filtered sample is sent to the BPM detection logic
                    let filtered_s = filter.run(s);
                    aubio_buffer.push(filtered_s);

                    if aubio_buffer.len() >= hop_size {
                        tempo.do_result(aubio_buffer.as_slice()).unwrap();
                        aubio_buffer.clear();
                    }
                }

                let frames = samples.len() / channels;
                if frames > 0 {
                    total_rms += (local_rms_sq / frames as f64).sqrt();
                    total_peak += local_peak as f64;
                    blocks += 1;
                }
            }
            Err(Error::IoError(_)) => break,
            Err(Error::DecodeError(err)) => {
                eprintln!("  -> Decode error: {}", err);
            }
            Err(err) => {
                eprintln!("  -> Fatal error: {}", err);
                break;
            }
        }
    }

    if blocks > 0 {
        let avg_rms = total_rms / blocks as f64;
        let avg_peak = total_peak / blocks as f64;

        // Final smoothing for BPM (e.g. restrict logical bounds and round)
        let mut raw_bpm = tempo.get_bpm();

        // Simple logic for doubling/halving if it detects out of bounds (100 to 200 BPM safely)
        if raw_bpm > 0.0 {
            while raw_bpm < 100.0 {
                raw_bpm *= 2.0;
            }
            while raw_bpm >= 200.0 {
                raw_bpm /= 2.0;
            }
        }

        let rounded_bpm = raw_bpm.round();

        println!("  -> Audio Analysis: Avg RMS: {:.4}, Avg Peak: {:.4}", avg_rms, avg_peak);
        println!("  -> BPM: {:.1} (raw: {:.3})", rounded_bpm, raw_bpm);
    }
}
