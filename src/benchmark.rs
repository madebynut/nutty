//! Repeatable CPU measurements for parsing and viewport extraction, without opening a window.

use crate::{config::Preferences, terminal::Terminal};
use anyhow::{Result, ensure};
use std::{hint::black_box, time::Instant};

pub(crate) fn run() -> Result<()> {
    let preferences = Preferences::default();
    let (terminal, _) = Terminal::channel(&preferences, "benchmark".into());
    let line = "\x1b[38;2;105;190;160mNutty\x1b[0m 中文 terminal output 0123456789\r\n";
    let block = line.repeat(128);
    let iterations = 2048;
    let bytes = block.len() * iterations;
    let start = Instant::now();
    for _ in 0..iterations {
        terminal.engine.feed(black_box(block.as_bytes()));
    }
    let parse = start.elapsed();
    terminal.engine.feed(b"NUTTY_BENCHMARK_COMPLETE");
    let snapshot = terminal.engine.snapshot();
    ensure!(
        snapshot
            .cells
            .iter()
            .map(|cell| cell.cell.c)
            .collect::<String>()
            .contains("NUTTY_BENCHMARK_COMPLETE"),
        "parser lost the final output"
    );
    let start = Instant::now();
    for _ in 0..10_000 {
        black_box(terminal.engine.snapshot());
    }
    let snapshot = start.elapsed();
    println!(
        "Alacritty parse: {:.1} MiB/s ({:.1} MiB, {:.3}s)",
        bytes as f64 / (1024. * 1024.) / parse.as_secs_f64(),
        bytes as f64 / (1024. * 1024.),
        parse.as_secs_f64()
    );
    println!(
        "Viewport extraction: {:.1} µs/snapshot (100 × 30 cells)",
        snapshot.as_secs_f64() * 1_000_000. / 10_000.
    );
    println!("CPU measurements only; GPU frame rate and input latency need native profiling.");
    Ok(())
}
