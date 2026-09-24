use clap::Parser;
use std::path::Path;
use std::fs;
use std::io::Write;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;
use walkdir::WalkDir;
use aubio::{OnsetMode, Tempo};
use biquad::*;
use binrw::{binrw, BinWrite};
use modular_bitfield::prelude::*;

#[bitfield]
#[derive(Debug, PartialEq, Eq, Clone, Copy, BinWrite)]
#[bw(map = |x: &WaveformPreviewColumn| x.into_bytes())]
pub struct WaveformPreviewColumn {
    pub height: B5,
    pub whiteness: B3,
}

#[binrw]
#[brw(big)]
pub enum ContentKind {
    #[brw(magic = b"PMAI")]
    File,
    #[brw(magic = b"PWAV")]
    WaveformPreview,
    #[brw(magic = b"PWV3")]
    WaveformDetail,
}

#[binrw]
#[brw(big)]
pub struct Header {
    pub kind: ContentKind,
    pub size: u32,
    pub total_size: u32,
}

/// rekord-lite: A tool to export music for Pioneer XDJ-XZ
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The directory containing the music files to scan
    #[arg(short, long)]
    dir: String,

    /// The output USB directory where the PIONEER structure will be created
    #[arg(short, long)]
    out: String,
}

fn main() {
    let args = Args::parse();
    let root_path = Path::new(&args.dir);
    let out_path = Path::new(&args.out);

    if !root_path.exists() || !root_path.is_dir() {
        eprintln!("Error: Directory '{}' does not exist.", args.dir);
        return;
    }

    // Create the PIONEER/USBANLZ directory structure
    let usbanlz_dir = out_path.join("PIONEER").join("USBANLZ");
    let rekordbox_dir = out_path.join("PIONEER").join("rekordbox");

    if let Err(e) = fs::create_dir_all(&usbanlz_dir) {
        eprintln!("Error: Could not create output directory structure: {}", e);
        return;
    }

    if let Err(e) = fs::create_dir_all(&rekordbox_dir) {
        eprintln!("Error: Could not create rekordbox directory structure: {}", e);
        return;
    }

    // Create a stub DeviceSQL database file
    let pdb_path = rekordbox_dir.join("export.pdb");
    if !pdb_path.exists() {
        if let Ok(mut f) = fs::File::create(&pdb_path) {
            f.write_all(b"STUB_DEVICE_SQL_DATABASE_TEMPLATE").ok();
        }
    }

    println!("Scanning directory: {}", args.dir);

    let mut track_counter = 1;

    for entry in WalkDir::new(root_path).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
                let ext_lower = extension.to_lowercase();
                if ext_lower == "wav" || ext_lower == "aiff" || ext_lower == "aif" || ext_lower == "flac" {
                    println!("\nFound file: {}", path.display());
                    process_file(path, &usbanlz_dir, track_counter);
                    track_counter += 1;
                }
            }
        }
    }
}

fn process_file(path: &Path, usbanlz_dir: &Path, track_id: u32) {
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

    let track_id_audio = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);

    let win_size = 2048;
    let hop_size = 512;
    let mut tempo = Tempo::new(OnsetMode::SpecFlux, win_size, hop_size, sample_rate).unwrap();

    let f0 = 200.0.hz();
    let fs = (sample_rate as f32).hz();
    let q_value = Q_BUTTERWORTH_F32;

    let coeffs = match Coefficients::<f32>::from_params(Type::LowPass, fs, f0, q_value) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  -> Erreur de création du filtre passe-bas: {:?}", e);
            return;
        }
    };

    let mut filter = DirectForm1::<f32>::new(coeffs);

    let mut total_rms = 0.0;
    let mut total_peak = 0.0;
    let mut blocks = 0;

    let mut global_max_peak = 0.0_f64;
    let mut chunk_peaks = Vec::new();

    let mut aubio_buffer = vec![];

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::ResetRequired) => unimplemented!(),
            Err(Error::IoError(_)) => break,
            Err(err) => {
                eprintln!("  -> Decode error: {}", err);
                break;
            }
        };

        if packet.track_id() != track_id_audio {
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

                for chunk in samples.chunks(channels) {
                    let s = chunk[0];

                    local_rms_sq += (s * s) as f64;
                    if s.abs() > local_peak {
                        local_peak = s.abs();
                    }

                    let filtered_s = filter.run(s);
                    aubio_buffer.push(filtered_s);

                    if aubio_buffer.len() >= hop_size {
                        tempo.do_result(aubio_buffer.as_slice()).unwrap();
                        aubio_buffer.clear();
                    }
                }

                let frames = samples.len() / channels;
                if frames > 0 {
                    let local_rms = (local_rms_sq / frames as f64).sqrt();
                    total_rms += local_rms;
                    total_peak += local_peak as f64;

                    if (local_peak as f64) > global_max_peak {
                        global_max_peak = local_peak as f64;
                    }

                    chunk_peaks.push(local_peak as f64);
                    blocks += 1;
                }
            }
            Err(Error::IoError(_)) => break,
            Err(Error::DecodeError(err)) => eprintln!("  -> Erreur décodage: {}", err),
            Err(err) => {
                eprintln!("  -> Erreur fatale: {}", err);
                break;
            }
        }
    }

    if blocks > 0 {
        let avg_rms = total_rms / blocks as f64;
        let avg_peak = total_peak / blocks as f64;

        let mut raw_bpm = tempo.get_bpm();
        if raw_bpm > 0.0 {
            while raw_bpm < 100.0 {
                raw_bpm *= 2.0;
            }
            while raw_bpm >= 200.0 {
                raw_bpm /= 2.0;
            }
        }

        let rounded_bpm = raw_bpm.round();

        println!("  -> Analyse Audio (Waveform base): Avg RMS: {:.4}, Avg Peak: {:.4}", avg_rms, avg_peak);
        println!("  -> BPM Lissé: {:.1} (BPM brut filtré: {:.3})", rounded_bpm, raw_bpm);

        // Generate Pioneer waveform data
        let mut dat_waveform_data = Vec::new();
        // Fixed-width preview is usually 400 columns for Pioneer
        let dat_num_columns = 400;
        let chunk_size = (chunk_peaks.len() as f64 / dat_num_columns as f64).ceil() as usize;

        for i in 0..dat_num_columns {
            let start = i * chunk_size;
            let end = std::cmp::min(start + chunk_size, chunk_peaks.len());

            let mut col_max = 0.0;
            for j in start..end {
                if chunk_peaks[j] > col_max {
                    col_max = chunk_peaks[j];
                }
            }

            // Normalize to 0-31 (5 bits)
            let mut height = if global_max_peak > 0.0 {
                ((col_max / global_max_peak) * 31.0).round() as u8
            } else {
                0
            };

            if height > 31 { height = 31; }

            let mut col = WaveformPreviewColumn::new();
            col.set_height(height);
            col.set_whiteness(7); // Full white for monochrome

            dat_waveform_data.push(col);
        }

        // Write .DAT file
        let dat_path = usbanlz_dir.join(format!("{:08X}.DAT", track_id));
        if let Ok(mut file) = fs::File::create(&dat_path) {
            let pmai_header = Header {
                kind: ContentKind::File,
                size: 28,
                total_size: 28 + 20 + dat_waveform_data.len() as u32,
            };
            pmai_header.write(&mut file).unwrap();

            // Write padding/unknown bytes of PMAI header (16 bytes)
            file.write_all(&[0; 16]).unwrap();

            let pwav_header = Header {
                kind: ContentKind::WaveformPreview,
                size: 12,
                total_size: 20 + dat_waveform_data.len() as u32,
            };
            pwav_header.write(&mut file).unwrap();

            let len_preview = dat_waveform_data.len() as u32;
            let unknown = 0x00100000;
            len_preview.write_be(&mut file).unwrap();
            unknown.write_be(&mut file).unwrap();

            for col in dat_waveform_data {
                col.write(&mut file).unwrap();
            }
            println!("  -> Wrote {}", dat_path.display());
        }

        // Write .EXT file (Detailed Waveform PWV3)
        // Detail waveform can have variable columns depending on length. For simplicitly here, we scale it
        let ext_waveform_data = chunk_peaks.clone(); // In reality, we'd downsample this to 150 points per second
        let mut ext_cols = Vec::new();
        for p in ext_waveform_data {
            let mut height = if global_max_peak > 0.0 {
                ((p / global_max_peak) * 31.0).round() as u8
            } else {
                0
            };
            if height > 31 { height = 31; }
            let mut col = WaveformPreviewColumn::new();
            col.set_height(height);
            col.set_whiteness(7);
            ext_cols.push(col);
        }

        let ext_path = usbanlz_dir.join(format!("{:08X}.EXT", track_id));
        if let Ok(mut file) = fs::File::create(&ext_path) {
            let pmai_header = Header {
                kind: ContentKind::File,
                size: 28,
                total_size: 28 + 24 + ext_cols.len() as u32,
            };
            pmai_header.write(&mut file).unwrap();

            file.write_all(&[0; 16]).unwrap();

            let pwv3_header = Header {
                kind: ContentKind::WaveformDetail,
                size: 12,
                total_size: 24 + ext_cols.len() as u32,
            };
            pwv3_header.write(&mut file).unwrap();

            let len_entry = 1u32;
            let len_entries = ext_cols.len() as u32;
            let unknown = 0x00100000;

            len_entry.write_be(&mut file).unwrap();
            len_entries.write_be(&mut file).unwrap();
            unknown.write_be(&mut file).unwrap();

            for col in ext_cols {
                col.write(&mut file).unwrap();
            }
            println!("  -> Wrote {}", ext_path.display());
        }
    }
}
