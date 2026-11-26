use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use memmap2::Mmap;
use rayon::prelude::*;

fn inspect_one(path: &Path) -> Result<()> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let meta = file.metadata()?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".into());

    let report = if meta.len() > 0 {
        let mmap = unsafe { Mmap::map(&file)? };
        ufo_core::open_bytes(&name, &mmap, Some(meta.len()))
    } else {
        ufo_core::open_bytes(&name, &[], Some(0))
    };

    println!(
        "{} | {} [{}] | {} bytes | entropy {:.2} | {} us",
        report.file_name,
        report.detected.name,
        report.detected.id,
        report.file_size,
        report.hashes.entropy_bits,
        report.parse_time_us
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!(
            "Universal File Opener (UFO) CLI — {} registered extensions across 23 categories",
            ufo_core::FORMAT_REGISTRY.len()
        );
        println!("Usage: ufo [--json] <file1> [file2 ...]");
        return Ok(());
    }

    if args[0] == "--json" && args.len() >= 2 {
        let path = Path::new(&args[1]);
        let bytes = std::fs::read(path)?;
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".into());
        let report = ufo_core::open_bytes(&name, &bytes, None);
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    args.par_iter().try_for_each(|p| inspect_one(Path::new(p)))?;
    Ok(())
}
