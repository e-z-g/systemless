//! Dump what Systemless's HFS reader sees in a disk image, as JSON.
//!
//! `cargo run --example hfs_dump --no-default-features -- <image.dsk>`
//!
//! This exists so that a writer of HFS volumes living outside this repository
//! can be checked against the reader in `src/disk_image/hfs.rs`: build an
//! image, read it back here, and compare the file list, the Finder types and
//! both forks of every file against what went in. The reader is the
//! interesting half -- it walks the catalog's leaf chain and the extents
//! overflow tree independently of whatever produced the bytes.
//!
//! The output is a single JSON object on stdout; forks are hex so the
//! comparison can be exact without worrying about encodings.

use std::process::ExitCode;

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: hfs_dump <image>");
        return ExitCode::FAILURE;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let contents = match systemless::disk_image::extract_dc42_or_hfs(&bytes) {
        Ok(Some(contents)) => contents,
        Ok(None) => {
            eprintln!("{path} does not look like a DC42 or HFS image");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let info = &contents.volume_info;
    println!("{{");
    println!("  \"volumeName\": {},", quote(&contents.volume_name));
    println!(
        "  \"volume\": {{\"attributes\": {}, \"fileCount\": {}, \"allocationBlockCount\": {}, \
\"allocationBlockSize\": {}, \"freeBlocks\": {}, \"bitmapStart\": {}, \"allocationStart\": {}, \
\"nextCatalogId\": {}}},",
        info.attributes,
        info.file_count,
        info.allocation_block_count,
        info.allocation_block_size,
        info.free_blocks,
        info.bitmap_start,
        info.allocation_start,
        info.next_catalog_id
    );
    let dirs: Vec<String> = contents.dirs.iter().map(|d| quote(d)).collect();
    println!("  \"dirs\": [{}],", dirs.join(", "));
    println!("  \"files\": [");
    for (index, file) in contents.files.iter().enumerate() {
        println!(
            "    {{\"path\": {}, \"type\": {}, \"creator\": {}, \"finderFlags\": {}, \
\"data\": {}, \"rsrc\": {}}}{}",
            quote(&file.path),
            quote(&String::from_utf8_lossy(&file.file_type)),
            quote(&String::from_utf8_lossy(&file.creator)),
            file.finder_flags,
            quote(&hex(&file.data)),
            quote(&hex(&file.rsrc)),
            if index + 1 == contents.files.len() { "" } else { "," }
        );
    }
    println!("  ]");
    println!("}}");
    ExitCode::SUCCESS
}
