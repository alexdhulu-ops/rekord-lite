use clap::Parser;
use std::path::Path;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;
use walkdir::WalkDir;

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
                    print_metadata(path);
                }
            }
        }
    }
}

fn print_metadata(path: &Path) {
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

    match symphonia::default::get_probe().format(&hint, mss, &fmt_opts, &meta_opts) {
        Ok(probed) => {
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
        }
        Err(e) => {
            eprintln!("  -> Failed to probe media: {}", e);
        }
    }
}
