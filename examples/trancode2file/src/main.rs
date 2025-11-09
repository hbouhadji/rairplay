use std::sync::Arc;

use tokio::net::TcpListener;
use tracing_chrome::ChromeLayerBuilder;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod audio;
mod discovery;
mod playback;
mod transport;
mod ui;
mod video;

fn main() {
    let (chrome_layer, _guard) = ChromeLayerBuilder::new().build();
    tracing_subscriber::registry().with(chrome_layer).init();

    gstreamer::init().expect("gstreamer initialization");

    let (frame_sink, frame_rx) = ui::video_channel();
    video::register_frame_sink(frame_sink);

    let config = Arc::new(airplay::config::Config::<_, _> {
        name: "rairplay".to_string(),
        video: airplay::config::Video {
            device: playback::PipeDevice {
                callback: video::transcode,
            },
            width: 3840,
            height: 2160,
            fps: 60,
            ..Default::default()
        },
        audio: airplay::config::Audio {
            device: playback::PipeDevice {
                callback: audio::transcode,
            },
            ..Default::default()
        },
        ..Default::default()
    });

    spawn_airplay_server(config);

    ui::run_video_window(frame_rx);
}

fn spawn_airplay_server<ADev, VDev>(config: Arc<airplay::config::Config<ADev, VDev>>)
where
    ADev: Send + Sync + 'static + airplay::playback::audio::AudioDevice,
    VDev: Send + Sync + 'static + airplay::playback::video::VideoDevice,
{
    std::thread::Builder::new()
        .name("airplay-runtime".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(async move {
                discovery::mdns_broadcast(config.as_ref());

                let tcp_listener = TcpListener::bind("0.0.0.0:5200").await.unwrap();
                axum::serve(
                    transport::RtspListener { tcp_listener },
                    airplay::rtsp::RtspService { config },
                )
                .await
                .unwrap();
            });
        })
        .expect("airplay runtime thread");
}
