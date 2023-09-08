use anyhow::{Context, Result};
use async_tempfile::TempFile;
use bytesize::ByteSize;
use clap::Parser;
use ffmpeg_next as ffmpeg;
use futures::{prelude::*, StreamExt};
use indicatif::{ProgressState, ProgressStyle};
use par_stream::prelude::*;
use std::path::{Path, PathBuf};
use std::{pin::Pin, time::Duration};
use tokio::{
    process::Command,
    sync::OnceCell,
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, error, info, info_span, trace, warn, Instrument, Span};
use tracing_indicatif::{span_ext::IndicatifSpanExt, IndicatifLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use walkdir::WalkDir;

const FFMPEG: &str = env!("FFMPEG_PATH");
const TRANSCODE_THRESHOLD: f64 = 0.6;

static DRY_RUN: OnceCell<bool> = OnceCell::const_new();

#[derive(Parser)]
struct Args {
    #[arg(long)]
    dry_run: bool,
    media: Vec<PathBuf>,
}

fn elapsed_subsec(state: &ProgressState, writer: &mut dyn std::fmt::Write) {
    // trim micro/nano secs
    let elapsed = Duration::from_secs(state.elapsed().as_secs());
    let _ = writer.write_str(&humantime::format_duration(elapsed).to_string());
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let indicatif_layer = IndicatifLayer::new().with_progress_style(
        ProgressStyle::with_template(
            "{color_start}{span_child_prefix}{span_fields} -- {span_name} {wide_msg} {elapsed_subsec}{color_end}",
        )
        .unwrap()
        .with_key(
            "elapsed_subsec",
            elapsed_subsec,
        )
        .with_key(
            "color_start",
            |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                let elapsed = state.elapsed();

                if elapsed > Duration::from_secs(30 * 60) {
                    // Red
                    let _ = write!(writer, "\x1b[{}m", 1 + 30);
                } else if elapsed > Duration::from_secs(15 * 60) {
                    // Yellow
                    let _ = write!(writer, "\x1b[{}m", 3 + 30);
                }
            },
        )
        .with_key(
            "color_end",
            |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                if state.elapsed() > Duration::from_secs(4) {
                    let _ =write!(writer, "\x1b[0m");
                }
            },
        ),
    ).with_span_child_prefix_symbol("↳ ").with_span_child_prefix_indent(" ");
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(indicatif_layer.get_stderr_writer()))
        .with(indicatif_layer)
        .init();

    let args = Args::parse();

    ffmpeg::init().context("initialize ffmpeg")?;

    DRY_RUN.get_or_init(|| async move { args.dry_run }).await;

    let pb_span = info_span!("transcode");
    pb_span.pb_set_style(&ProgressStyle::default_bar());
    pb_span.pb_set_length(1);

    let pb_span_transcoder = pb_span.clone();
    let pb_span_finder = pb_span.clone();

    let _pb_span_entered = pb_span.enter();

    let (send, recv) = async_priority_channel::unbounded();
    let mut tasks = JoinSet::new();

    tasks.spawn_blocking(move || {
        args.media
            .into_iter()
            .map(WalkDir::new)
            .flat_map(IntoIterator::into_iter)
            .for_each(|entry| {
                let Ok(entry) = entry else {
                    warn!("skipping entry: {entry:?}");
                    return;
                };
                let path = entry.path().to_path_buf();
                if entry.path_is_symlink() {
                    // trace!("skipping symlink: '{}'", path.display());
                    return;
                }
                if path.is_dir() {
                    // trace!("skipping directory: '{}'", path.display());
                    return;
                }
                let Some(ext) = path.extension() else {
                    // trace!("skipping extensionless path: '{}'", path.display());
                    return;
                };
                let video_exts = [
                    "avi", "flv", "m4v", "mkv", "mov", "mp4", "mpg", "ts", "webm", "wmv",
                ];
                if !video_exts.iter().any(|&e| e == ext) {
                    // trace!(ext=%ext.to_string_lossy(), "skipping non-video file '{}'", path.display());
                    return;
                }

                let priority = std::fs::metadata(&path).map(|md| md.len()).unwrap_or(0);

                if let Err(e) = send.try_send(path, priority) {
                    error!("skipped file due to channel error: {e:?}");
                    return;
                };
                pb_span_finder.pb_inc_length(1);
            });
    });

    tasks.spawn(async move {
        PriorityReceiverStream::new(recv)
            .map(move |(path, _prio)| (pb_span_transcoder.clone(), path))
            .then(|(span, path)| async move {
                tokio::spawn(async move {
                    let video = VideoFile::new(&path)
                        .await
                        .map_err(|e| {
                            span.pb_inc(1);
                            error!(path = %path.display(), "failed to get video info: {e:?}");
                        })
                        .ok()?;

                    use ffmpeg::codec::Id;
                    match video.codec {
                        Id::AV1 | Id::HEVC => {
                            debug!(path=%path.display(), "skipping {:?} video", video.codec);
                            span.pb_inc(1);
                            return None;
                        }
                        _ => {
                            info!(path=%path.display(), "enqueuing {:?} video", video.codec);
                        }
                    };

                    Some((span, video))
                })
            })
            .buffered(100)
            .filter_map(|opt| async move { opt.ok().flatten() })
            .par_then(0.25, |(span, video)| async move {
                if DRY_RUN.get().copied().unwrap_or(true) {
                    trace!("Skipping due to dry-run");
                    return anyhow::Ok(None);
                }
                let original_size = tokio::fs::metadata(&video.path)
                    .await
                    .context("read original metadata")?
                    .len();
                let transcode_threshold = (original_size as f64) * TRANSCODE_THRESHOLD;

                let start_time = tokio::time::Instant::now();
                let (transcoded_path, transcode_task) =
                    video.transcode_x265("/tmp").instrument(span.clone()).await;

                loop {
                    // wait for file to show up initially
                    if !transcoded_path.exists() {
                        if start_time.elapsed() > Duration::from_secs(5) {
                            // this is way too long, something isn't right, avoid infinite loops
                            anyhow::bail!(
                                "Transcode file never showed up: '{}'",
                                transcoded_path.display()
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    // check the size of the transcoded file
                    let transcoded_size = tokio::fs::metadata(&transcoded_path).await?.len();
                    if (transcoded_size as f64) > transcode_threshold {
                        warn!(
                            path=%video.path.display(),
                            threshold=ByteSize::b(transcode_threshold as u64).to_string_as(true),
                            size=ByteSize::b(transcoded_size).to_string_as(true),
                            "Aborting transcode"
                        );
                        transcode_task.abort();
                        tokio::fs::remove_file(&transcoded_path).await?;
                        return Ok(None);
                    }
                    if transcode_task.is_finished() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }

                if let Err(e) = transcode_task.await.context("transcode task")? {
                    tokio::fs::remove_file(&transcoded_path).await?;
                    return Err(e);
                };

                let transcoded =
                    TempFile::from_existing(transcoded_path, async_tempfile::Ownership::Owned)
                        .await
                        .context("wrap transcoded in tempfile")?;

                info!(
                    "Successfully transcoded: {}",
                    transcoded.file_path().display()
                );
                Ok(Some((span, video, transcoded)))
            })
            .filter_map(
                |res: Result<Option<(Span, VideoFile, TempFile)>>| async move {
                    match res {
                        Ok(inner) => inner,
                        Err(e) => {
                            error!("{e:?}");
                            None
                        }
                    }
                },
            )
            .par_then_unordered(None, |(span, original, transcode)| async move {
                let (transcode_codec, transcode_frames) = VideoFile::new(transcode.file_path())
                    .await
                    .map(|vf| (vf.codec, vf.frames))
                    .context("get transcode info")?;

                if transcode_codec != ffmpeg::codec::Id::HEVC {
                    anyhow::bail!(
                        "Transcode was '{transcode_codec:?}' not HEVC: '{}'",
                        transcode.file_path().display()
                    );
                }

                let frame_diff_threshold = ((original.frames as f64) * 0.1) as u64;
                let frame_diff = original.frames.saturating_sub(transcode_frames);
                if frame_diff > frame_diff_threshold {
                    warn!(
                        original = original.frames,
                        transcode = transcode_frames,
                        "Ignoring transcode due to frame count difference > 1%"
                    );
                    span.pb_inc(1);
                    return Ok(None);
                }

                Ok(Some((span, original, transcode)))
            })
            .filter_map(|res| async move {
                match res {
                    Ok(inner) => inner,
                    Err(e) => {
                        error!("{e:?}");
                        None
                    }
                }
            })
            .par_for_each(None, |(span, original, transcode)| async move {
                info!(
                    original=%original.path.display(),
                    transcode=%transcode.file_path().display(),
                    "replacing original with transcode"
                );
                let final_path = original.path.with_file_name(
                    transcode
                        .file_path()
                        .file_name()
                        .expect("transcoded file always has a file name"),
                );
                match tokio::fs::copy(&transcode.file_path(), &final_path).await {
                    Ok(_) => {
                        tokio::fs::remove_file(&original.path).await.ok();
                    }
                    Err(e) => {
                        error!("Failed to copy transcoded file: {e:?}");
                        return;
                    }
                }
                span.pb_inc(1)
            })
            .await;
    });

    while let Some(res) = tasks.join_next().await {
        res?;
    }

    drop(_pb_span_entered);
    Ok(())
}

struct PriorityReceiverStream<I, P: Ord>(async_priority_channel::Receiver<I, P>);

impl<I, P: Ord> PriorityReceiverStream<I, P> {
    fn new(recv: async_priority_channel::Receiver<I, P>) -> Self {
        Self(recv)
    }
}

// I think this sucks?
impl<I: Send, P: Ord + Send> Stream for PriorityReceiverStream<I, P> {
    type Item = (I, P);
    fn poll_next(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            // Attempt to receive a message.
            match self.0.try_recv() {
                Ok(msg) => {
                    // The stream is not blocked on an event - drop the listener.
                    return std::task::Poll::Ready(Some(msg));
                }
                Err(async_priority_channel::TryRecvError::Closed) => {
                    // The stream is not blocked on an event - drop the listener.
                    return std::task::Poll::Ready(None);
                }
                Err(async_priority_channel::TryRecvError::Empty) => {}
            }
        }
    }
}

struct VideoFile {
    path: PathBuf,
    codec: ffmpeg::codec::Id,
    frames: u64,
}

impl VideoFile {
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let (path, codec, frames) = tokio::task::spawn_blocking(move || {
            let ctx = ffmpeg::format::input(&path).context("ffmpeg input")?;

            let Some(video_stream) = ctx.streams().best(ffmpeg::media::Type::Video) else {
                return anyhow::Ok(None);
            };

            let frames = video_stream.frames();
            anyhow::ensure!(frames > 0, "Non-natural frame count, rejecting file");
            let frames = frames as u64;

            let codec = ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())
                .context("codec context from parameters")?;
            let codec = codec.id();

            Ok(Some((path, codec, frames)))
        })
        .await
        .context("spawn ffmpeg format analysis")?
        .context("ffmpeg analysis")?
        .context("no video stream in file")?;

        Ok(Self {
            path,
            codec,
            frames,
        })
    }

    #[tracing::instrument(skip_all, fields(path = %self.path.display()))]
    pub async fn transcode_x265(
        &self,
        out_dir: impl AsRef<Path>,
    ) -> (PathBuf, JoinHandle<Result<()>>) {
        let out_dir = out_dir.as_ref().to_path_buf();
        let file_name = self.path.file_name().expect("video file has no file name");
        let transcoded_path = out_dir.join(file_name).with_extension("mkv");

        let mut cmd = Command::new(FFMPEG);

        #[rustfmt::skip]
        cmd.args([
            "-y",
            "-threads", "0",
            "-hwaccel", "vaapi",
            "-hwaccel_device", "/dev/dri/renderD128",
        ]);

        cmd.arg("-i").arg(&self.path);

        #[rustfmt::skip]
        cmd.args([
            "-f", "matroska",
            "-c:v", "libx265",
            "-crf", "25",
            "-preset", "medium",
            "-c:a", "copy",
            "-c:s", "copy"
        ]);

        cmd.arg(&transcoded_path);

        cmd.kill_on_drop(true);

        trace!(ffmpeg_cmd=?cmd);

        let log_path = transcoded_path.with_extension("log");
        let task = tokio::spawn(
            async move {
                let log_file = tokio::fs::File::options()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .await
                    .context("open ffmpeg log")?;
                debug!("writing ffmpeg output to: '{}", log_path.display());

                let stdout = log_file
                    .try_clone()
                    .await
                    .context("clone log file as stdout")?
                    .into_std()
                    .await;
                let stderr = log_file
                    .try_clone()
                    .await
                    .context("clone log file as stderr")?
                    .into_std()
                    .await;

                trace!("starting");
                let mut child = cmd
                    .stdin(std::process::Stdio::null())
                    .stdout(stdout)
                    .stderr(stderr)
                    .spawn()
                    .context("spawn ffmpeg transcode")?;

                let status = child.wait().await.context("run ffmpeg transcode")?;
                trace!("finished");

                if !status.success() {
                    anyhow::bail!("transcode failed, stderr: '{}'", log_path.display());
                }

                drop(log_file);
                tokio::fs::remove_file(&log_path).await.ok();

                Ok(())
            }
            .instrument(Span::current()),
        );

        (transcoded_path, task)
    }
}
