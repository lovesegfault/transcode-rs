use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{self, StreamExt};
use indicatif::ProgressStyle;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::sync::{Mutex, OnceCell};
use tokio::{process::Command, task::spawn_blocking};
use tracing::{debug, error, info, info_span, warn, Span};
use tracing_indicatif::{span_ext::IndicatifSpanExt, IndicatifLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use walkdir::WalkDir;

const MEDIAINFO: &str = env!("MEDIAINFO_PATH");
const FFMPEG: &str = env!("FFMPEG_PATH");

static ENCODER_LOCK: Mutex<()> = Mutex::const_new(());
static DRY_RUN: OnceCell<bool> = OnceCell::const_new();

#[derive(Parser)]
struct Args {
    #[arg(long)]
    dry_run: bool,
    media: Vec<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let indicatif_layer = IndicatifLayer::new();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(indicatif_layer.get_stderr_writer()))
        .with(indicatif_layer)
        .init();

    let args = Args::parse();

    DRY_RUN.get_or_init(|| async move { args.dry_run }).await;

    info!("Discovering files to transcode...");
    let walker = args
        .media
        .into_iter()
        .map(|p| WalkDir::new(p).into_iter())
        .flatten();

    let files = spawn_blocking(move || {
        let mut files: Vec<PathBuf> = Vec::with_capacity(walker.size_hint().0);
        let video_exts = [
            ".avi", ".flv", ".m4v", ".mkv", ".mov", ".mp4", ".mpg", ".ts", ".webm", ".wmv",
        ];
        for entry in walker {
            let Ok(entry) = entry else {
                warn!("skipping entry: {entry:?}");
                continue;
            };
            let path = entry.path();
            if entry.path_is_symlink() {
                debug!("skipping symlink: '{}'", path.display());
                continue;
            }
            if path.is_dir() {
                debug!("skipping directory: '{}'", path.display());
                continue;
            }
            let Some(ext) = path.extension() else {
                debug!("skipping extensionless path: '{}'", path.display());
                continue;
            };
            if !video_exts.iter().any(|&e| e == ext) {
                debug!(ext=%ext.to_string_lossy(), "skipping non-video file '{}'", path.display());
                continue;
            }

            debug!("queueing video file '{}'", path.display());
            files.push(path.to_path_buf());
        }
        anyhow::Ok(files)
    })
    .await??;
    let media_count = files.len();
    info!("Found {media_count} files to transcode");

    let header_span = info_span!("header");
    header_span.pb_set_style(&ProgressStyle::default_bar());
    header_span.pb_set_length(media_count as u64);
    let header_span_enter = header_span.enter();

    stream::iter(files.into_iter())
        .filter_map(|p| async move {
            let media = VideoFile::new(&p)
                .await
                .map_err(|e| {
                    error!(path=%p.display(), "failed to parse video metadata: {e:?}");
                    e
                })
                .ok()?;
            let Some(video_md) = media.metadata.video_info() else {
                warn!("No video metadata in video file");
                return None;
            };

            if video_md.codec_id == "av01" {
                info!(path=%p.display(), "Skipping AV1 file");
                return None;
            }

            if video_md.codec_id == "hvc1" {
                info!(path=%p.display(), "Skipping HEVC file");
                return None;
            }

            info!("Enqueued '{}'", p.display());
            Some(futures::future::ready(media))
        })
        .buffer_unordered(200)
        .filter_map(|video| async move {
            if DRY_RUN.get().copied().unwrap_or(true) {
                debug!("Skipping due to dry-run");
                return None;
            }
            let transcoded = video
                .transcode_hevc_vaapi("/tmp")
                .await
                .map_err(|e| {
                    error!(path=%video.path.display(), "Failed to transcode: {e:?}");
                    e
                })
                .ok()?;
            Some(futures::future::ready((video, transcoded)))
        })
        .buffer_unordered(200)
        .for_each_concurrent(None, |(original, transcoded)| async move {
            let Some(transcoded_md) = transcoded.metadata.video_info() else {
                error!(path = %transcoded.path.display(), "Transcoded file has no video metadata");
                tokio::fs::remove_file(&transcoded.path).await.ok();
                return;
            };
            if transcoded_md.codec_id != "hvc1" {
                error!(path = %transcoded.path.display(), "Transcoded file is not HEVC");
                tokio::fs::remove_file(&transcoded.path).await.ok();
                return;
            }
            info!(original=%original.path.display(), transcode=%transcoded.path.display(), "Replacing original with transcode");
            let final_path = original.path.with_file_name(transcoded.path.file_name().expect("transcoded file always has a file name"));
            match tokio::fs::copy(&transcoded.path, &final_path).await {
                Ok(_) => {
                    tokio::fs::remove_file(&transcoded.path).await.ok();
                },
                Err(e) => {
                    error!("Failed to copy transcoded file: {e:?}");
                    tokio::fs::remove_file(&transcoded.path).await.ok();
                },
            }

            match VideoFile::new(&final_path).await {
                Ok(final_video) => {
                    info!("Successfully transcoded '{}'", final_video.path.display());
                    tokio::fs::remove_file(&original.path).await.ok();
                },
                Err(e) => {
                    error!("Final transcode metadata error: {e:?}");
                    tokio::fs::remove_file(&final_path).await.ok();
                }
            }

            Span::current().pb_inc(1);

        })
        .await;

    std::mem::drop(header_span_enter);
    std::mem::drop(header_span);
    Ok(())
}

#[derive(Deserialize, Debug)]
struct MediaInfo(Vec<TrackInfo>);

impl MediaInfo {
    async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        let mut mediainfo_cmd = Command::new(MEDIAINFO);
        mediainfo_cmd.arg("--Output=Json");
        mediainfo_cmd.arg(&path);

        let output = mediainfo_cmd.output().await.context("run mediainfo")?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to get mediainfo for '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let metadata: MediaInfo =
            serde_json::from_slice(&output.stdout).context("parse mediainfo")?;
        Ok(metadata)
    }

    fn video_info(&self) -> Option<&VideoInfo> {
        self.0.iter().find_map(|md| match md {
            TrackInfo::Container(_) => None,
            TrackInfo::Video(vmd) => Some(vmd),
        })
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
enum TrackInfo {
    Container(ContainerInfo),
    Video(VideoInfo),
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct ContainerInfo {
    #[serde(rename = "@type")]
    track_type: String,
    video_count: usize,
    audio_count: usize,
    file_extension: String,
    format: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct VideoInfo {
    #[serde(rename = "@type")]
    track_type: String,
    stream_order: String,
    format: String,
    codec_id: String,
    width: usize,
    height: usize,
    color_space: String,
}

struct VideoFile {
    path: PathBuf,
    metadata: MediaInfo,
}

impl VideoFile {
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let metadata = MediaInfo::new(&path).await?;

        Ok(Self { path, metadata })
    }

    #[tracing::instrument(skip_all, fields(path = %self.path.display()))]
    pub async fn transcode_hevc_vaapi(&self, out_dir: impl AsRef<Path>) -> Result<Self> {
        let file_name = self
            .path
            .file_name()
            .context("video file has no file name")?;
        let transcoded_path = out_dir.as_ref().join(file_name).with_extension("mkv");

        let mut cmd = Command::new(FFMPEG);

        #[rustfmt::skip]
        cmd.args([
            "-y",
            "-hwaccel", "vaapi",
            "-hwaccel_device", "/dev/dri/renderD128",
            "-hwaccel_output_format", "vaapi",
        ]);

        cmd.arg("-i").arg(&self.path);

        #[rustfmt::skip]
        cmd.args([
            "-f", "matroska",
            "-c:a", "copy",
            "-crf", "20",
            "-vf", "scale_vaapi=format=p010",
            "-c:v", "hevc_vaapi",
            "-c:s", "copy"
        ]);

        cmd.arg(&transcoded_path);

        let _lock = ENCODER_LOCK.lock();
        info!("Transcoding");

        let output = cmd.output().await.context("run ffmpeg transcode")?;
        if !output.status.success() {
            tokio::fs::remove_file(&transcoded_path).await.ok();
            anyhow::bail!(
                "transcode failed: {}",
                &String::from_utf8_lossy(&output.stderr)
            );
        }
        info!("Done");
        drop(_lock);

        let transcoded = Self::new(transcoded_path)
            .await
            .context("parse transcoded file")?;

        Ok(transcoded)
    }
}
