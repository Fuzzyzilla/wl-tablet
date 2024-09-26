//! Simple winit example, using tiny-skia for drawing.
//! Run under `--release` or it's horrifyingly slow!

use octotablet::builder::Builder;
use winit::dpi::PhysicalSize;

/// Paint every `PAINT_SPACING_PX` during a stroke.
const PAINT_SPACING_PX: f32 = 5.0;

/// Painter for an individual tool interaction
#[derive(Default)]
struct ToolPainter {
    /// Whether the tool should erase ink instead of placing it.
    is_eraser: bool,
    /// Position at the end of last frame, for connecting previous data to new data.
    /// None if start of frame.
    previous_pose: Option<octotablet::axis::Pose>,
    /// The path being traced
    builder: tiny_skia::PathBuilder,
    /// Length of the painted stroke, modulo [`PAINT_SPACING_PX`]
    arc_len_modulo: f32,
}
impl ToolPainter {
    fn pressure_to_radius(pressure: f32) -> f32 {
        const BRUSH_SIZE: f32 = 15.0;
        // Could perform some curves in here!
        pressure * BRUSH_SIZE
    }
    // Linearly interpolate two points `(position, pressure)` across `t` in `[0, 1]`
    fn lerp_pose(from: ([f32; 2], f32), to: ([f32; 2], f32), t: f32) -> ([f32; 2], f32) {
        let inv_t = 1.0 - t;
        (
            [
                from.0[0] * inv_t + to.0[0] * t,
                from.0[1] * inv_t + to.0[1] * t,
            ],
            from.1 * inv_t + to.1 * t,
        )
    }

    /// Add a pose to the painter.
    fn push_pose(&mut self, pose: octotablet::axis::Pose) {
        // Update and take old pose
        let Some(previous_pose) = self.previous_pose.replace(pose) else {
            // We need before and after to perform the draw - nothing to do here.
            return;
        };

        // We want to place a circle at even intervals during the length of the drawn path.
        // However, since we only get data in bursts and not all at once, extra bookkeeping is needed
        // to allow this to happen!
        let delta = [
            previous_pose.position[0] - pose.position[0],
            previous_pose.position[1] - pose.position[1],
        ];

        // Reduce poses to the info we care about for this simple example
        // (position, pressure)
        // If pressure isn't supported by the device, then assume 100% pressure.
        let previous_pose = (
            previous_pose.position,
            previous_pose.pressure.get().unwrap_or(1.0),
        );
        let pose = (pose.position, pose.pressure.get().unwrap_or(1.0));

        let length = ((delta[0] * delta[0]) + (delta[1] * delta[1])).sqrt();

        if length > (PAINT_SPACING_PX - self.arc_len_modulo) {
            // Enough length to draw!
            // First dot location, in [0, 1] where 0 is previous and 1 is next
            let mut t = (PAINT_SPACING_PX - self.arc_len_modulo) / length;
            // How much percentage each dot advances by
            let delta_t = PAINT_SPACING_PX / length;

            // Draw circles every PAINT_SPACING_PX until we surpass 100%
            while t <= 1.0 {
                let (pos, pressure) = Self::lerp_pose(previous_pose, pose, t);
                let size = Self::pressure_to_radius(pressure);
                self.builder.push_circle(pos[0], pos[1], size);
                t += delta_t;
            }

            // Accumulate the arclength
            self.arc_len_modulo += length;
            self.arc_len_modulo %= PAINT_SPACING_PX;
        } else {
            // Too short to draw..
            // Accumulate the arclength
            self.arc_len_modulo += length;
        }
    }
    /// Consume the pending drawings, rendering into the given pixmap.
    /// Returns true to request a redraw.
    fn consume_draw(&mut self, pixmap: &mut tiny_skia::Pixmap) -> bool {
        // Consume path
        let Some(path) = std::mem::take(&mut self.builder).finish() else {
            // Empty path, no draw!
            return false;
        };
        // Draw!

        // If the tool is known to be an eraser, erase!
        let color = if self.is_eraser {
            // mwuhahahaha, clear with black instead of actually reducing opacity.
            tiny_skia::Color::BLACK
        } else {
            tiny_skia::Color::WHITE
        };

        pixmap.fill_path(
            &path,
            &tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(color),
                blend_mode: tiny_skia::BlendMode::Source,
                anti_alias: false,
                force_hq_pipeline: false,
            },
            // Circles may overlap, EvenOdd would give weird XOR results!
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        // We changed the buffer, request redraw for this frame.
        true
    }
}

/// Painter container, dispatches octotablet events to the sub-painters to render the image
struct Painter {
    scale_factor: f32,
    tools: std::collections::HashMap<octotablet::tool::ID, ToolPainter>,
}

impl Painter {
    /// Paint from the events stream. Returns true to request a redraw.
    fn paint<'a>(
        &mut self,
        events: impl IntoIterator<Item = octotablet::events::Event<'a>>,
        pixmap: &mut tiny_skia::Pixmap,
    ) -> bool {
        // Track every draw event, if any of them request redraw then forward that.
        let mut needs_redraw = false;

        for event in events {
            use octotablet::events::{Event, ToolEvent};
            if let Event::Tool { tool, event } = event {
                match event {
                    // Start drawing!
                    ToolEvent::Down => {
                        // Create a painter.
                        self.tools.entry(tool.id()).or_insert_with(|| ToolPainter {
                            // Mark eraser-type tools to erase
                            is_eraser: matches!(
                                tool.tool_type,
                                Some(octotablet::tool::Type::Eraser)
                            ),
                            ..Default::default()
                        });
                    }
                    // Positioning data, continue drawing!
                    ToolEvent::Pose(mut pose) => {
                        println!("{:?} - x {}", tool.name, pose.position[0]);
                        // If there's a painter, paint on it!
                        // If not, we haven't hit the `Down` event yet.
                        if let Some(painter) = self.tools.get_mut(&tool.id()) {
                            // Octotablet works in "logical pixels", but our tiny_skia renderer needs
                            // "physical pixels". Multiplying by the window's scale factor performs
                            // that conversion.
                            pose.position = [
                                pose.position[0] * self.scale_factor,
                                pose.position[1] * self.scale_factor,
                            ];

                            painter.push_pose(pose);
                        }
                    }
                    // At any of these events, finish the drawing.
                    ToolEvent::Removed | ToolEvent::Out | ToolEvent::Up => {
                        // Remove the painter for this tool, and allow it to draw before drop.
                        if let Some(mut painter) = self.tools.remove(&tool.id()) {
                            needs_redraw |= painter.consume_draw(pixmap);
                        }
                    }
                    _ => (),
                }
            }
        }

        // Draw any pending painters but keep them around.
        // This allows for the ink to be seen immediately.
        for painter in self.tools.values_mut() {
            needs_redraw |= painter.consume_draw(pixmap);
        }

        needs_redraw
    }
}

fn main() {
    let event_loop = winit::event_loop::EventLoopBuilder::<()>::default()
        .build()
        .expect("start event loop");
    event_loop.listen_device_events(winit::event_loop::DeviceEvents::Always);
    let window = std::sync::Arc::new(
        winit::window::WindowBuilder::default()
            .with_inner_size(PhysicalSize::new(512u32, 512u32))
            .with_title("octotablet paint demo")
            .build(&event_loop)
            .expect("create window"),
    );

    // To allow us to draw on the screen without pulling in a whole GPU package,
    // we use `softbuffer` for presentation and `tiny-skia` for drawing
    let mut pixmap =
        tiny_skia::Pixmap::new(window.inner_size().width, window.inner_size().height).unwrap();
    let softbuffer = softbuffer::Context::new(window.as_ref()).expect("init softbuffer");
    let mut surface =
        softbuffer::Surface::new(&softbuffer, &window).expect("make presentation surface");
    surface
        .resize(
            window.inner_size().width.try_into().unwrap(),
            window.inner_size().height.try_into().unwrap(),
        )
        .unwrap();

    // Fetch the tablets, using our window's handle for access.
    // Since we `Arc'd` our window, we get the safety of `build_shared`. Where this is not possible,
    // `build_raw` is available as well!
    let mut manager = Builder::default()
        .build_shared(&window)
        .expect("connect to stylus server");

    // Make a painter to turn the stylus events into pretty pictures!
    let mut painter = Painter {
        tools: std::collections::HashMap::new(),
        // Winit doesn't notify for the initial scale factor, query directly!
        scale_factor: window.scale_factor() as f32,
    };

    // Whether the winit loop should poll more often (during a stylus interaction)
    let mut should_poll = false;

    // Let winit manage its messages....
    event_loop
        .run(|e, target| {
            use winit::event::*;

            // We must poll occasionally for tablet events. Winit will not wake up automatically on stylus events since
            // octotablet uses a separate event loop!
            target.set_control_flow(winit::event_loop::ControlFlow::wait_duration(
                // Poll more often during an interaction.
                std::time::Duration::from_millis(if should_poll { 10 } else { 100 }),
            ));

            match e {
                Event::WindowEvent { event, .. } => {
                    match event {
                        // Esc pressed or system-specific close event.
                        WindowEvent::KeyboardInput {
                            event:
                                winit::event::KeyEvent {
                                    physical_key:
                                        winit::keyboard::PhysicalKey::Code(
                                            winit::keyboard::KeyCode::Escape,
                                        ),
                                    state: winit::event::ElementState::Pressed,
                                    ..
                                },
                            ..
                        }
                        | WindowEvent::CloseRequested => target.exit(),
                        WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                            painter.scale_factor = scale_factor as f32
                            // Window resize event occurs next and that handles image resize.
                        }
                        WindowEvent::Resized(size) => {
                            // Make a new map
                            let mut new_map =
                                tiny_skia::Pixmap::new(size.width, size.height).unwrap();
                            // copy the old onto it.
                            new_map.draw_pixmap(
                                0,
                                0,
                                pixmap.as_ref(),
                                &tiny_skia::PixmapPaint {
                                    opacity: 1.0,
                                    blend_mode: tiny_skia::BlendMode::Source,
                                    // No resizing is taking place.
                                    quality: tiny_skia::FilterQuality::Nearest,
                                },
                                tiny_skia::Transform::identity(),
                                None,
                            );
                            // Replace map, inform the surface of the change, and redraw.
                            pixmap = new_map;
                            surface
                                .resize(
                                    size.width.try_into().unwrap(),
                                    size.height.try_into().unwrap(),
                                )
                                .unwrap();
                            window.request_redraw();
                        }
                        WindowEvent::RedrawRequested => {
                            // Copy skia bitmap into the framebufer and present.
                            let mut buffer = surface.buffer_mut().expect("fetch draw buffer");
                            buffer.iter_mut().zip(pixmap.pixels().iter()).for_each(
                                |(into, from)| {
                                    // This is a premul color. Treat it as composited atop black,
                                    // which has the nice side effect of requiring literally no work
                                    // other than setting alpha to 1 :D
                                    let r = from.red();
                                    let g = from.green();
                                    let b = from.blue();
                                    // softbuffer requires `0000'0000'rrrr'rrrr'gggg'gggg'bbbb'bbbb` format
                                    *into =
                                        (u32::from(r) << 16) | (u32::from(g) << 8) | (u32::from(b));
                                },
                            );

                            window.pre_present_notify();
                            buffer.present().expect("present");
                        }
                        _ => (),
                    }
                }
                Event::AboutToWait => {
                    // Finished events from Winit, parse events from octotablet.
                    // Accept all new messages from the stylus server, draw with them!
                    let events = manager.pump().expect("octotablet event pump");

                    // Has events - mark to pull more often if so!
                    should_poll = events.into_iter().next().is_some();

                    // Perform painting...
                    let needs_present = painter.paint(events, &mut pixmap);

                    // Something was drawn in the buffer, request present.
                    if needs_present {
                        window.request_redraw();
                    }
                }
                // Other winit events..
                _ => (),
            }
        })
        .expect("winit event loop");
}
