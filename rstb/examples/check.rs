//! Checks the crate against an unpacked romfs and its own table:
//! `cargo run --release --example check -- <romfs>`.
//! Prints how every calculated size compares with the table's entry, and the time spent per file type.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use rstb::sizing::{table_name, Sizer, GAME_SIZED, SHADER_ARCHIVE, UNSUPPORTED};
use rstb::table::{self, Table};
use rstb::yaz0;

fn files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(root, &path, out);
        } else {
            out.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
        }
    }
}

fn main() {
    let root = std::env::args().nth(1).expect("usage: check <romfs>");
    let root = Path::new(&root);
    let file = std::fs::read(root.join(table::PATH)).unwrap();
    let data = yaz0::decompress(&file).unwrap();
    let table = Table::parse(&data).unwrap();
    assert_eq!(table.serialize(), data, "table round-trip");
    let start = Instant::now();
    let compressed = yaz0::compress(&data);
    println!(
        "Table: {} unique, {} collided; Yaz0 {} -> {} bytes in {:?} (vanilla {})",
        table.unique.len(),
        table.collided.len(),
        data.len(),
        compressed.len(),
        start.elapsed(),
        file.len()
    );
    assert_eq!(yaz0::decompress(&compressed).unwrap(), data, "Yaz0 round-trip");

    let mut all = Vec::new();
    files(root, root, &mut all);
    all.sort();
    let mut sizer = Sizer::new(root.join(SHADER_ARCHIVE).to_string_lossy());
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut times: BTreeMap<String, (usize, Duration)> = BTreeMap::new();
    let mut slowest = (String::new(), Duration::ZERO);
    let start = Instant::now();
    for relative in &all {
        let Some(name) = table_name(relative) else { continue };
        let path = root.join(relative).to_string_lossy().into_owned();
        let started = Instant::now();
        let size = sizer.file_size(&path, name);
        let elapsed = started.elapsed();
        let kind = if name.starts_with("Model/") && !name.starts_with("Model/Layout_") {
            "model archive"
        } else {
            relative.rsplit('.').next().unwrap_or("")
        };
        let time = times.entry(kind.to_string()).or_default();
        time.0 += 1;
        time.1 += elapsed;
        if elapsed > slowest.1 {
            slowest = (relative.clone(), elapsed);
        }
        let vanilla = table.get(name).map(|e| e.size as i64);
        let outcome = match (size, vanilla) {
            (Err(e), _) => format!("error: {e}"),
            (Ok(UNSUPPORTED), _) => "unsupported".into(),
            (Ok(GAME_SIZED), None) => "game sized, no entry".into(),
            (Ok(GAME_SIZED), Some(_)) => "game sized, has entry".into(),
            (Ok(_), None) => "sized, no entry".into(),
            (Ok(s), Some(v)) if s == v => "exact".into(),
            (Ok(s), Some(v)) if s > v => "over".into(),
            (Ok(s), Some(v)) => {
                println!("UNDER {name}: {s} < {v}");
                "UNDER".into()
            }
        };
        *counts.entry(outcome).or_default() += 1;
    }
    println!("Sized {} files in {:?}", all.len(), start.elapsed());
    let mut times: Vec<_> = times.into_iter().collect();
    times.sort_by_key(|(_, (_, time))| std::cmp::Reverse(*time));
    for (kind, (count, time)) in times.iter().take(8) {
        println!("{time:>12.2?}  {count:>6}  {kind}");
    }
    println!("Slowest: {} in {:?}", slowest.0, slowest.1);
    if let Some(error) = sizer.shader_error() {
        println!("Shader archive failed: {error}");
    }
    for (outcome, count) in counts {
        println!("{count:>7}  {outcome}");
    }
}
