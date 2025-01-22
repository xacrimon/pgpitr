use crate::context::Context;
use anyhow::bail;
use anyhow::Result;
use clap::Args;
use log::info;
use std::cell::Cell;
use std::cmp;
use std::fs::{self, File};
use std::io;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, channel, TryRecvError};
use std::thread;
use std::time;

#[derive(Debug, Args)]
pub(super) struct Options {
    #[arg(long)]
    pub(super) label: String,
}

pub fn run(ctx: &Context, opts: &Options) -> Result<()> {
    info!("starting backup with label {}", opts.label);

    let mut child = Command::new("pg_basebackup")
        .arg("-U")
        .arg("postgres")
        .arg("-D")
        .arg("-")
        .arg("-Ft")
        .arg("-c")
        .arg("fast")
        .arg("-Xn")
        .arg("-l")
        .arg(&opts.label)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let (mut backup_stream, rx) = Splitter::new(child.stdout.take().unwrap());
    let (label_tx, label_rx) = channel();
    thread::spawn(move || {
        let label_search = find_wal_label(rx);
        label_tx.send(label_search).unwrap();
    });

    let mut backup_buffered: Vec<u8> = Vec::new();
    let label = loop {
        match label_rx.try_recv() {
            Ok(res) => break res?,
            Err(TryRecvError::Empty) => (),
            Err(TryRecvError::Disconnected) => unreachable!(),
        }

        let mut buffer = [0; 4096];
        let len = backup_stream.read(&mut buffer)?;
        backup_buffered.extend(&buffer[..len]);

        if len == 0 {
            unreachable!();
        }
    };

    info!(
        "found wal label {} after scanning {} bytes",
        label,
        backup_buffered.len()
    );
    hex::decode(label).expect("invalid label");

    let backup_dir_path = ctx.storage.join("backups");
    if !backup_dir_path.exists() {
        fs::create_dir(&backup_dir_path)?;
    }

    let backup_target_path = backup_dir_path.join(format!("{}.tar.zst", &opts.label));
    let target_file = File::create(&backup_target_path)?;
    info!("writing backup to {:?}...", backup_target_path);

    let buffer_and_stream = backup_buffered.as_slice().chain(backup_stream);
    let total_read_bytes = Cell::new(0);
    let total_written_bytes = Cell::new(0);

    let mut tracked_reader = TrackedReader::new(buffer_and_stream, &total_read_bytes);
    let tracked_writer = TrackedWriter::new(&target_file, &total_written_bytes);
    let mut encoder = zstd::stream::write::Encoder::new(tracked_writer, 3)?;
    let start_time = time::Instant::now();
    let mut last_info = start_time;

    let unit_scale = 1024 * 1024;
    let read = || total_read_bytes.get() / unit_scale;
    let read_rate = || (read() as f32) / start_time.elapsed().as_secs_f32();
    let written = || total_written_bytes.get() / unit_scale;
    let written_rate = || (written() as f32) / start_time.elapsed().as_secs_f32();
    let ratio = || (total_read_bytes.get() as f32) / (total_written_bytes.get() as f32);

    let log_stats = |last: bool| {
        info!(
            "{}processed {} MiB @ {:.0} MiB/s, written {} MiB @ {:.0} MiB/s, compression ratio: {:.2}x",
            if !last { "progress: " } else { "" },
            read(),
            read_rate(),
            written(),
            written_rate(),
            ratio()
        );
    };

    loop {
        let chunk_size = 4 * 1024 * 1024;
        let mut chunk = tracked_reader.by_ref().take(chunk_size);
        let copied = io::copy(&mut chunk, &mut encoder)?;
        if copied == 0 {
            break;
        }

        if last_info.elapsed() >= time::Duration::from_secs(5) {
            log_stats(false);
            last_info = time::Instant::now();
        }
    }

    log_stats(true);
    info!("write finished, flushing...");
    encoder.finish()?;
    info!("syncing file...");
    target_file.sync_all()?;
    info!("completed backup");
    Ok(())
}

fn find_wal_label(stream: SplitReceiver) -> Result<String> {
    let mut archive = tar::Archive::new(stream);

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_str() == Some("backup_label") {
            let mut contents = String::new();
            entry.read_to_string(&mut contents)?;

            for line in contents.lines() {
                if line.starts_with("START WAL LOCATION") {
                    let parts: Vec<&str> = line.split("file").collect();
                    if let Some(part) = parts.get(1) {
                        if part.len() >= 1 {
                            return Ok(part[1..part.len() - 1].to_string());
                        }
                    }
                }
            }
        }
    }

    bail!("No backup label found")
}

struct TrackedReader<'tracker, R> {
    inner: R,
    total_bytes: &'tracker Cell<usize>,
}

impl<'tracker, R> TrackedReader<'tracker, R> {
    fn new(inner: R, total_bytes: &'tracker Cell<usize>) -> Self {
        Self { inner, total_bytes }
    }
}

impl<'tracker, R> Read for TrackedReader<'tracker, R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.inner.read(buf)?;
        self.total_bytes.set(self.total_bytes.get() + len);
        Ok(len)
    }
}

struct TrackedWriter<'tracker, W> {
    inner: W,
    total_bytes: &'tracker Cell<usize>,
}

impl<'tracker, W> TrackedWriter<'tracker, W> {
    fn new(inner: W, total_bytes: &'tracker Cell<usize>) -> Self {
        Self { inner, total_bytes }
    }
}

impl<'tracker, W> Write for TrackedWriter<'tracker, W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = self.inner.write(buf)?;
        self.total_bytes.set(self.total_bytes.get() + len);
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct Splitter<R> {
    inner: R,
    tx: mpsc::Sender<Vec<u8>>,
}

impl<R> Splitter<R> {
    fn new(inner: R) -> (Self, SplitReceiver) {
        let (tx, rx) = channel();
        (Self { inner, tx }, SplitReceiver::new(rx))
    }
}

impl<R> Read for Splitter<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.inner.read(buf)?;
        let _ = self.tx.send(buf[..len].to_vec());
        Ok(len)
    }
}

struct SplitReceiver {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl SplitReceiver {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            buf: Vec::new(),
        }
    }
}

impl Read for SplitReceiver {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(data) => self.buf = data,
                Err(_) => return Ok(0),
            }
        }

        let len = cmp::min(buf.len(), self.buf.len());
        buf[..len].copy_from_slice(&self.buf[..len]);
        self.buf.drain(..len);
        Ok(len)
    }
}
