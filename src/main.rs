pub mod priority_channel;

use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_atomic::Atomic;
use async_tempfile::TempFile;
use bytesize::ByteSize;
use clap::Parser;
use decimal_percentage::Percentage;
use derivative::Derivative;
use ffmpeg_sidecar::{
    child::FfmpegChild,
    command::FfmpegCommand,
    event::{FfmpegEvent, LogLevel},
};
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressState, ProgressStyle};
use par_stream::ParStreamExt;
use thread_priority::{
    set_thread_priority_and_policy, NormalThreadSchedulePolicy, ThreadId, ThreadPriority,
    ThreadSchedulePolicy,
};
use tikv_jemallocator::Jemalloc;
use tokio::{
    io::AsyncWriteExt,
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::{spawn_blocking, JoinSet},
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info, info_span, trace, warn, Span};
use tracing_indicatif::{span_ext::IndicatifSpanExt, IndicatifLayer};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter,
};
use walkdir::WalkDir;

use crate::priority_channel::{priority_channel, PriorityReceiver, PrioritySender};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// `transcode-rs` is a tool to automate the transcoding and general maintenance of a video
/// library.
#[derive(Debug, clap::Parser)]
#[command(about, author)]
struct Config {
    /// Increases logging verbosity, may be specified multiple times.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Dry-run, not altering any files
    #[arg(long)]
    dry_run: bool,

    /// Use hardware acceleration to transcode video streams
    #[arg(long, value_enum, default_value_t=Hwaccel::None)]
    hwaccel: Hwaccel,

    /// The desired file-size change after compression, expressed as a percentage.
    ///
    /// If you wanted 100byte video files to end up with at most 60 bytes, this value would be 40%.
    #[arg(long, default_value = "0.4")]
    compression_goal: Percentage,

    /// The minimum constant rate factor (CRF) to use when transcoding.
    ///
    /// If we miss the <COMPRESSION_GOAL> with this CRF, it will be raised until the goal is
    /// reached.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u8).range(0..64))]
    min_crf: u8,

    /// How to handle non-video files discovered in <VIDEO_DIR>.
    #[arg(long, value_enum, default_value_t=FileAction::Skip)]
    non_video_action: FileAction,

    /// If <NON_VIDEO_ACTION> is `move`, where to move the files to.
    #[arg(long, required_if_eq("non_video_action", "move"))]
    non_video_dir: Option<PathBuf>,

    /// Whether to remove symlinks in <VIDEO_DIR>.
    #[arg(long)]
    remove_symlinks: bool,

    /// Whether to remove empty directories in <VIDEO_DIR>.
    #[arg(long)]
    remove_empty_dirs: bool,

    /// How to handle broken video files in <VIDEO_DIR>
    ///
    /// Broken is defined as files which fail to `ffprobe`.
    #[arg(long, value_enum, default_value_t=FileAction::Skip)]
    broken_video_action: FileAction,

    /// If <BROKEN_VIDEO_ACTION> is `move`, where to move the files to.
    #[arg(long, required_if_eq("broken_video_action", "move"))]
    broken_video_dir: Option<PathBuf>,

    /// How to handle video files which failed to transcode.
    #[arg(long, value_enum, default_value_t=FileAction::Skip)]
    failed_transcode_action: FileAction,

    /// If <FAILED_TRANSCODE_ACTION> is `move`, where to move the files to.
    #[arg(long, required_if_eq("failed_transcode_action", "move"))]
    failed_transcode_dir: Option<PathBuf>,

    /// How to handle successfully transcoded files.
    ///
    /// Here `delete` means to clobber the original video with the transcode.
    #[arg(long, value_enum, default_value_t=FileAction::Delete)]
    transcoded_video_action: FileAction,

    /// Path where to place transcoded videos. If unspecified, transcodes will clobber originals.
    #[arg(long, required_if_eq("transcoded_video_action", "move"))]
    transcoded_video_dir: Option<PathBuf>,

    /// Path to use for temporary processing artifacts, such as ongoing transcodes.
    #[arg(long, default_value = "/tmp/transcode-rs")]
    working_dir: PathBuf,

    /// Directory to look for video files to transcode.
    video_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum FileAction {
    /// Move the file
    Move,
    /// Delete the file
    Delete,
    /// Skip (ignore) the file
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, strum::Display)]
#[strum(serialize_all = "lowercase")]
enum Hwaccel {
    /// Do not use any hardware acceleration
    None,
    /// Automatically select the hardware acceleration method
    Auto,
    /// Use VDPAU (Video Decode and Presentation API for Unix) hardware acceleration
    Vdpau,
    /// Use DXVA2 (DirectX Video Acceleration) hardware acceleration
    Dxva2,
    /// Use D3D11VA (DirectX Video Acceleration) hardware acceleration
    D3d11va,
    /// Use VAAPI (Video Acceleration API) hardware acceleration
    Vaapi,
    /// Use the Intel QuickSync Video acceleration for video transcoding
    Qsv,
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// ```mermaid
/// ---
/// title: transcode-rs
/// ---
/// stateDiagram-v2
///     direction LR
///     [*] --> find_video_files
///     find_video_files --> analyze_video_files
///     find_video_files --> handle_empty_dirs
///     find_video_files --> handle_symlinks
///     find_video_files --> handle_nonvideo_files
///
///     handle_empty_dirs --> [*]
///     handle_symlinks --> [*]
///     handle_nonvideo_files --> [*]
///
///     analyze_video_files --> transcode_video_files
///     analyze_video_files --> handle_broken_video_files
///
///     handle_broken_video_files --> [*]
///
///     transcode_video_files --> finalize_transcodes
///     transcode_video_files --> handle_failed_transcodes
///
///     finalize_transcodes --> [*]
///     handle_failed_transcodes --> [*]
/// ```
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    human_panic::setup_panic!();

    let mut config = Config::parse();
    config.canonicalize()?;

    // set up logging
    let level_filter = match config.verbose {
        0 => LevelFilter::INFO,
        1 => LevelFilter::DEBUG,
        2.. => LevelFilter::TRACE,
    };
    let indicatif_layer = IndicatifLayer::new()
        .with_progress_style(
            ProgressStyle::with_template(
                "{span_child_prefix} {span_name} {span_fields} {wide_msg} {elapsed}",
            )
            .unwrap()
            .with_key(
                "elapsed",
                |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                    let elapsed = Duration::from_secs(state.elapsed().as_secs());
                    write!(writer, "{}", humantime::format_duration(elapsed)).ok();
                },
            ),
        )
        .with_span_child_prefix_symbol("↳")
        .with_span_child_prefix_indent(" ");
    tracing_subscriber::registry()
        .with(
            EnvFilter::builder()
                .with_default_directive(level_filter.into())
                .from_env_lossy(),
        )
        .with(
            tracing_subscriber::fmt::layer()
                // .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_writer(indicatif_layer.get_stderr_writer()),
        )
        .with(indicatif_layer)
        .init();

    let pb_span = info_span!("progress");
    pb_span.pb_set_style(&ProgressStyle::default_bar());
    pb_span.pb_set_length(1);
    pb_span.pb_start();

    let state = State::new(config, pb_span);

    let mut tasks = JoinSet::new();

    let (find_video_files_in, find_video_files_out) = priority_channel();
    let (find_dirs_in, find_dirs_out) = unbounded_channel();
    let (find_symlinks_in, find_symlinks_out) = unbounded_channel();
    let (find_nonvideo_files_in, find_nonvideo_files_out) = unbounded_channel();
    let (analyze_video_files_in, analyze_video_files_out) = unbounded_channel();
    let (analyze_broken_video_files_in, analyze_broken_video_files_out) = unbounded_channel();
    let (transcode_video_files_in, transcode_video_files_out) = unbounded_channel();
    let (transcode_failed_files_in, transcode_failed_files_out) = unbounded_channel();
    let ingestion_done_signal = Arc::new(Atomic::new(false));

    let _signal_move = ingestion_done_signal.clone();
    let _state_move = state.clone();
    tasks.spawn_blocking(move || {
        find_video_files(
            find_video_files_in,
            find_nonvideo_files_in,
            find_symlinks_in,
            find_dirs_in,
            _signal_move,
            _state_move,
        );
        Ok(())
    });
    tasks.spawn(analyze_video_files(
        find_video_files_out,
        analyze_video_files_in,
        analyze_broken_video_files_in,
        state.clone(),
    ));
    tasks.spawn(handle_nonvideo_files(
        find_nonvideo_files_out,
        state.clone(),
    ));
    tasks.spawn(handle_symlinks(find_symlinks_out, state.clone()));
    tasks.spawn(handle_dirs(find_dirs_out, state.clone()));
    tasks.spawn(handle_broken_video_files(
        analyze_broken_video_files_out,
        state.clone(),
    ));
    tasks.spawn(transcode_video_files(
        analyze_video_files_out,
        transcode_video_files_in,
        transcode_failed_files_in,
        ingestion_done_signal,
        state.clone(),
    ));
    tasks.spawn(handle_failed_transcodes(
        transcode_failed_files_out,
        state.clone(),
    ));
    tasks.spawn(finalize_transcodes(
        transcode_video_files_out,
        state.clone(),
    ));

    while let Some(res) = tasks.join_next().await {
        res??;
    }
    Ok(())
}

#[derive(Clone)]
struct State(Arc<StateInner>);

#[derive(Debug)]
struct StateInner {
    config: Config,
    pb_span: Span,
}

impl Deref for State {
    type Target = StateInner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl State {
    fn new(config: Config, pb_span: Span) -> Self {
        Self(Arc::new(StateInner { config, pb_span }))
    }
}

impl Config {
    fn canonicalize(&mut self) -> Result<()> {
        let expand_canon = |p: &Path| -> Result<PathBuf> {
            let p = shellexpand::path::full(p).context("expand path")?;
            if !p.exists() {
                std::fs::create_dir_all(&p).context("create configured dir")?;
            }
            p.canonicalize().context("canonicalize configured dir")
        };
        let expand_canon_opt = |p: &Option<PathBuf>| -> Result<Option<PathBuf>> {
            p.as_ref().map(|p| expand_canon(p)).transpose()
        };

        self.non_video_dir = expand_canon_opt(&self.non_video_dir)?;
        self.broken_video_dir = expand_canon_opt(&self.broken_video_dir)?;
        self.failed_transcode_dir = expand_canon_opt(&self.failed_transcode_dir)?;
        self.transcoded_video_dir = expand_canon_opt(&self.transcoded_video_dir)?;
        self.working_dir = expand_canon(&self.working_dir)?;
        self.video_dir = expand_canon(&self.video_dir)?;
        Ok(())
    }
}

#[tracing::instrument(skip_all)]
fn find_video_files(
    video_files_out: PrioritySender<PathBuf, u64>,
    nonvideo_out: UnboundedSender<PathBuf>,
    symlink_out: UnboundedSender<PathBuf>,
    dir_out: UnboundedSender<PathBuf>,
    ingestion_done_signal: Arc<Atomic<bool>>,
    state: State,
) {
    let video_exts = [
        "avi", "flv", "m4v", "mkv", "mov", "mp4", "mpg", "ts", "vob", "webm", "wmv",
    ];

    let root = &state.config.video_dir;
    info!(path=%root.display(), "Scanning dir");

    let walker = WalkDir::new(root);
    for entry in walker {
        let Ok(entry) = entry else {
            warn!("skipping entry: {entry:?}");
            continue;
        };
        let path = entry.path();
        if entry.path_is_symlink() {
            if let Err(e) = symlink_out.send(path.to_path_buf()) {
                error!(path=%path.display(), "failed to submit symlink: {e:?}");
            }
            continue;
        }
        if entry.file_type().is_dir() {
            if let Err(e) = dir_out.send(path.to_path_buf()) {
                error!(path=%path.display(), "failed to submit dir: {e:?}");
            }
            continue;
        }
        let Some(ext) = path.extension() else {
            if let Err(e) = nonvideo_out.send(path.to_path_buf()) {
                error!(path=%path.display(), "failed to submit non-video file: {e:?}");
            }
            continue;
        };
        if !video_exts.iter().any(|&e| e == ext.to_ascii_lowercase()) {
            if let Err(e) = nonvideo_out.send(path.to_path_buf()) {
                error!(path=%path.display(), "failed to submit non-video file: {e:?}");
            }
            continue;
        }

        trace!(path=%path.display(), "found video file");
        let metadata = match std::fs::metadata(&path) {
            Ok(md) => md,
            Err(e) => {
                error!(path=%path.display(), "failed to get metadata for video file: {e:?}");
                continue;
            }
        };
        let file_size = metadata.len();
        if let Err(e) = video_files_out.blocking_send(path.to_path_buf(), file_size) {
            error!(path=%path.display(), "failed to submit video file: {e:?}");
        } else {
            state.pb_span.pb_inc_length(1);
        }
    }
    ingestion_done_signal.store(true);
}

async fn handle_symlinks(symlinks_in: UnboundedReceiver<PathBuf>, state: State) -> Result<()> {
    #[tracing::instrument(skip_all, fields(path=%path.display()), parent=state.pb_span.clone())]
    async fn handle_symlink(path: PathBuf, state: State) -> Result<()> {
        if !path.is_symlink() {
            anyhow::bail!("received non-symlink path while handling symlinks");
        }
        if !state.config.remove_symlinks {
            trace!("skipped symlink");
            return Ok(());
        }
        if !state.config.dry_run {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                error!("failed to remove symlink: {e:?}");
                return Ok(());
            }
        }
        info!("removed symlink");
        Ok(())
    }
    UnboundedReceiverStream::new(symlinks_in)
        .map(move |path| (path, state.clone()))
        .par_then_unordered(None, |(path, state)| handle_symlink(path, state))
        .try_collect()
        .await
}

async fn handle_dirs(dirs_in: UnboundedReceiver<PathBuf>, state: State) -> Result<()> {
    #[tracing::instrument(skip_all, fields(path=%path.display()), parent=state.pb_span.clone())]
    async fn handle_dir(path: PathBuf, state: State) -> Result<()> {
        if !path.is_dir() {
            anyhow::bail!("received non-dir path while handling dirs");
        }
        if !state.config.remove_empty_dirs {
            trace!("skipped dir");
            return Ok(());
        }
        let mut read_dir = match tokio::fs::read_dir(&path).await {
            Ok(r) => r,
            Err(e) => {
                error!("failed to read dir: {e:?}");
                return Ok(());
            }
        };
        let first_entry = match read_dir.next_entry().await {
            Ok(e) => e,
            Err(e) => {
                error!("failed to read dir entry: {e:?}");
                return Ok(());
            }
        };
        let dir_is_empty = first_entry.is_none();
        drop(first_entry);
        drop(read_dir);

        if !dir_is_empty {
            trace!("skipping non-empty dir");
            return Ok(());
        }

        if !state.config.dry_run {
            if let Err(e) = tokio::fs::remove_dir(&path).await {
                error!("failed to remove empty dir: {e:?}");
                return Ok(());
            };
        }

        info!("removed empty dir");
        Ok(())
    }
    UnboundedReceiverStream::new(dirs_in)
        .map(move |path| (path, state.clone()))
        .par_then_unordered(None, |(path, state)| handle_dir(path, state))
        .try_collect()
        .await
}

async fn handle_nonvideo_files(
    nonvideo_in: UnboundedReceiver<PathBuf>,
    state: State,
) -> Result<()> {
    #[tracing::instrument(skip_all, fields(path=%path.display()), parent=state.pb_span.clone())]
    async fn handle_nonvideo_file(path: PathBuf, state: State) -> Result<()> {
        let dry_run = state.config.dry_run;
        if !path.is_file() {
            anyhow::bail!("received non-file path when handling nonvideo files");
        }
        match state.config.non_video_action {
            FileAction::Skip => trace!("skipped non-video file"),
            FileAction::Delete => {
                if !dry_run {
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        error!("failed to delete non-video file: {e:?}");
                        return Ok(());
                    }
                }
                info!("deleted non-video file");
            }
            FileAction::Move => {
                let nonvideo_dir = state
                    .config
                    .non_video_dir
                    .as_ref()
                    .context("get nonvideo dir")?;
                if !dry_run {
                    let move_res =
                        move_subtree(&state.config.video_dir, &path, None, nonvideo_dir).await;
                    let _dest_path = match move_res {
                        Ok(p) => p,
                        Err(e) => {
                            error!(
                                destination=%nonvideo_dir.display(),
                                "failed to move nonvideo file: {e:?}"
                            );
                            return Ok(());
                        }
                    };
                }
                info!("moved non-video file to {}", nonvideo_dir.display());
            }
        }
        Ok(())
    }
    UnboundedReceiverStream::new(nonvideo_in)
        .map(move |path| (path, state.clone()))
        .par_then_unordered(None, |(path, state)| handle_nonvideo_file(path, state))
        .try_collect()
        .await
}

async fn analyze_video_files(
    video_files_in: PriorityReceiver<PathBuf, u64>,
    transcode_out: UnboundedSender<VideoFile<PathBuf>>,
    broken_out: UnboundedSender<PathBuf>,
    state: State,
) -> Result<()> {
    #[tracing::instrument(skip_all, fields(path=%path.display()), parent=state.pb_span.clone())]
    async fn analyze_video_file(
        path: PathBuf,
        transcode_out: UnboundedSender<VideoFile<PathBuf>>,
        broken_out: UnboundedSender<PathBuf>,
        state: State,
    ) -> Result<()> {
        match VideoFile::new(path.clone()).await {
            Ok(vif) => {
                debug!(codec=?vif.video_codec, "successfully analyzed video file");
                if vif.video_codec == VideoCodec::AV1 {
                    debug!("skipping AV1 video file");
                    state.pb_span.pb_inc(1);
                    return Ok(());
                }
                transcode_out.send(vif)?;
            }
            Err(e) => {
                error!("broken video file: {e:?}");
                broken_out.send(path)?;
            }
        }
        Ok(())
    }
    video_files_in
        .into_stream()
        .map(move |(path, _prio)| {
            (
                path,
                transcode_out.clone(),
                broken_out.clone(),
                state.clone(),
            )
        })
        .par_then(None, |(path, transcode_out, broken_out, state)| {
            analyze_video_file(path, transcode_out, broken_out, state)
        })
        .try_collect()
        .await
}

async fn handle_broken_video_files(
    broken_in: UnboundedReceiver<PathBuf>,
    state: State,
) -> Result<()> {
    #[tracing::instrument(skip_all, fields(path=%path.display()), parent=state.pb_span.clone())]
    async fn handle_broken_video_file(path: PathBuf, state: State) -> Result<()> {
        let dry_run = state.config.dry_run;
        if !path.is_file() {
            anyhow::bail!("received non-file path when handling broken video files");
        }
        match state.config.broken_video_action {
            FileAction::Skip => trace!("skipped broken video file"),
            FileAction::Delete => {
                if !dry_run {
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        error!("failed to delete broken video file: {e:?}");
                        return Ok(());
                    }
                }
                info!("deleted broken video file");
            }
            FileAction::Move => {
                let broken_dir = state
                    .config
                    .broken_video_dir
                    .as_ref()
                    .context("get broken video dir")?;
                if !dry_run {
                    let move_res =
                        move_subtree(&state.config.video_dir, &path, None, broken_dir).await;
                    let _dest_path = match move_res {
                        Ok(p) => p,
                        Err(e) => {
                            error!(
                                destination=%broken_dir.display(),
                                "failed to move broken video file: {e:?}"
                            );
                            return Ok(());
                        }
                    };
                }
                info!("moved broken video file to {}", broken_dir.display());
            }
        }
        Ok(())
    }
    UnboundedReceiverStream::new(broken_in)
        .map(move |path| (path, state.clone()))
        .par_then_unordered(None, |(path, state)| handle_broken_video_file(path, state))
        .try_collect()
        .await
}

async fn transcode_video_files(
    video_files_in: UnboundedReceiver<VideoFile<PathBuf>>,
    transcode_out: UnboundedSender<(VideoFile<PathBuf>, TempFile)>,
    failed_out: UnboundedSender<VideoFile<PathBuf>>,
    ingestion_done_signal: Arc<Atomic<bool>>,
    state: State,
) -> Result<()> {
    #[tracing::instrument(skip_all, parent=span)]
    fn transcode_progress(
        original_path: &Path,
        crf: u8,
        nb_frames: Option<u64>,
        mut ffmpeg: FfmpegChild,
        span: Span,
    ) -> Result<()> {
        let template = "{span_child_prefix} {span_name} '{path}' crf={crf} fps={fps} frame={frame} progress={progress} {wide_msg} {elapsed}";

        ffmpeg
            .iter()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .map(|e| (e, original_path.to_path_buf()))
            .for_each(|(event, original_path)| match event {
                FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, msg) => {
                    error!("ffmpeg error: {msg}")
                }
                FfmpegEvent::Log(LogLevel::Warning, msg) => warn!("ffmpeg warn: {msg}"),
                FfmpegEvent::Progress(p) => {
                    let frame = p.frame;
                    let fps = p.fps;
                    let progress = if let Some(nb_frames) = nb_frames {
                        let percent = ((frame as f64) / (nb_frames as f64)) * 100.0;
                        format!("{percent:.2}%")
                    } else {
                        "unknown".to_string()
                    };
                    Span::current().pb_set_style(
                        &ProgressStyle::with_template(template)
                            .unwrap()
                            .with_key(
                                "path",
                                move |_: &ProgressState, writer: &mut dyn std::fmt::Write| {
                                    write!(writer, "{}", original_path.display()).ok();
                                },
                            )
                            .with_key(
                                "crf",
                                move |_: &ProgressState, writer: &mut dyn std::fmt::Write| {
                                    write!(writer, "{crf}").ok();
                                },
                            )
                            .with_key(
                                "fps",
                                move |_: &ProgressState, writer: &mut dyn std::fmt::Write| {
                                    write!(writer, "{fps}").ok();
                                },
                            )
                            .with_key(
                                "frame",
                                move |_: &ProgressState, writer: &mut dyn std::fmt::Write| {
                                    write!(writer, "{frame}").ok();
                                },
                            )
                            .with_key(
                                "progress",
                                move |_: &ProgressState, writer: &mut dyn std::fmt::Write| {
                                    write!(writer, "{progress}").ok();
                                },
                            )
                            .with_key(
                                "elapsed",
                                |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                                    let elapsed = Duration::from_secs(state.elapsed().as_secs());
                                    write!(writer, "{}", humantime::format_duration(elapsed)).ok();
                                },
                            ),
                    );
                }
                _ => {}
            });

        Ok(())
    }

    let mut stream = UnboundedReceiverStream::new(video_files_in);

    ingestion_done_signal
        .subscribe_arc()
        .wait(|ingestion_done| ingestion_done)
        .await;

    'next_video: while let Some(original) = stream.next().await {
        info!("transcoding '{}'", original.path().display());
        let original_size = original.size;
        let max_size = if original.video_codec == VideoCodec::Hevc {
            original_size
        } else {
            (1.0 - state.config.compression_goal) * original_size
        };

        'next_crf: for crf in state.config.min_crf..64 {
            let dest = match TempFile::new_in(&state.config.working_dir).await {
                Ok(tmp) => tmp,
                Err(e) => {
                    error!(working_dir=%state.config.working_dir.display(), "failed to create transcode file: {e:?}");
                    continue;
                }
            };
            let mut ffmpeg = original.spawn_transcode(
                dest.file_path(),
                VideoCodec::AV1,
                AudioCodec::Aac,
                crf,
                state.config.hwaccel,
            )?;
            let mut ffmpeg_stdin = tokio::process::ChildStdin::from_std(
                ffmpeg
                    .take_stdin()
                    .context("ffmpeg child has no stdin handle")?,
            )?;

            let _original_path = original.path().to_path_buf();
            let _nb_frames = original.nb_frames;
            let _span = state.pb_span.clone();
            let progress_task = tokio::task::spawn_blocking(move || {
                transcode_progress(&_original_path, crf, _nb_frames, ffmpeg, _span)
            });

            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if progress_task.is_finished() {
                    break;
                }
                let transcode_size = tokio::fs::metadata(dest.file_path())
                    .await
                    .context("get metadata for transcode")?
                    .len();
                if transcode_size > max_size {
                    warn!(
                        path=%original.path().display(),
                        original_size=ByteSize::b(original_size).to_string_as(true),
                        max_size=ByteSize::b(max_size).to_string_as(true),
                        crf,
                        "transcode failed compression goal, increasing crf"
                    );
                    ffmpeg_stdin.write_all(b"q").await?;
                    progress_task.await.ok();
                    drop(dest);
                    continue 'next_crf;
                }
            }

            if let Err(e) = progress_task.await {
                error!(path=%original.path().display(), "transcode failed: {e:?}");
                drop(dest);
                failed_out.send(original)?;
                continue 'next_video;
            };
            info!(path=%original.path().display(), crf, "transcoded successfully");
            transcode_out.send((original, dest))?;
            continue 'next_video;
        }
    }
    Ok(())
}

async fn handle_failed_transcodes(
    failed_videos_in: UnboundedReceiver<VideoFile<PathBuf>>,
    state: State,
) -> Result<()> {
    #[tracing::instrument(skip_all, fields(path=%video.path().display()), parent=state.pb_span.clone())]
    async fn handle_failed_transcode(video: VideoFile<PathBuf>, state: State) -> Result<()> {
        let dry_run = state.config.dry_run;
        let path = video.path;
        if !path.is_file() {
            anyhow::bail!("received non-file path when handling failed transcodes");
        }
        match state.config.failed_transcode_action {
            FileAction::Skip => trace!("skipped failed transcode"),
            FileAction::Delete => {
                if !dry_run {
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        error!("failed to delete failed transcode video file: {e:?}");
                        return Ok(());
                    }
                }
                info!("deleted failed transcode video file");
            }
            FileAction::Move => {
                let failed_dir = state
                    .config
                    .failed_transcode_dir
                    .as_ref()
                    .context("get failed transcode dir")?;
                if !dry_run {
                    let move_res =
                        move_subtree(&state.config.video_dir, &path, None, failed_dir).await;
                    let _dest_path = match move_res {
                        Ok(p) => p,
                        Err(e) => {
                            error!(
                                destination=%failed_dir.display(),
                                "failed to move failed transcode file: {e:?}"
                            );
                            return Ok(());
                        }
                    };
                }
                info!("moved failed transcode file to {}", failed_dir.display());
            }
        }
        Ok(())
    }
    UnboundedReceiverStream::new(failed_videos_in)
        .map(move |video| (video, state.clone()))
        .par_then_unordered(None, |(video, state)| handle_failed_transcode(video, state))
        .try_collect()
        .await
}

async fn finalize_transcodes(
    transcoded_videos_in: UnboundedReceiver<(VideoFile<PathBuf>, TempFile)>,
    state: State,
) -> Result<()> {
    #[tracing::instrument(skip_all, fields(path=%original.path().display()), parent=state.pb_span.clone())]
    async fn finalize_transcode(
        original: VideoFile<PathBuf>,
        transcode: TempFile,
        state: State,
    ) -> Result<()> {
        let transcode = match VideoFile::new(transcode).await {
            Ok(v) => v,
            Err(e) => {
                error!("failed to analyze transcode: {e:?}");
                return Ok(());
            }
        };
        if transcode.video_codec != VideoCodec::AV1 {
            error!(
                "transcode did not result in an AV1 video stream: {:?}",
                transcode.video_codec
            );
            return Ok(());
        }
        let shrunk_amount = original.size - transcode.size;
        let shrunk_percent = ((transcode.size as f64) / (original.size as f64)) * 100.0;

        info!(
            saved = ByteSize::b(shrunk_amount).to_string_as(true),
            shrunk = format!("{shrunk_percent:.2}%"),
            "successfully transcoded"
        );
        let dry_run = state.config.dry_run;
        match state.config.transcoded_video_action {
            FileAction::Skip => {
                drop(transcode);
                trace!("discarded transcode");
            }
            FileAction::Delete => {
                // Clobber
                let dest = original.path().with_extension("mp4");
                if !dry_run {
                    if let Err(e) = tokio::fs::copy(transcode.path(), &dest).await {
                        error!(path=%dest.display(), "failed to copy transcode to final destination: {e:?}");
                        return Ok(());
                    };
                    if original.path() != dest {
                        if let Err(e) = tokio::fs::remove_file(original.path()).await {
                            error!("failed to remove original file after transcode: {e:?}");
                            return Ok(());
                        };
                    }
                }
                trace!("clobbered original with transcode");
            }
            FileAction::Move => {
                let transcode_dir = state
                    .config
                    .transcoded_video_dir
                    .as_ref()
                    .context("get transcoded dir")?;
                if !dry_run {
                    let move_res = move_subtree(
                        &state.config.video_dir,
                        transcode.path(),
                        Some(original.path()),
                        transcode_dir,
                    )
                    .await;
                    let dest_path = match move_res {
                        Ok(p) => p,
                        Err(e) => {
                            error!(destination=%transcode_dir.display(), "failed to move transcode: {e:?}");
                            return Ok(());
                        }
                    };
                    drop(transcode);
                    info!(path=%dest_path.display(), "moved transcode");
                }
            }
        }
        state.pb_span.pb_inc(1);
        Ok(())
    }
    UnboundedReceiverStream::new(transcoded_videos_in)
        .map(move |(original, transcode)| (original, transcode, state.clone()))
        .par_then_unordered(None, |(original, transcode, state)| {
            finalize_transcode(original, transcode, state)
        })
        .try_collect()
        .await
}

async fn move_subtree(
    src_root: &Path,
    src_file: &Path,
    src_subtree: Option<&Path>,
    dst_root: &Path,
) -> Result<PathBuf> {
    let src_subtree = src_subtree.unwrap_or(src_file);
    let canon_subtree = src_subtree.canonicalize()?;
    let subtree = canon_subtree.strip_prefix(src_root)?;

    let dst_subtree = dst_root.join(subtree);
    let dst_dir = dst_subtree
        .parent()
        .context("get parent for destination subtree")?;
    tokio::fs::create_dir_all(&dst_dir).await?;
    tokio::fs::copy(&src_file, &dst_subtree).await?;
    tokio::fs::remove_file(&src_file).await?;
    Ok(dst_subtree)
}

trait AsPath: std::fmt::Debug {
    fn as_path(&self) -> &Path;
}

impl AsPath for PathBuf {
    fn as_path(&self) -> &Path {
        self.as_path()
    }
}

impl AsPath for TempFile {
    fn as_path(&self) -> &Path {
        self.file_path().as_path()
    }
}

fn hash_as_path<P: AsPath, H: std::hash::Hasher>(p: &P, state: &mut H) {
    use std::hash::Hash;
    p.as_path().hash(state);
}

fn eq_as_path<A: AsPath, B: AsPath>(a: &A, b: &B) -> bool {
    a.as_path().eq(b.as_path())
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
enum VideoCodec {
    AV1,
    Hevc,
    Other,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
enum AudioCodec {
    Aac,
    Other,
}

#[derive(Derivative)]
#[derivative(Debug, Hash, PartialEq, Eq)]
struct VideoFile<P: AsPath> {
    #[derivative(
        Hash(hash_with = "hash_as_path"),
        PartialEq(compare_with = "eq_as_path")
    )]
    path: P,
    video_codec: VideoCodec,
    audio_codec: Option<AudioCodec>,
    nb_frames: Option<u64>,
    size: u64,
}

impl<P: AsPath + Send> VideoFile<P> {
    async fn new(path: P) -> Result<Self> {
        let _path_move = path.as_path().to_path_buf();
        let probe = spawn_blocking(move || {
            let config = ffprobe::Config::builder()
                .ffprobe_bin(env!("FFPROBE_PATH"))
                .build();
            ffprobe::ffprobe_config(config, _path_move).context("ffprobe video file")
        })
        .await??;

        let Some(primary_video_stream) = probe
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("video"))
        else {
            anyhow::bail!("video file has no video stream");
        };
        let video_codec = match primary_video_stream.codec_name.as_deref() {
            Some("av1") => VideoCodec::AV1,
            Some("hevc") => VideoCodec::Hevc,
            Some(_) => VideoCodec::Other,
            None => anyhow::bail!("video file has video stream without codec name"),
        };

        let mut audio_codec = None;
        if let Some(primary_audio_stream) = probe
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("audio"))
        {
            audio_codec = match primary_audio_stream.codec_name.as_deref() {
                Some("aac") => Some(AudioCodec::Aac),
                Some(_) => Some(AudioCodec::Other),
                None => None,
            };
        }

        let size: u64 = probe
            .format
            .size
            .parse()
            .context("parse ffprobe'd file size")?;

        let nb_frames: Option<u64> = primary_video_stream
            .nb_frames
            .as_ref()
            .and_then(|nb| nb.parse().context("parse nb_frames as u64").ok());

        Ok(Self {
            path,
            video_codec,
            audio_codec,
            size,
            nb_frames,
        })
    }

    fn path(&self) -> &Path {
        self.path.as_path()
    }

    #[tracing::instrument(skip_all, fields(video=%self.path().display()))]
    fn spawn_transcode(
        &self,
        dst: &Path,
        vcodec: VideoCodec,
        acodec: AudioCodec,
        crf: u8,
        hwaccel: Hwaccel,
    ) -> Result<FfmpegChild> {
        let mut transcoder = FfmpegCommand::new_with_path(env!("FFMPEG_PATH"));
        transcoder
            .arg("-y")
            .args(["-threads", "0"])
            .hwaccel(&hwaccel.to_string())
            .arg("-i")
            .arg(self.path())
            .format("mp4")
            .args(["-movflags", "faststart"])
            .args(["-c:s", "copy"]);

        match (self.video_codec, vcodec) {
            (current, desired) if current == desired => transcoder.codec_video("copy"),
            (_, VideoCodec::AV1) => transcoder.codec_video("libsvtav1").args([
                "-preset",
                "5",
                "-crf",
                &format!("{crf}"),
                "-svtav1-params",
                "tune=0:film-grain=8",
            ]),
            _ => unimplemented!(),
        };

        match (self.audio_codec, acodec) {
            (None, _) => transcoder.codec_audio("copy"),
            (Some(current), desired) if current == desired => transcoder.codec_audio("copy"),
            (_, AudioCodec::Aac) => transcoder.codec_audio("libfdk_aac").args(["-vbr", "3"]),
            _ => unimplemented!(),
        };

        transcoder.arg(dst);

        let mut ffmpeg = transcoder
            .spawn()
            .with_context(|| format!("spawn ffmpeg transcode of '{}'", self.path().display()))?;

        let pid = ffmpeg.as_inner().id() as ThreadId;
        let prio = ThreadPriority::Crossplatform(10u8.try_into().unwrap());
        set_thread_priority_and_policy(
            pid,
            prio,
            ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other),
        )
        .ok();

        Ok(ffmpeg)
    }
}
