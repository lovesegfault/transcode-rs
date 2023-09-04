use anyhow::{Context, Result};
use async_tempfile::TempFile;
use clap::Parser;
use futures::{prelude::*, StreamExt};
use indicatif::{ProgressState, ProgressStyle};
use par_stream::prelude::*;
use serde::Deserialize;
use serde_aux::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};
use tokio::{process::Command, task::spawn_blocking};
use tracing::{debug, error, info, info_span, warn, Span};
use tracing_indicatif::{span_ext::IndicatifSpanExt, IndicatifLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use walkdir::WalkDir;

const MEDIAINFO: &str = env!("MEDIAINFO_PATH");
const FFMPEG: &str = env!("FFMPEG_PATH");
const TRANSCODE_THRESHOLD_PERCENT: f64 = 30.0;

static ENCODER_LOCK: Mutex<()> = Mutex::const_new(());
static DRY_RUN: OnceCell<bool> = OnceCell::const_new();

#[derive(Parser)]
struct Args {
    #[arg(long)]
    dry_run: bool,
    media: Vec<PathBuf>,
}

fn elapsed_subsec(state: &ProgressState, writer: &mut dyn std::fmt::Write) {
    let _ = writer.write_str(&humantime::format_duration(state.elapsed()).to_string());
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

    info!("Discovering files to transcode...");
    let walker = args
        .media
        .into_iter()
        .map(|p| WalkDir::new(p).into_iter())
        .flatten();

    let files = spawn_blocking(move || {
        let mut files: Vec<PathBuf> = Vec::with_capacity(walker.size_hint().0);
        let video_exts = [
            "avi", "flv", "m4v", "mkv", "mov", "mp4", "mpg", "ts", "webm", "wmv",
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

    let header_span = info_span!("transcode");
    header_span.pb_set_style(&ProgressStyle::default_bar());
    header_span.pb_set_length(media_count as u64);
    let header_span_enter = header_span.enter();

    stream::iter(files.into_iter())
        .par_then_unordered(None, |p| async move {
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

            if video_md.format == "AV1" {
                info!(path=%p.display(), "Skipping AV1 file");
                return None;
            }

            if video_md.format == "HEVC" {
                info!(path=%p.display(), "Skipping HEVC file");
                return None;
            }

            info!("Enqueued '{}'", p.display());
            Some(media)
        })
        .filter_map(|opt| async move { opt })
        .then(|video| async move {
            if DRY_RUN.get().copied().unwrap_or(true) {
                debug!("Skipping due to dry-run");
                return anyhow::Ok(None);
            }
            let transcoded = video.transcode_hevc_vaapi("/tmp").await?;
            let transcoded =
                TempFile::from_existing(transcoded.clone(), async_tempfile::Ownership::Owned)
                    .await
                    .context("wrap transcoded in tempfile");

            transcoded.map(|ts| Some((video, ts)))
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
        .par_then_unordered(None, |(original, transcode)| async move {
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

            let original_sz = tokio::fs::metadata(&original.path)
                .await
                .context("read original metadata")?
                .len();
            let transcoded_sz = transcode
                .metadata()
                .await
                .context("read transcoded metadata")?
                .len();
            let shrink_percentage = 100.0 - (100.0 * (transcoded_sz as f64 / original_sz as f64));
            if shrink_percentage < TRANSCODE_THRESHOLD_PERCENT {
                warn!("Ignoring transcode due to below-threshold gains of {shrink_percentage:.2}%");
                return Ok(None);
            }
            Ok(Some((original, transcode)))
        })
        .filter_map(|res| async move {
            match res {
                Ok(inner) => inner,
                Err(e) => {
                    error!("{e:?}");
                    None
                },
            }
        })
        .par_for_each(None, |(original, transcode)| async move {
         info!(original=%original.path.display(), transcode=%transcode.file_path().display(), "Replacing original with transcode");
         let final_path = original.path.with_file_name(transcode.file_path().file_name().expect("transcoded file always has a file name"));
         match tokio::fs::copy(&transcode.file_path(), &final_path).await {
             Ok(_) => {
                 info!("Successfully transcoded '{}'", final_path.display());
                 tokio::fs::remove_file(&original.path).await.ok();
             },
             Err(e) => {
                 error!("Failed to copy transcoded file: {e:?}");
                 return;
             },
         }

         Span::current().pb_inc(1);
        }).await;

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
}

#[derive(Deserialize, Debug)]
#[serde(tag = "@type")]
enum TrackInfo {
    General(ContainerInfo),
    Video(VideoInfo),
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct ContainerInfo {
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    video_count: usize,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    audio_count: usize,
    file_extension: String,
    format: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct VideoInfo {
    stream_order: String,
    format: String,
    #[serde(rename = "CodecID")]
    codec_id: String,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    width: usize,
    #[serde(deserialize_with = "deserialize_number_from_string")]
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
    pub async fn transcode_hevc_vaapi(&self, out_dir: impl AsRef<Path>) -> Result<PathBuf> {
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

        Ok(transcoded_path)
    }
}
