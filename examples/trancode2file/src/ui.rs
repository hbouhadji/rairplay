use std::sync::Arc;

use async_channel::{Receiver, Sender, TrySendError};
use gpui::{
    AnyElement, App, Application, AsyncApp, Bounds, Context, ObjectFit, Render, RenderImage,
    WeakEntity, Window, WindowBounds, WindowOptions, div, img, prelude::*, px, size,
};
use image::{Frame as ImageFrame, ImageBuffer, Rgba};
use smallvec::SmallVec;

#[derive(Clone)]
pub struct FrameSink {
    tx: Sender<VideoFrame>,
}

impl FrameSink {
    pub fn send(&self, frame: VideoFrame) {
        if let Err(err) = self.tx.try_send(frame) {
            match err {
                TrySendError::Full(_) => {
                    tracing::debug!("dropping video frame (UI is catching up)");
                }
                TrySendError::Closed(_) => {
                    tracing::warn!("video window closed, dropping frame");
                }
            }
        }
    }
}

pub fn video_channel() -> (FrameSink, Receiver<VideoFrame>) {
    let (tx, rx) = async_channel::bounded(2);
    (FrameSink { tx }, rx)
}

#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl VideoFrame {
    fn into_render_image(self) -> Option<(Arc<RenderImage>, u32, u32)> {
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(self.width, self.height, self.data)?;
        let mut frames: SmallVec<[ImageFrame; 1]> = SmallVec::new();
        frames.push(ImageFrame::new(buffer));
        Some((Arc::new(RenderImage::new(frames)), self.width, self.height))
    }
}

pub fn run_video_window(frame_rx: Receiver<VideoFrame>) {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: true,
                show: true,
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title("AirPlay Preview");
                let rx = frame_rx;
                cx.new(|cx| VideoView::new(window, rx, cx))
            },
        )
        .expect("failed to open GPUI window");
        cx.activate(true);
    });
}

struct VideoView {
    latest_frame: Option<Arc<RenderImage>>,
    latest_dims: Option<(u32, u32)>,
}

impl VideoView {
    fn new(window: &mut Window, frame_rx: Receiver<VideoFrame>, cx: &mut Context<Self>) -> Self {
        cx.observe_window_bounds(window, |view, window, _| {
            view.align_window(window);
        })
        .detach();

        cx.spawn(move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut app = cx.clone();
            async move {
                let frames = frame_rx;
                while let Ok(frame) = frames.recv().await {
                    let Some((image, width, height)) = frame.into_render_image() else {
                        continue;
                    };

                    if view
                        .update(&mut app, |view, cx| {
                            view.latest_frame = Some(image.clone());
                            view.latest_dims = Some((width, height));
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
        .detach();

        Self {
            latest_frame: None,
            latest_dims: None,
        }
    }

    fn align_window(&self, window: &mut Window) {
        let Some((width, height)) = self.latest_dims else {
            return;
        };
        if height == 0 {
            return;
        }

        let target_ratio = width as f32 / height as f32;
        let bounds = window.bounds();
        let current_width: f32 = bounds.size.width.into();
        let current_height: f32 = bounds.size.height.into();
        if current_width <= 0.0 || current_height <= 0.0 {
            return;
        }

        let current_ratio = current_width / current_height;
        if (current_ratio - target_ratio).abs() > 0.01 {
            let new_height = current_width / target_ratio;
            window.resize(size(px(current_width), px(new_height)));
        }
    }
}

impl Render for VideoView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.align_window(window);

        let content: AnyElement = if let Some(image) = &self.latest_frame {
            img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .id("video-preview")
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_color(gpui::white())
                .text_xl()
                .child("En attente du flux vidéo…")
                .id("video-waiting")
                .into_any_element()
        };

        div()
            .id("video-root")
            .size_full()
            .bg(gpui::black())
            .child(content)
    }
}
