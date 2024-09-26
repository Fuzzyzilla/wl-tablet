//! An `eframe` app for displaying all of the data this crate provides access too. Great
//! for testing (both hardware and software) and exploring the capabilities of devices.
//!
//! With such a wide scope, this example is *quite large* and much more than is needed for simple
//! integration with the octotablet crate - For a more minimal example, see the `winit-paint` example.

use eframe::{
    egui::{self, Frame, RichText},
    epaint::{Color32, Vec2},
    CreationContext,
};
use octotablet::{
    builder::{BuildError, Builder},
    Manager,
};

mod pretty;
mod state;
use state::State;

fn main() {
    let native_options = eframe::NativeOptions {
        persist_window: false,
        viewport: egui::ViewportBuilder::default().with_inner_size(Vec2 { x: 800.0, y: 500.0 }),
        // Im stupid and don't want to figure out how to make the
        // colors dynamic, they only work on Dark lol
        default_theme: eframe::Theme::Dark,
        follow_system_theme: false,
        ..Default::default()
    };
    // Startup!
    eframe::run_native(
        "octotablet viewer",
        native_options,
        Box::new(|context| Box::new(Viewer::new(context))),
    )
    .unwrap();
}

#[derive(Copy, Clone)]
struct EventFilter {
    frames: bool,
    poses: bool,
}
impl EventFilter {
    fn passes(self, event: &octotablet::events::Event) -> bool {
        use octotablet::events::{Event, PadEvent, PadGroupEvent, ToolEvent, TouchStripEvent};
        match event {
            Event::Tool { event, .. } => match event {
                ToolEvent::Pose(..) => self.poses,
                ToolEvent::Frame(..) => self.frames,
                _ => true,
            },
            Event::Tablet { .. } => true,
            Event::Pad { event, .. } => match event {
                PadEvent::Group {
                    event: PadGroupEvent::Ring { event, .. } | PadGroupEvent::Strip { event, .. },
                    ..
                } => match event {
                    TouchStripEvent::Frame(..) => self.frames,
                    TouchStripEvent::Pose(..) => self.poses,
                    _ => true,
                },
                _ => true,
            },
        }
    }
}

enum EventMessage {
    String((String, Color32)),
    Filtered(usize),
}

/// Main app, displaying info, raw event stream, and a [test area](State).
struct Viewer {
    manager: Result<Manager, BuildError>,
    // Allow code to set a duration to poll events more rapidly.
    // Egui does not automatically update when the tablet does!
    poll_until: std::time::Instant,
    // Show a toggle for whether to show the event queue, off by default.
    show_events: bool,
    // The event queue in question:
    events_stream: std::collections::VecDeque<EventMessage>,
    event_filter: EventFilter,
    state: State,
}
impl Viewer {
    fn new(context: &CreationContext<'_>) -> Self {
        let now = std::time::Instant::now();

        // Prepare to create a tablet manager:
        let config = Builder::new()
            // Include mouse devices as tools in the listing
            .emulate_tool_from_mouse(true);

        // Context gives us access to the handle, connect to the tablet server:
        Self {
            // Safety: Destroyed in `on_exit`, before we lose the display.
            manager: unsafe { config.build_raw(context) },
            poll_until: now,
            show_events: false,
            events_stream: std::collections::VecDeque::with_capacity(128),
            event_filter: EventFilter {
                frames: false,
                poses: false,
            },
            state: State::default(),
        }
    }
}
impl eframe::App for Viewer {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Drop the tablet, since our connection to the server is soon over.
        // Replace with dummy err.
        self.manager = Err(BuildError::Unsupported);
    }
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Access tablets, or show a message and bail if failed.
        let manager = match &mut self.manager {
            Ok(t) => t,
            Err(e) => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(
                        RichText::new(format!("Failed to acquire connection: {e}"))
                            .monospace()
                            .heading(),
                    )
                });
                return;
            }
        };
        let events = manager.pump().unwrap();
        // If at least one event...
        let has_events = events.into_iter().next().is_some();
        // Poll more often for a while!
        let poll = if has_events {
            // Set up to poll for an additional 500ms afterwards, to catch future events.
            self.poll_until = std::time::Instant::now() + std::time::Duration::from_millis(500);
            true
        } else {
            // No new events. Still poll if we're in the time frame set before:
            self.poll_until > std::time::Instant::now()
        };

        // If an interaction is ongoing, request redraws often.
        if poll {
            // Arbitrary fast poll time, without being so fast as to gobble up the CPU.
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(1.0 / 60.0));
        } else {
            // poll for new events slower. (Egui will not necessarily notice the tablet input and so won't repaint on its own!)
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        // Format events.
        for event in events {
            if self.event_filter.passes(&event) {
                // Show this event
                if self.events_stream.len() == self.events_stream.capacity() {
                    // remove top
                    let _ = self.events_stream.pop_front();
                }
                // add pretty-print at bottom!
                self.events_stream
                    .push_back(EventMessage::String(pretty::format_event(event)));
            } else {
                // Fildered out.
                if let Some(EventMessage::Filtered(ref mut num)) = self.events_stream.back_mut() {
                    // Already a filter message at the end, inc it.
                    *num = num.saturating_add(1);
                } else {
                    if self.events_stream.len() == self.events_stream.capacity() {
                        // remove top
                        let _ = self.events_stream.pop_front();
                    }
                    // add pretty-print at bottom!
                    self.events_stream.push_back(EventMessage::Filtered(1));
                }
            }
        }
        // Show an area to print events, if requested.
        egui::SidePanel::right("events").show_animated(ctx, self.show_events, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.event_filter.poses, "Show poses");
                ui.add(egui::Separator::default().vertical());
                ui.checkbox(&mut self.event_filter.frames, "Show frames");
            });
            // Show events:
            egui::ScrollArea::new([false, true])
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("events-grid")
                        .num_columns(1)
                        .striped(true)
                        .show(ui, |ui| {
                            for event in &self.events_stream {
                                match event {
                                    EventMessage::String((str, color)) => {
                                        ui.label(RichText::new(str).monospace().color(*color));
                                    }
                                    EventMessage::Filtered(num) => {
                                        ui.label(
                                            RichText::new(format!("{num} filtered messages."))
                                                .weak()
                                                .italics(),
                                        );
                                    }
                                }
                                ui.end_row();
                            }
                        });
                });
        });

        // Set the scale factor. Notably, this does *not* include the window's scale factor!
        self.state.egui_scale_factor = ctx.zoom_factor();
        // update the state with the new events!
        self.state.extend(events);

        // Display the state:
        egui::TopBottomPanel::bottom("viewer")
            .exact_height(ctx.available_rect().height() / 2.0)
            .frame(Frame::canvas(&ctx.style()))
            .show(ctx, |ui| ui.add(self.state.visualize(manager)));

        // Show an info panel listing connected devices and their capabilities
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    // Heading
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Octotablet viewer ~ Connected! 🐙").heading());
                        ui.checkbox(&mut self.show_events, "Show Events stream...")
                    });
                    ui.separator();

                    ui.label("Tablets");
                    for tablet in manager.tablets() {
                        egui::CollapsingHeader::new(pretty::name_tablet(tablet)).show(ui, |ui| {
                            // Pretty-print the USBID
                            ui.label(
                                RichText::new(pretty::format_usb_id(tablet.usb_id)).monospace(),
                            )
                        });
                    }
                    if manager.tablets().is_empty() {
                        ui.label(RichText::new("No tablets...").weak());
                    }

                    ui.separator();

                    ui.label("Pads");
                    for (idx, pad) in manager.pads().iter().enumerate() {
                        ui.collapsing(idx.to_string(), |ui| {
                            ui.label(format!("Total Buttons: {}", pad.total_buttons));
                            for (idx, group) in pad.groups.iter().enumerate() {
                                egui::CollapsingHeader::new(format!("Group {idx}"))
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        ui.label(format!(
                                            "Mode count: {:?}",
                                            group.mode_count.map(std::num::NonZeroU32::get)
                                        ));
                                        ui.label(format!(
                                            "Associated button indices: {:?}",
                                            &group.buttons
                                        ));
                                        // Show rings
                                        egui::CollapsingHeader::new(format!(
                                            "Rings ({})",
                                            group.rings.len()
                                        ))
                                        .default_open(true)
                                        .enabled(!group.rings.is_empty())
                                        .show(ui, |ui| {
                                            for ring in &group.rings {
                                                ui.label(format!("{ring:#?}"));
                                            }
                                        });
                                        // Show strips
                                        egui::CollapsingHeader::new(format!(
                                            "Strips ({})",
                                            group.strips.len()
                                        ))
                                        .default_open(true)
                                        .enabled(!group.strips.is_empty())
                                        .show(ui, |ui| {
                                            for strip in &group.strips {
                                                ui.label(format!("{strip:#?}"));
                                            }
                                        });
                                    });
                            }
                        });
                    }
                    if manager.pads().is_empty() {
                        ui.label(RichText::new("No pads...").weak());
                    }

                    ui.separator();

                    ui.label("Tools");
                    for (idx, tool) in manager.tools().iter().enumerate() {
                        egui::CollapsingHeader::new(pretty::name_tool(tool))
                            .id_source((tool.hardware_id, tool.wacom_id, idx))
                            .show(ui, |ui| {
                                ui.label(format!("Wacom ID: {:08X?}", tool.wacom_id,));
                                ui.label(format!(" ✅ Position: {:?}", tool.axes.position));
                                for axis in
                                    <octotablet::axis::Axis as strum::IntoEnumIterator>::iter()
                                {
                                    // Todo: list units
                                    if let Ok((limits, granularity)) =
                                        tool.axes.limits(axis).and_then(|limits| {
                                            Ok((limits, tool.axes.granularity(axis)?))
                                        })
                                    {
                                        ui.label(format!(
                                            " ✅ {}: {}, {}",
                                            axis.as_ref(),
                                            match limits {
                                                Some(limits) => format!("{limits:?}"),
                                                None => "Unknown range".to_owned(),
                                            },
                                            match granularity {
                                                Some(granularity) => format!("{granularity:?}"),
                                                None => "Unknown granularity".to_owned(),
                                            },
                                        ));
                                    } else {
                                        ui.label(
                                            RichText::new(format!(
                                                " 🗙 {}: Unsupported",
                                                axis.as_ref()
                                            ))
                                            .weak()
                                            .italics(),
                                        );
                                    }
                                }
                            });
                    }
                    if manager.tools().is_empty() {
                        ui.label(RichText::new("No tools...").weak());
                    }

                    ui.separator();
                });
        });
    }
}
