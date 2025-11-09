use std::{
    error::Error,
    sync::{mpsc, OnceLock},
};

use airplay::playback::video::{PacketKind, VideoPacket, VideoParams};
use crate::ui::{FrameSink, VideoFrame};
use gstreamer::{
    Buffer, Caps, Element, ElementFactory, FlowError, FlowSuccess, Format, MessageType,
    MessageView, Pipeline, State, event::Eos, glib::GString, prelude::*,
};
use gstreamer_app::{AppSink, AppSinkCallbacks, AppSrc};
use gstreamer_video::{VideoInfo, VideoMeta};

static VIDEO_SINK: OnceLock<FrameSink> = OnceLock::new();

pub fn register_frame_sink(sink: FrameSink) {
    let _ = VIDEO_SINK.set(sink);
}

fn frame_sink() -> Option<FrameSink> {
    VIDEO_SINK.get().cloned()
}

pub fn transcode(
    id: u64,
    _params: VideoParams,
    rx: mpsc::Receiver<VideoPacket>,
) -> Result<(), Box<dyn Error>> {
    let mut ctx = None;
    loop {
        if let Ok(VideoPacket { kind, payload, .. }) = rx.recv() {
            match kind {
                PacketKind::AvcC => match create_stream(payload, id) {
                    Ok(res) => {
                        ctx = Some(res);
                    }
                    Err(err) => {
                        tracing::error!(%err, "couldn't initialize context with avcc header");
                    }
                },
                PacketKind::Payload => {
                    let Some(ctx) = &ctx else {
                        tracing::warn!("uninitialized context before payload");
                        continue;
                    };

                    let _ = ctx
                        .appsrc
                        .push_buffer(Buffer::from_slice(payload))
                        .inspect_err(|err| tracing::warn!(%err, "packet push failed"));
                }
                PacketKind::Other(kind) => {
                    tracing::debug!(%kind, "unknown packet type");
                }
            }
        } else {
            let Some(ctx) = &ctx else {
                return Ok(());
            };

            ctx.pipeline.send_event(Eos::new());
        }

        let Some(state) = &ctx else {
            continue;
        };

        let bus = state
            .pipeline
            .bus()
            .ok_or("pipeline must have message bus")?;

        for msg in bus.iter_filtered(&[MessageType::Eos, MessageType::Error]) {
            match msg.view() {
                MessageView::Eos(..) => {
                    return Ok(());
                }
                MessageView::Error(err) => {
                    return Err(format!(
                        "Error from {:?}: {} (debug: {:?})",
                        msg.src()
                            .map_or_else(|| GString::from("UNKNOWN"), GstObjectExt::path_string),
                        err.error(),
                        err.debug(),
                    ).into());
                }
                _ => {}
            }
        }
    }
}

fn build_display_pipeline(
    pipeline: &Pipeline,
    appsrc: &AppSrc,
    spec: &CodecPipelineSpec,
    frame_sink: FrameSink,
) -> Result<AppSink, Box<dyn Error>> {
    let parser = ElementFactory::make(spec.parser).build()?;
    let decoder = ElementFactory::make(spec.decoder).build()?;
    let convert = ElementFactory::make("videoconvert").build()?;
    let scale = ElementFactory::make("videoscale").build()?;
    let video_caps = Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .build();
    let capsfilter = ElementFactory::make("capsfilter")
        .property("caps", &video_caps)
        .build()?;
    let video_caps = Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .build();
    let appsink = AppSink::builder()
        .caps(&video_caps)
        .max_buffers(1)
        .drop(true)
        .build();

    let dispatcher = frame_sink.clone();
    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(FlowError::Error)?;
                let caps = sample.caps().ok_or(FlowError::Error)?;
                let info = VideoInfo::from_caps(&caps).map_err(|_| FlowError::Error)?;
                let width = info.width() as u32;
                let height = info.height() as u32;

                let stride = buffer
                    .meta::<VideoMeta>()
                    .map(|meta| meta.stride()[0] as usize)
                    .unwrap_or_else(|| info.stride()[0] as usize);
                let row_len = (width as usize) * 4;
                let total = row_len * (height as usize);

                let map = buffer.map_readable().map_err(|_| FlowError::Error)?;
                let src = map.as_slice();
                let required = stride * (height as usize);
                if stride == row_len && src.len() >= total {
                    dispatcher.send(VideoFrame {
                        width,
                        height,
                        data: src[..total].to_vec(),
                    });
                } else if stride >= row_len && src.len() >= required {
                    let mut data = vec![0u8; total];
                    for row in 0..height as usize {
                        let src_offset = row * stride;
                        let dst_offset = row * row_len;
                        data[dst_offset..dst_offset + row_len]
                            .copy_from_slice(&src[src_offset..src_offset + row_len]);
                    }
                    dispatcher.send(VideoFrame { width, height, data });
                } else {
                    return Err(FlowError::Error);
                }

                Ok(FlowSuccess::Ok)
            })
            .build(),
    );

    let appsrc_element = appsrc.upcast_ref::<Element>();
    let parser_element = parser.upcast_ref::<Element>();
    let decoder_element = decoder.upcast_ref::<Element>();
    let convert_element = convert.upcast_ref::<Element>();
    let scale_element = scale.upcast_ref::<Element>();
    let capsfilter_element = capsfilter.upcast_ref::<Element>();
    let appsink_element = appsink.upcast_ref::<Element>();

    pipeline.add_many([
        appsrc_element,
        parser_element,
        decoder_element,
        convert_element,
        scale_element,
        capsfilter_element,
        appsink_element,
    ])?;

    Element::link_many([
        appsrc_element,
        parser_element,
        decoder_element,
        convert_element,
        scale_element,
        capsfilter_element,
        appsink_element,
    ])?;

    Ok(appsink)
}

fn detect_codec(avcc: &[u8]) -> VideoCodec {
    if avcc.len() >= 8 {
        return match &avcc[4..8] {
            b"hvc1" => {
                VideoCodec::H265
            }
            _ => {
                VideoCodec::H264
            }
        }
    }

    VideoCodec::Unknown
}

fn extract_codec_record(header: &[u8], codec: VideoCodec) -> Option<Vec<u8>> {
    if header.first().copied() == Some(1) {
        return Some(header.to_vec());
    }

    let marker = match codec {
        VideoCodec::H264 => b"avcC",
        VideoCodec::H265 => b"hvcC",
        VideoCodec::Unknown => return None,
    };

    find_box_payload(header, marker)
}

fn find_box_payload(buf: &[u8], marker: &[u8; 4]) -> Option<Vec<u8>> {
    const VIDEO_SAMPLE_ENTRY_FIELDS_LEN: usize = 78;

    fn parse_range(
        buf: &[u8],
        mut cursor: usize,
        limit: usize,
        marker: &[u8; 4],
    ) -> Option<Vec<u8>> {
        while cursor + 8 <= limit {
            let mut size = u32::from_be_bytes(buf[cursor..cursor + 4].try_into().ok()?) as usize;
            if size == 0 {
                size = limit - cursor;
            }
            if size < 8 || cursor + size > limit {
                return None;
            }

            let kind = &buf[cursor + 4..cursor + 8];
            if kind == marker {
                return Some(buf[cursor + 8..cursor + size].to_vec());
            }

            if matches!(kind, b"avc1" | b"hvc1" | b"hev1") {
                let body_start = cursor + 8 + VIDEO_SAMPLE_ENTRY_FIELDS_LEN;
                if body_start < cursor + size {
                    if let Some(inner) = parse_range(buf, body_start, cursor + size, marker) {
                        return Some(inner);
                    }
                }
            } else if matches!(kind, b"stsd" | b"trak" | b"mdia" | b"minf" | b"stbl") {
                // generic container boxes that may wrap sample entries
                let body_start = cursor + 8;
                if body_start < cursor + size {
                    if let Some(inner) = parse_range(buf, body_start, cursor + size, marker) {
                        return Some(inner);
                    }
                }
            }

            cursor += size;
        }
        None
    }

    parse_range(buf, 0, buf.len(), marker)
}

fn format_header_snapshot(avcc: &[u8]) -> String {
    avcc.iter()
        .take(16)
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn create_stream(
    avcc: impl AsRef<[u8]> + Send + 'static,
    id: u64,
) -> Result<Context, Box<dyn Error>> {
    let header = avcc.as_ref().to_vec();
    let codec = detect_codec(&header);
    match codec {
        VideoCodec::H264 => println!("stream {id}: detected H.264"),
        VideoCodec::H265 => println!("stream {id}: detected H.265"),
        VideoCodec::Unknown => println!(
            "stream {id}: codec unknown (len={}, header={})",
            header.len(),
            format_header_snapshot(&header)
        ),
    }

    let spec = CodecPipelineSpec::from(codec);
    let pipeline = Pipeline::default();
    let codec_data = extract_codec_record(&header, codec).unwrap_or_else(|| header.clone());

    let caps = Caps::builder(spec.caps_mime)
        .field("stream-format", spec.stream_format)
        .field("alignment", "au")
        .field("codec_data", Buffer::from_slice(codec_data))
        .build();

    let appsrc = AppSrc::builder()
        .caps(&caps)
        .format(Format::Time)
        .is_live(true)
        .do_timestamp(true)
        .build();

    let Some(frame_sink) = frame_sink() else {
        return Err("video frame sink not initialized".into());
    };
    let appsink = build_display_pipeline(&pipeline, &appsrc, &spec, frame_sink)?;

    pipeline.set_state(State::Playing)?;

    Ok(Context {
        pipeline,
        appsrc,
        _appsink: appsink,
    })
}

struct Context {
    pipeline: Pipeline,
    appsrc: AppSrc,
    _appsink: AppSink,
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Err(err) = self.pipeline.set_state(State::Null) {
            tracing::warn!(%err, "pipeline state failed to be set to null");
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum VideoCodec {
    H264,
    H265,
    Unknown,
}

struct CodecPipelineSpec {
    caps_mime: &'static str,
    stream_format: &'static str,
    parser: &'static str,
    decoder: &'static str,
}

impl CodecPipelineSpec {
    fn from(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H265 => Self {
                caps_mime: "video/x-h265",
                stream_format: "hvc1",
                parser: "h265parse",
                decoder: "vtdec_hw",
            },
            _ => Self {
                caps_mime: "video/x-h264",
                stream_format: "avc",
                parser: "h264parse",
                decoder: "vtdec_hw",
            },
        }
    }
}
