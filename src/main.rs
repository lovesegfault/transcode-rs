use anyhow::{Context, Result};
use async_tempfile::TempFile;
use bytesize::ByteSize;
use clap::Parser;
use futures::{prelude::*, StreamExt};
use indicatif::{ProgressState, ProgressStyle};
use par_stream::prelude::*;
use serde::Deserialize;
use serde_aux::prelude::*;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};
use tokio::task::{JoinHandle, JoinSet};
use tokio::{process::Command, task::spawn_blocking};
use tracing::{debug, error, info, info_span, warn, Instrument, Span};
use tracing_indicatif::{span_ext::IndicatifSpanExt, IndicatifLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use walkdir::WalkDir;

const MEDIAINFO: &str = env!("MEDIAINFO_PATH");
const FFMPEG: &str = env!("FFMPEG_PATH");
const TRANSCODE_THRESHOLD: f64 = 0.6;

static ENCODER_LOCK: Mutex<()> = Mutex::const_new(());
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

    DRY_RUN.get_or_init(|| async move { args.dry_run }).await;

    let pb_span = info_span!("transcode");
    pb_span.pb_set_style(&ProgressStyle::default_bar());
    pb_span.pb_set_length(1);
    let pb_span_clone = pb_span.clone();
    let _pb_span_entered = pb_span.enter();

    let (send, recv) = async_priority_channel::unbounded();
    let mut tasks = JoinSet::new();

    tasks.spawn(async move {
        BlockingStream::new(
            args.media
                .into_iter()
                .map(|p| WalkDir::new(p).into_iter())
                .flatten(),
        )
        .map(move |entry| (send.clone(), pb_span_clone.clone(), entry))
        .for_each_concurrent(None, |(send, span, entry)| async move {
            let Ok(entry) = entry else {
                warn!("skipping entry: {entry:?}");
                return;
            };
            let path = entry.path();
            if entry.path_is_symlink() {
                debug!("skipping symlink: '{}'", path.display());
                return;
            }
            if path.is_dir() {
                debug!("skipping directory: '{}'", path.display());
                return;
            }
            let Some(ext) = path.extension() else {
                debug!("skipping extensionless path: '{}'", path.display());
                return;
            };
            let video_exts = [
                "avi", "flv", "m4v", "mkv", "mov", "mp4", "mpg", "ts", "webm", "wmv",
            ];
            if !video_exts.iter().any(|&e| e == ext) {
                debug!(ext=%ext.to_string_lossy(), "skipping non-video file '{}'", path.display());
                return;
            }

            let Ok(media) = VideoFile::new(&path).await.map_err(|e| {
                error!(path=%path.display(), "failed to parse video metadata: {e:?}");
                e
            }) else {
                return;
            };
            let Some(video_md) = media.metadata.video_info() else {
                warn!("No video metadata in video file");
                return;
            };

            if video_md.format == "AV1" {
                info!(path=%path.display(), "Skipping AV1 file");
                span.pb_inc(1);
                return;
            }

            if video_md.format == "HEVC" {
                info!(path=%path.display(), "Skipping HEVC file");
                span.pb_inc(1);
                return;
            }

            info!("Enqueued '{}'", path.display());
            span.pb_inc_length(1);
            let prio = media.metadata.file_size().unwrap_or_default();
            send.send((span, media), prio).await.unwrap();
        })
        .await;
    });

    tasks.spawn(async move {
        PriorityReceiverStream::new(recv)
            .map(|(inner, _prio)| inner)
            .then(|(span, video)| async move {
                if DRY_RUN.get().copied().unwrap_or(true) {
                    debug!("Skipping due to dry-run");
                    return anyhow::Ok(None);
                }
                let original_size = tokio::fs::metadata(&video.path)
                    .await
                    .context("read original metadata")?
                    .len();
                let transcode_threshold = (original_size as f64) * TRANSCODE_THRESHOLD;

                let start_time = tokio::time::Instant::now();
                let (transcoded_path, transcode_task) = video
                    .transcode_hevc_vaapi("/tmp")
                    .instrument(span.clone())
                    .await;

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
                        .context("wrap transcoded in tempfile");

                transcoded.map(|ts| Some((span, video, ts)))
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
                let transcode_info = MediaInfo::new(transcode.file_path())
                    .await
                    .context("read transcode mediainfo")?;
                let transcode_info = transcode_info
                    .video_info()
                    .context("transcode has no video track")?;
                if transcode_info.format != "HEVC" {
                    anyhow::bail!(
                        "Transcode was not HEVC: '{}'",
                        transcode.file_path().display()
                    );
                }
                let original_info = original
                    .metadata
                    .video_info()
                    .context("original has no video track")?;
                let duration_diff =
                    100.0 - (100.0 * (transcode_info.duration / original_info.duration));
                if duration_diff > 5.0 {
                    warn!(
                        original = humantime::format_duration(Duration::from_secs_f64(
                            original_info.duration
                        ))
                        .to_string(),
                        transcode = humantime::format_duration(Duration::from_secs_f64(
                            transcode_info.duration
                        ))
                        .to_string(),
                        "Ignoring transcode due to duration difference of {duration_diff:.2}%"
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
                debug!(
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

struct BlockingStream<N, I>(BlockingState<N, I>);

enum BlockingState<N, I> {
    Idle(Option<I>),
    Busy(tokio::task::JoinHandle<(I, Option<N>)>),
}

impl<N, I> BlockingStream<N, I> {
    fn new(inner: I) -> Self
    where
        I: Iterator<Item = N>,
    {
        BlockingStream(BlockingState::Idle(Some(inner)))
    }
}

impl<N: Send + 'static, I: Iterator<Item = N> + Send + Unpin + 'static> Stream
    for BlockingStream<N, I>
{
    type Item = <I as Iterator>::Item;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>>
    where
        <I as Iterator>::Item: Send,
    {
        loop {
            match &mut self.0 {
                BlockingState::Idle(opt) => {
                    let mut inner = opt.take().unwrap();
                    self.0 = BlockingState::Busy(spawn_blocking(move || {
                        let next = inner.next();
                        (inner, next)
                    }));
                }
                BlockingState::Busy(task) => {
                    let (inner, opt) = futures::ready!(Pin::new(task).poll(cx))
                        .context("walkdir blocking task")
                        .unwrap();
                    self.0 = BlockingState::Idle(Some(inner));
                    return std::task::Poll::Ready(opt);
                }
            }
        }
    }
}

#[derive(Deserialize, Debug, PartialEq)]
struct MediaInfo(Vec<TrackInfo>);

impl MediaInfo {
    async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        let mut mediainfo_cmd = Command::new(MEDIAINFO);
        mediainfo_cmd.arg("--Output=JSON");
        mediainfo_cmd.arg(&path);

        let output = mediainfo_cmd.output().await.context("run mediainfo")?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to get mediainfo for '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("parse mediainfo")?;
        let tracks = json
            .get("media")
            .and_then(|m| m.get("track"))
            .cloned()
            .context("get track info from mediainfo")?;
        let metadata: MediaInfo = serde_json::from_value(tracks).context("parse track info")?;

        Ok(metadata)
    }

    fn video_info(&self) -> Option<&VideoInfo> {
        self.0.iter().find_map(|md| match md {
            TrackInfo::General(_) => None,
            TrackInfo::Video(vmd) => Some(vmd),
            TrackInfo::Unknown => None,
        })
    }

    fn general_info(&self) -> Option<&GeneralInfo> {
        self.0.iter().find_map(|md| match md {
            TrackInfo::General(gmd) => Some(gmd),
            TrackInfo::Video(_) => None,
            TrackInfo::Unknown => None,
        })
    }

    fn file_size(&self) -> Option<usize> {
        self.general_info().map(|info| info.file_size)
    }
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "@type")]
enum TrackInfo {
    General(GeneralInfo),
    Video(VideoInfo),
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct GeneralInfo {
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    video_count: usize,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    audio_count: usize,
    file_extension: String,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    file_size: usize,
    format: String,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct VideoInfo {
    format: String,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    width: usize,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    height: usize,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    duration: f64,
}

struct VideoFile {
    path: PathBuf,
    metadata: MediaInfo,
}

impl PartialEq for VideoFile {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for VideoFile {}

// ord by size
impl PartialOrd for VideoFile {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let self_sz = self.metadata.general_info()?.file_size;
        let other_sz = other.metadata.general_info()?.file_size;
        self_sz.partial_cmp(&other_sz)
    }
}

impl Ord for VideoFile {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.partial_cmp(other) {
            Some(ord) => ord,
            None => std::cmp::Ordering::Equal,
        }
    }
}

impl VideoFile {
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let metadata = MediaInfo::new(&path).await?;

        Ok(Self { path, metadata })
    }

    #[tracing::instrument(skip_all, fields(path = %self.path.display()))]
    pub async fn transcode_hevc_vaapi(
        &self,
        out_dir: impl AsRef<Path>,
    ) -> (PathBuf, JoinHandle<Result<()>>) {
        let file_name = self.path.file_name().expect("video file has no file name");
        let transcoded_path = out_dir.as_ref().join(file_name).with_extension("mkv");

        let mut cmd = Command::new(FFMPEG);

        cmd.arg("-i").arg(&self.path);

        #[rustfmt::skip]
        cmd.args([
            "-y",
            "-threads", "0",
            "-c:v", "libx265",
            "-preset", "medium",
            "-f", "matroska",
            "-c:a", "copy",
            "-c:s", "copy"
        ]);

        cmd.arg(&transcoded_path);

        cmd.kill_on_drop(true);

        debug!(ffmpeg_cmd=?cmd);

        let task = tokio::spawn(
            async move {
                let _lock = ENCODER_LOCK.lock();
                debug!("starting ffmpeg transcode");

                let ffmpeg_log = TempFile::new_with_name("ffmpeg.log")
                    .await
                    .context("create tempfile for ffmpeg output")?;
                debug!(
                    "writing ffmpeg output to: '{}",
                    ffmpeg_log.file_path().display()
                );

                let log_path = ffmpeg_log.file_path();
                let stdout = std::process::Stdio::from(std::fs::File::open(log_path)?);
                let stderr = std::process::Stdio::from(std::fs::File::open(log_path)?);

                let mut child = cmd
                    .stdin(std::process::Stdio::null())
                    .stdout(stdout)
                    .stderr(stderr)
                    .spawn()
                    .context("spawn ffmpeg transcode")?;

                let status = child.wait().await.context("run ffmpeg transcode")?;

                if !status.success() {
                    // leave the encoder log
                    let log_path = log_path.clone();
                    std::mem::forget(ffmpeg_log);
                    anyhow::bail!("transcode failed, stderr: '{}'", log_path.display());
                }
                debug!("finished");
                Ok(())
            }
            .instrument(Span::current()),
        );

        (transcoded_path, task)
    }
}
