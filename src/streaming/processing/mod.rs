use std::{io, net::IpAddr};

use bytes::Buf;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream, UdpSocket},
};
use tracing::Instrument;

use super::EncryptionMaterial;
use crate::{
    crypto::{AesIv128, AesKey128, ChaCha20Poly1305Key},
    pairing::SessionKey,
    playback::{
        audio::{AudioPacket, AudioStream},
        video::{PacketKind, VideoPacket, VideoStream, VideoStreamEvent, VideoStreamMessage},
    },
};

mod crypto;
mod memory;

#[derive(Debug)]
pub enum Encryption {
    ChaCha {
        key: ChaCha20Poly1305Key,
    },
    HomeKit {
        key: SessionKey,
        stream_connection_id: u64,
    },
    Legacy {
        key: AesKey128,
        iv: AesIv128,
        stream_connection_id: Option<u64>,
    },
}

impl TryFrom<EncryptionMaterial> for Encryption {
    type Error = io::Error;

    fn try_from(value: EncryptionMaterial) -> Result<Self, Self::Error> {
        if let Some(key) = value.chacha_key {
            Ok(Encryption::ChaCha { key })
        } else if let Some(key) = value.aeskey
            && let Some(iv) = value.aesiv
        {
            Ok(Encryption::Legacy {
                key,
                iv,
                stream_connection_id: value.stream_connection_id,
            })
        } else if let Some(key) = value.session_key
            && let Some(stream_connection_id) = value.stream_connection_id
        {
            Ok(Encryption::HomeKit {
                key,
                stream_connection_id,
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no encryption key passed",
            ))
        }
    }
}

#[tracing::instrument(level = "DEBUG")]
pub async fn event_processor(listener: TcpListener) {
    let mut buf = [0; 16 * 1024];
    while let Ok((mut stream, remote_addr)) = listener.accept().await {
        while let Ok(len @ 1..) = stream.read(&mut buf).await {
            tracing::trace!(%len, %remote_addr, "event data");
        }
    }
}

#[tracing::instrument(level = "DEBUG", skip(stream))]
pub async fn audio_buffered_processor(
    mut tcp_stream: TcpStream,
    stream: &impl AudioStream,
    audio_buf_size: u32,
    encryption: Encryption,
) -> io::Result<()> {
    const TRAILER_LEN: usize = 24;

    let mut audio_buf = memory::BytesHunk::new(audio_buf_size as usize);
    let cipher = build_audio_cipher(&encryption);

    loop {
        async {
            let pkt_len = tcp_stream.read_u16().await?;
            // 2 is pkt_len field size itself
            let pkt_len: usize = pkt_len.saturating_sub(2).into();

            if pkt_len < AudioPacket::HEADER_LEN + TRAILER_LEN {
                return Err(io::Error::other("malformed buffered stream"));
            }

            let mut rtp = audio_buf.allocate_buf(pkt_len);
            tcp_stream.read_exact(&mut rtp).await?;
            tracing::trace!(%pkt_len, "packet read");

            if cipher.decrypt(&mut rtp).is_ok() {
                tracing::trace!("packet decrypted");
            } else {
                tracing::warn!("packet decryption failed");
            }

            stream.on_data(AudioPacket { rtp });
            tokio::task::consume_budget().await;

            Ok(())
        }
        .instrument(tracing::debug_span!("packet.buffered"))
        .await?;
    }
}

#[tracing::instrument(level = "DEBUG", skip(stream))]
pub async fn audio_realtime_processor(
    expected_remote_addr: IpAddr,
    socket: UdpSocket,
    stream: &impl AudioStream,
    audio_buf_size: u32,
    encryption: Encryption,
) -> io::Result<()> {
    let mut pkt_buf = [0u8; 16 * 1024];
    let mut audio_buf = memory::BytesHunk::new(audio_buf_size as usize);
    let cipher = build_audio_cipher(&encryption);

    loop {
        async {
            let (pkt_len, remote_addr) = socket.recv_from(&mut pkt_buf).await?;

            // Filter out unexpected addresses
            if expected_remote_addr == remote_addr.ip() {
                if pkt_len < AudioPacket::HEADER_LEN {
                    tracing::warn!(%pkt_len, "malformed packet");
                } else {
                    let mut rtp = audio_buf.allocate_buf(pkt_len);
                    rtp.copy_from_slice(&pkt_buf[..pkt_len]);
                    tracing::trace!(%pkt_len, "packet read");

                    if cipher.decrypt(&mut rtp).is_ok() {
                        tracing::trace!("packet decrypted");
                    } else {
                        tracing::warn!("packet decryption failed");
                    }

                    stream.on_data(AudioPacket { rtp });
                    tokio::task::consume_budget().await;
                }
            } else {
                tracing::debug!(%remote_addr, "skip invalid connection");
            }

            io::Result::Ok(())
        }
        .instrument(tracing::debug_span!("packet.realtime"))
        .await?;
    }
}

#[tracing::instrument(level = "DEBUG", err)]
pub async fn control_processor(_expected_remote_addr: IpAddr, socket: UdpSocket) -> io::Result<()> {
    const BUF_SIZE: usize = 16 * 1024;

    let mut buf = [0u8; BUF_SIZE];
    loop {
        let _pkt_len = socket.recv(&mut buf).await?;
    }
}

const VIDEO_HEADER_LEN: usize = 128;
const CODEC_CONFIGURATION_PACKET_TYPE: u16 = 1;
const STREAM_SUSPEND_OPTIONS: [u16; 2] = [0x0156, 0x015e]; // h264 & hevc suspend
const STREAM_RESUME_OPTIONS: [u16; 2] = [0x0116, 0x011e]; // h264 & hevc resume

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoHeader {
    payload_len: u32,
    packet_type: u16,
    stream_option: u16,
    timestamp: u64,
}

impl VideoHeader {
    fn decode(raw: &[u8; VIDEO_HEADER_LEN]) -> Self {
        let mut bytes = &raw[..];
        Self {
            payload_len: bytes.get_u32_le(),
            packet_type: bytes.get_u16_le(),
            stream_option: bytes.get_u16_le(),
            timestamp: bytes.get_u64_le(),
        }
    }

    fn announced_stream_state(self) -> Option<VideoStreamState> {
        if self.packet_type != CODEC_CONFIGURATION_PACKET_TYPE {
            return None;
        }

        if STREAM_SUSPEND_OPTIONS.contains(&self.stream_option) {
            Some(VideoStreamState::Suspended)
        } else if STREAM_RESUME_OPTIONS.contains(&self.stream_option) {
            Some(VideoStreamState::Active)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum VideoStreamState {
    #[default]
    Active,
    Suspended,
}

impl VideoStreamState {
    fn transition(&mut self, header: VideoHeader) -> Option<VideoStreamEvent> {
        let next = header.announced_stream_state()?;
        if *self == next {
            return None;
        }

        *self = next;
        Some(match next {
            Self::Active => VideoStreamEvent::Resume,
            Self::Suspended => VideoStreamEvent::Suspend,
        })
    }
}

#[tracing::instrument(level = "DEBUG", skip(stream))]
pub async fn video_processor(
    mut tcp_stream: TcpStream,
    stream: &impl VideoStream,
    video_buf_size: u32,
    encryption: Encryption,
) -> io::Result<()> {
    let mut video_buf = memory::BytesHunk::new(video_buf_size as usize);
    let mut cipher = build_video_cipher(&encryption);
    let mut stream_state = VideoStreamState::default();

    loop {
        async {
            let mut raw_header = [0u8; VIDEO_HEADER_LEN];
            tcp_stream.read_exact(&mut raw_header).await?;

            let header = VideoHeader::decode(&raw_header);
            let mut payload = video_buf.allocate_buf(header.payload_len as usize);
            tcp_stream.read_exact(&mut payload).await?;
            let kind = match header.packet_type {
                CODEC_CONFIGURATION_PACKET_TYPE => {
                    if payload.len() >= 8 && &payload[4..8] == b"hvc1" {
                        PacketKind::Hvc1
                    } else {
                        PacketKind::AvcC
                    }
                }
                0 | 4096 => PacketKind::Payload,
                5 => PacketKind::Plist,
                other => PacketKind::Other(other),
            };
            let stream_event = stream_state.transition(header);
            tracing::trace!(
                ?kind,
                ?stream_event,
                timestamp = header.timestamp,
                option = format_args!("{:#06x}", header.stream_option),
                payload_len = header.payload_len,
                "packet read"
            );

            if let Some(event) = stream_event {
                stream.on_data(VideoStreamMessage::Event(event));
                if event == VideoStreamEvent::Suspend {
                    tokio::task::consume_budget().await;
                    return io::Result::Ok(());
                }
            }

            let mut pkt = VideoPacket {
                kind,
                timestamp: header.timestamp,
                payload,
            };

            // Only payload need to be decrypted
            // TODO: Other(_) too?
            if matches!(kind, PacketKind::Payload) {
                if cipher.decrypt(raw_header, &mut pkt.payload).is_ok() {
                    tracing::trace!("packet decrypted");
                } else {
                    tracing::warn!("packet decryption failed");
                }
            }

            stream.on_data(VideoStreamMessage::Packet(pkt));
            tokio::task::consume_budget().await;

            io::Result::Ok(())
        }
        .instrument(tracing::debug_span!("packet.video"))
        .await?;
    }
}

fn build_audio_cipher(encryption: &Encryption) -> Box<dyn crypto::AudioCipher + Send + Sync> {
    match encryption {
        Encryption::ChaCha { key } => Box::new(crypto::ChachaAudioCipher::from_key(*key)),
        Encryption::HomeKit {
            key,
            stream_connection_id,
        } => Box::new(crypto::ChachaAudioCipher::from_secret_and_id(
            &key.key_material,
            *stream_connection_id,
        )),
        Encryption::Legacy { key, iv, .. } => Box::new(crypto::AesAudioCipher::new(*key, *iv)),
    }
}

fn build_video_cipher(encryption: &Encryption) -> Box<dyn crypto::VideoCipher + Send + Sync> {
    match encryption {
        Encryption::HomeKit {
            key,
            stream_connection_id,
        } => Box::new(crypto::ChachaVideoCipher::from_secret_and_id(
            &key.key_material,
            *stream_connection_id,
        )),
        Encryption::Legacy {
            key,
            stream_connection_id: Some(stream_connection_id),
            ..
        } => Box::new(crypto::AesVideoCipher::from_key_and_id(
            *key,
            *stream_connection_id,
        )),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod video_tests {
    use std::{error::Error, sync::Mutex};

    use tokio::{io::AsyncWriteExt, net::TcpListener};

    use super::{
        CODEC_CONFIGURATION_PACKET_TYPE, Encryption, STREAM_RESUME_OPTIONS, STREAM_SUSPEND_OPTIONS,
        VIDEO_HEADER_LEN, VideoHeader, VideoStreamState, video_processor,
    };
    use crate::playback::{
        Stream,
        video::{PacketKind, VideoStreamEvent, VideoStreamMessage},
    };

    fn header(packet_type: u16, stream_option: u16) -> VideoHeader {
        VideoHeader {
            payload_len: 0,
            packet_type,
            stream_option,
            timestamp: 0,
        }
    }

    #[test]
    fn decodes_all_little_endian_header_fields() {
        let mut raw = [0; VIDEO_HEADER_LEN];
        raw[..16].copy_from_slice(&[
            0x04, 0x03, 0x02, 0x01, 0x06, 0x05, 0x08, 0x07, 0x10, 0x0f, 0x0e, 0x0d, 0x0c, 0x0b,
            0x0a, 0x09,
        ]);

        assert_eq!(
            VideoHeader::decode(&raw),
            VideoHeader {
                payload_len: 0x0102_0304,
                packet_type: 0x0506,
                stream_option: 0x0708,
                timestamp: 0x090a_0b0c_0d0e_0f10,
            }
        );
    }

    #[test]
    fn every_known_suspend_and_resume_option_emits_a_transition() {
        for suspend_option in STREAM_SUSPEND_OPTIONS {
            for resume_option in STREAM_RESUME_OPTIONS {
                let mut state = VideoStreamState::default();

                assert_eq!(
                    state.transition(header(CODEC_CONFIGURATION_PACKET_TYPE, suspend_option)),
                    Some(VideoStreamEvent::Suspend)
                );
                assert_eq!(state, VideoStreamState::Suspended);
                assert_eq!(
                    state.transition(header(CODEC_CONFIGURATION_PACKET_TYPE, resume_option)),
                    Some(VideoStreamEvent::Resume)
                );
                assert_eq!(state, VideoStreamState::Active);
            }
        }
    }

    #[test]
    fn duplicate_and_unrelated_announcements_do_not_emit_transitions() {
        let mut state = VideoStreamState::default();

        assert_eq!(
            state.transition(header(
                CODEC_CONFIGURATION_PACKET_TYPE,
                STREAM_RESUME_OPTIONS[0]
            )),
            None
        );
        assert_eq!(state.transition(header(0, STREAM_SUSPEND_OPTIONS[0])), None);
        assert_eq!(
            state.transition(header(CODEC_CONFIGURATION_PACKET_TYPE, 0xffff)),
            None
        );

        assert_eq!(
            state.transition(header(
                CODEC_CONFIGURATION_PACKET_TYPE,
                STREAM_SUSPEND_OPTIONS[0]
            )),
            Some(VideoStreamEvent::Suspend)
        );
        assert_eq!(
            state.transition(header(
                CODEC_CONFIGURATION_PACKET_TYPE,
                STREAM_SUSPEND_OPTIONS[1]
            )),
            None
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    enum RecordedMessage {
        Packet(PacketKind),
        Event(VideoStreamEvent),
    }

    #[derive(Default)]
    struct RecordingStream(Mutex<Vec<RecordedMessage>>);

    impl Stream for RecordingStream {
        type Content = VideoStreamMessage;

        fn on_data(&self, content: Self::Content) {
            let message = match content {
                VideoStreamMessage::Packet(packet) => RecordedMessage::Packet(packet.kind),
                VideoStreamMessage::Event(event) => RecordedMessage::Event(event),
            };
            self.0.lock().unwrap().push(message);
        }

        fn on_ok(self) {}

        fn on_err(self, _err: Box<dyn Error>) {}
    }

    async fn write_frame(stream: &mut tokio::net::TcpStream, stream_option: u16, timestamp: u64) {
        let payload = [0, 0, 0, 0, b'a', b'v', b'c', b'C'];
        let mut header = [0; VIDEO_HEADER_LEN];
        header[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[4..6].copy_from_slice(&CODEC_CONFIGURATION_PACKET_TYPE.to_le_bytes());
        header[6..8].copy_from_slice(&stream_option.to_le_bytes());
        header[8..16].copy_from_slice(&timestamp.to_le_bytes());
        stream.write_all(&header).await.unwrap();
        stream.write_all(&payload).await.unwrap();
    }

    #[tokio::test]
    async fn processor_delivers_control_and_packet_messages_in_canonical_order() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut sender = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (receiver, _) = listener.accept().await.unwrap();

        write_frame(&mut sender, STREAM_SUSPEND_OPTIONS[0], 1).await;
        write_frame(&mut sender, STREAM_RESUME_OPTIONS[0], 2).await;
        sender.shutdown().await.unwrap();

        let stream = RecordingStream::default();
        let result = video_processor(
            receiver,
            &stream,
            1024,
            Encryption::Legacy {
                key: [0; 16],
                iv: [0; 16],
                stream_connection_id: Some(0),
            },
        )
        .await;

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            stream.0.lock().unwrap().as_slice(),
            [
                RecordedMessage::Event(VideoStreamEvent::Suspend),
                RecordedMessage::Event(VideoStreamEvent::Resume),
                RecordedMessage::Packet(PacketKind::AvcC),
            ]
        );
    }
}
