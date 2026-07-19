use bytes::BytesMut;

use super::{Device, Stream};

/// Playback backend for video streams.
pub trait VideoDevice: Device<Params = VideoParams, Stream: VideoStream> {}

/// Stream receiving decrypted video packets and lifecycle events.
pub trait VideoStream: Stream<Content = VideoStreamMessage> {}
impl<T> VideoStream for T where T: Stream<Content = VideoStreamMessage> {}

/// Parameters provided when a video stream is created.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct VideoParams {}

/// Decrypted video data packet.
#[derive(Debug)]
pub struct VideoPacket {
    /// Packet classification.
    pub kind: PacketKind,
    /// Stream timestamp associated with the packet.
    pub timestamp: u64,
    /// Packet payload bytes.
    pub payload: BytesMut,
}

/// Message delivered to a [`VideoStream`].
///
/// Lifecycle events are separate from packets so consumers never have to
/// decide whether a protocol frame carrying an event still contains usable
/// data. A suspension frame produces only [`Self::Event`]. A resumption frame
/// produces a `Resume` event followed by its codec-configuration packet.
#[derive(Debug)]
#[non_exhaustive]
pub enum VideoStreamMessage {
    /// A decrypted video packet.
    Packet(VideoPacket),
    /// A lifecycle transition for the current stream.
    Event(VideoStreamEvent),
}

/// A video stream lifecycle transition announced by the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoStreamEvent {
    /// Temporarily stop rendering while keeping the transport alive.
    Suspend,
    /// Resume rendering after a suspension.
    Resume,
}

/// Kind of video payload delivered to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PacketKind {
    /// AVC decoder configuration record.
    AvcC,
    /// HEVC decoder configuration record.
    Hvc1,
    /// Regular encoded video payload.
    Payload,
    /// Auxiliary plist payload.
    Plist,
    /// Unknown packet kind.
    Other(u16),
}
