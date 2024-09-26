//! Parses instantaneous events into a description of interaction state.
//!
//! From this data, provides a visualization through egui.
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Shape, Stroke, Vec2};
use octotablet::{
    axis::{AvailableAxes, Pose},
    events::{Event, PadEvent, PadGroupEvent, TabletEvent, ToolEvent, TouchStripEvent},
    pad, tablet, tool,
};
use std::collections;

/// Maximum length of tool path history visualization.
const PATH_LEN: usize = 64;

struct ToolState {
    over: tablet::ID,
    path: collections::VecDeque<Pos2>,
    now: Pose,
    down: bool,
}

/// Fetch the difference between two angles in `[0, TAU)`
/// This requires extra logic to find the delta when the state crosses the modulous point.
fn radial_delta(from: f32, to: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    // Check the actual `to` position, as well as the next closest one to `from`
    let rotations = [to, if from > PI { to + TAU } else { to - TAU }];

    // Find variation with smallest absolute delta.
    // This is to handle the continuity break from jumping from the end of one rotation
    // to the start of the next.
    let nearest_delta = rotations
        .iter()
        .map(|rot| from - rot)
        .min_by(|&a, &b| (a.abs()).total_cmp(&b.abs()));

    // Unwrap OK - this array is always non-empty of course!
    nearest_delta.unwrap()
}

pub struct State {
    /// State of any `In` tools, removed when they go `Out`.
    /// Note that this isn't a singleton! Several tools can be active at the same time
    /// (*usually* only one per tablet due to the way the digitizers work)
    tools: collections::HashMap<tool::ID, ToolState>,
    /// Current position of rings and strips during an interaction, removed when they stop being interacted.
    rings: collections::HashMap<pad::ring::ID, f32>,
    strips: collections::HashMap<pad::strip::ID, f32>,
    /// The position of a virtual knob to show off slider/ring states.
    knob_pos: f32,
    /// Egui's scale factor. octotablet gives us positions in logical window space, but we need
    /// to draw in egui's coordinate space.
    pub egui_scale_factor: f32,
}
impl Default for State {
    fn default() -> Self {
        Self {
            tools: collections::HashMap::new(),
            rings: collections::HashMap::new(),
            strips: collections::HashMap::new(),
            knob_pos: 0.0,
            egui_scale_factor: 1.0,
        }
    }
}
impl<'a> Extend<octotablet::events::Event<'a>> for State {
    fn extend<T: IntoIterator<Item = octotablet::events::Event<'a>>>(&mut self, iter: T) {
        // Drain these events, updating internal state along the way.
        for event in iter {
            match event {
                Event::Tool { tool, event } => match event {
                    ToolEvent::In { tablet } => {
                        // Insert if not found. Then set tablet ID.
                        self.tools
                            .entry(tool.id())
                            .or_insert(ToolState {
                                over: tablet.id(),
                                path: collections::VecDeque::new(),
                                now: Pose::default(),
                                down: false,
                            })
                            .over = tablet.id();
                    }
                    ToolEvent::Down => {
                        let Some(tool) = self.tools.get_mut(&tool.id()) else {
                            continue;
                        };
                        tool.down = true;
                    }
                    ToolEvent::Up => {
                        let Some(tool) = self.tools.get_mut(&tool.id()) else {
                            continue;
                        };
                        tool.down = false;
                    }
                    ToolEvent::Pose(mut pose) => {
                        let Some(tool) = self.tools.get_mut(&tool.id()) else {
                            continue;
                        };
                        // Remap from logical window pixels to egui points.
                        pose.position[0] /= self.egui_scale_factor;
                        pose.position[1] /= self.egui_scale_factor;
                        // Limited size circular buf - pop to make room if full.
                        if tool.path.len() == PATH_LEN {
                            tool.path.pop_front();
                        }
                        tool.path.push_back(Pos2 {
                            x: pose.position[0],
                            y: pose.position[1],
                        });
                        tool.now = pose;
                    }
                    ToolEvent::Removed | ToolEvent::Out => {
                        self.tools.remove(&tool.id());
                    }
                    ToolEvent::Added | ToolEvent::Button { .. } | ToolEvent::Frame(..) => (),
                },
                Event::Pad { pad, event } => match event {
                    PadEvent::Group { event, .. } => match event {
                        PadGroupEvent::Ring { ring, event } => match event {
                            TouchStripEvent::Up => {
                                // End interaction by deleting the state
                                self.rings.remove(&ring.id());
                            }
                            TouchStripEvent::Pose(p) => match self.rings.entry(ring.id()) {
                                collections::hash_map::Entry::Occupied(mut o) => {
                                    // Continued interaction, find the delta and advance the knob
                                    let delta = radial_delta(*o.get(), p);
                                    // Negative converts from delta clockwise as reported from rings
                                    // to delta counterclockwise
                                    self.knob_pos -= delta;
                                    // Ensure it remains in [0, TAU)
                                    self.knob_pos %= std::f32::consts::TAU;
                                    o.insert(p);
                                }
                                collections::hash_map::Entry::Vacant(v) => {
                                    v.insert(p);
                                }
                            },
                            TouchStripEvent::Source(_) | TouchStripEvent::Frame(_) => (),
                        },
                        // Very similar logic to Ring, but the deltas must be interpreted differently.
                        PadGroupEvent::Strip { strip, event } => match event {
                            TouchStripEvent::Up => {
                                // End interaction by deleting the state
                                self.strips.remove(&strip.id());
                            }
                            TouchStripEvent::Pose(p) => match self.strips.entry(strip.id()) {
                                collections::hash_map::Entry::Occupied(mut o) => {
                                    // Continued interaction, find the delta and advance the knob
                                    let delta = p - o.get();
                                    self.knob_pos += delta;
                                    // Ensure it remains in [0, TAU)
                                    self.knob_pos %= std::f32::consts::TAU;
                                    o.insert(p);
                                }
                                collections::hash_map::Entry::Vacant(v) => {
                                    v.insert(p);
                                }
                            },
                            TouchStripEvent::Source(_) | TouchStripEvent::Frame(_) => (),
                        },
                        PadGroupEvent::Mode(_) => (),
                    },
                    PadEvent::Exit | PadEvent::Removed => {
                        // Remove all relevant state
                        for group in &pad.groups {
                            for strip in &group.strips {
                                self.strips.remove(&strip.id());
                            }
                            for ring in &group.rings {
                                self.rings.remove(&ring.id());
                            }
                        }
                    }
                    PadEvent::Added | PadEvent::Enter { .. } | PadEvent::Button { .. } => (),
                },
                Event::Tablet { tablet, event } => match event {
                    TabletEvent::Added => (),
                    // On removal, delete all states referencing it.
                    TabletEvent::Removed => self.tools.retain(|_, state| state.over != tablet.id()),
                },
            }
        }
    }
}
impl State {
    /// Create a visualizer of the current state. The returned widget will consume *all available space* when added!
    // weird that this lifetime can't be elided :o
    pub fn visualize<'a>(&'a self, manager: &'a octotablet::Manager) -> impl egui::Widget + 'a {
        Visualizer {
            state: self,
            manager,
        }
    }
}
struct Visualizer<'a> {
    state: &'a State,
    manager: &'a octotablet::Manager,
}
impl<'a> egui::Widget for Visualizer<'a> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let Self { state, manager } = self;
        let (resp, painter) = ui.allocate_painter(
            ui.available_size(),
            egui::Sense {
                click: false,
                drag: false,
                focusable: false,
            },
        );

        // Visualize as much as we can so we can tell at a glance everything is working.
        // All of the math fiddling and constants are just for aesthetics, little of it means anything lol
        for (tool, state) in &state.tools {
            // Find the tool object for this ID
            let Some(tool) = manager.tools().iter().find(|t| &t.id() == tool) else {
                continue;
            };
            // Find the tablet object for this interaction
            let Some(tablet) = manager.tablets().iter().find(|t| t.id() == state.over) else {
                continue;
            };

            // ======= Interation text ======
            {
                // Show the name
                let mut cursor = resp.rect.left_top();
                let pen_name = match (tool.hardware_id, tool.tool_type) {
                    (None, None) => "Unknown Tool".to_owned(),
                    (Some(id), None) => format!("{:08X?}", id),
                    (None, Some(ty)) => ty.as_ref().to_string(),
                    (Some(id), Some(ty)) => format!("{} {:08X?}", ty.as_ref(), id),
                };
                let rect = painter.text(
                    cursor,
                    Align2::LEFT_TOP,
                    format!(
                        "{pen_name} on {}",
                        tablet.name.as_deref().unwrap_or("Unknown Tablet")
                    ),
                    FontId::default(),
                    Color32::WHITE,
                );
                cursor.y += rect.height();
                // I can't write visualizers for tools I can't test :V
                // Let the user know if there's more available that are invisible.
                let available_axes = tool.axes.available();
                let visualized_axes =
                    AvailableAxes::PRESSURE | AvailableAxes::TILT | AvailableAxes::DISTANCE;
                let seen_axes = available_axes.intersection(visualized_axes);
                let not_seen_axes = available_axes.difference(visualized_axes);
                let axes = if !not_seen_axes.is_empty() {
                    // Pen supports axes not visualized.
                    format!(
                        "Showing axes: {seen_axes:?}. No visualizers for {not_seen_axes:?}, Fixme!"
                    )
                } else {
                    // We show all axes! yay!
                    format!("Showing all axes: {seen_axes:?}")
                };
                // Show the capabilities
                let rect = painter.text(
                    cursor,
                    Align2::LEFT_TOP,
                    axes,
                    FontId::default(),
                    Color32::WHITE,
                );
                cursor.y += rect.height();
                // Show pressed buttons
                /*
                let buttons = if !state.pressed_buttons.is_empty() {
                    format!("Pressing tool buttons {:04X?}", state.pressed_buttons)
                } else {
                    "No pressed tool buttons".to_owned()
                };
                painter.text(
                    cursor,
                    Align2::LEFT_TOP,
                    buttons,
                    FontId::default(),
                    Color32::WHITE,
                );*/
            }
            // ======= Visualize Pose ======
            ui.ctx().set_cursor_icon(egui::CursorIcon::None);
            // Fade out with distance.
            let opacity = state
                .now
                .distance
                .get()
                .map(|v| 1.0 - v.powf(1.5))
                .unwrap_or(1.0);

            // Show an arrow in the direction of tilt.
            let tip_pos = egui::Pos2 {
                x: state.now.position[0],
                y: state.now.position[1],
            };
            if let Some([tiltx, tilty]) = state.now.tilt {
                // These ops actually DO have mathematical meaning unlike the other
                // mad aesthetical tinkering. These tilts describe a 3D vector from tip to back of pen,
                // project this vector down onto the page for visualization.
                let mut tilt_vec = egui::Vec2 {
                    x: tiltx.sin(),
                    y: tilty.sin(),
                };
                // Hardware quirk - some devices report angles which don't make any physical or mathematical
                // sense. (trigonometrically, this length should always be <= 1. This is not the case in practice lol)
                if tilt_vec.length_sq() > 1.0 {
                    tilt_vec = tilt_vec.normalized();
                }
                painter.arrow(
                    tip_pos,
                    400.0 * tilt_vec,
                    Stroke {
                        color: Color32::WHITE
                            .gamma_multiply(tilt_vec.length())
                            .gamma_multiply(opacity),
                        width: tilt_vec.length() * 5.0,
                    },
                )
            }
            // Draw a shape to visualize pressure and distance and such
            painter.add(make_pen_shape(state.now, state.down, opacity));
            // Visualize the path by showing a white dot at every tool frame
            // - helps to show the sampling rate of the digitizer
            if !state.path.is_empty() {
                let mut vec = Vec::with_capacity(state.path.len());
                let color = Color32::WHITE.gamma_multiply(0.1);
                vec.extend(
                    state
                        .path
                        .iter()
                        .map(|&pos| Shape::circle_filled(pos, 2.0, color)),
                );
                painter.add(Shape::Vec(vec));
            }
        }
        if state.tools.is_empty() {
            painter.text(
                resp.rect.center(),
                Align2::CENTER_CENTER,
                "Bring tool near...",
                FontId::new(30.0, Default::default()),
                Color32::WHITE,
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
        }
        // Pad stuffs!
        // ============ Show a scroll wheel based on rings and strips, with absolute touch positions ==============
        {
            const SCROLL_CIRCLE_SIZE: f32 = 50.0;
            const ABSOLUTE_CIRCLE_SIZE: f32 = 10.0;

            let interacted = !state.rings.is_empty() || !state.strips.is_empty();

            let interacted = ui
                .ctx()
                .animate_bool(egui::Id::new("scroll-opacity"), interacted);

            let color = Color32::WHITE.gamma_multiply(interacted * 0.75 + 0.25);
            let stroke = Stroke::new(interacted * 3.0 + 2.0, color);

            // 0 is north, radians CW
            // Convert to 0 is East, radians CW.
            let angled =
                Vec2::angled(-std::f32::consts::FRAC_PI_2 + state.knob_pos) * SCROLL_CIRCLE_SIZE;
            // bottom corner with margin.
            let center = resp.rect.expand(-SCROLL_CIRCLE_SIZE * 1.2).left_bottom();
            // Base circle shape
            painter.circle_stroke(center, SCROLL_CIRCLE_SIZE, stroke);
            // Line showing current scroll position
            painter.line_segment([center + angled, center + angled * 0.5], stroke);
            // Circle showing absolute touch position
            for &pos in state.rings.values() {
                let angled = Vec2::angled(-std::f32::consts::FRAC_PI_2 + pos)
                    * (SCROLL_CIRCLE_SIZE - ABSOLUTE_CIRCLE_SIZE);
                painter.circle_filled(center + angled * 0.9, ABSOLUTE_CIRCLE_SIZE, color);
            }
            // Vertical slider showing absolute strip position. (we don't know if this is a horizontal or
            // vertical strip, just gotta make this assumption)
            for &pos in state.strips.values() {
                // Start to the top right of the scroll circle
                let start_pos = center
                    + Vec2::new(
                        SCROLL_CIRCLE_SIZE * 1.1,
                        -SCROLL_CIRCLE_SIZE + ABSOLUTE_CIRCLE_SIZE,
                    );
                // Move down two units over a swipe.
                let length_vec =
                    Vec2::new(0.0, SCROLL_CIRCLE_SIZE * 2.0 - ABSOLUTE_CIRCLE_SIZE * 2.0);

                painter.line_segment([start_pos, start_pos + length_vec], stroke);
                painter.circle_filled(start_pos + pos * length_vec, ABSOLUTE_CIRCLE_SIZE, color);
            }
        }
        // ================ Show buttons ===================
        /*{
            const BUTTON_CIRCLE_SIZE: f32 = 15.0;
            // Each pad with buttons gets it's own column in a square grid
            for (x, buttons) in summary
                .pads
                .iter()
                .filter_map(|pad| (!pad.buttons.is_empty()).then_some(&pad.buttons))
                .enumerate()
            {
                // Each button goes down in the column
                for (y, button) in buttons.iter().enumerate() {
                    let center = resp.rect.right_top()
                        + Vec2::new(-(x as f32 + 0.5), y as f32 + 0.5) * BUTTON_CIRCLE_SIZE * 2.5;
                    painter.add(if button.currently_pressed {
                        Shape::circle_filled(center, BUTTON_CIRCLE_SIZE, Color32::WHITE)
                    } else {
                        Shape::circle_stroke(
                            center,
                            BUTTON_CIRCLE_SIZE,
                            Stroke::new(2.0, Color32::from_white_alpha(128)),
                        )
                    });
                }
            }
        }*/
        resp
    }
}

/// Draw a circle around the pen visualizing distance, down, and pressure attributes.
fn make_pen_shape(pose: Pose, down: bool, opacity: f32) -> Shape {
    // White filled if pressed, red outline if hovered.
    let (fill, stroke) = if down {
        (
            Color32::WHITE.gamma_multiply(opacity),
            Stroke {
                width: 0.0,
                color: Color32::TRANSPARENT,
            },
        )
    } else {
        (
            Color32::TRANSPARENT,
            Stroke {
                width: 2.0,
                color: Color32::LIGHT_RED.gamma_multiply(opacity),
            },
        )
    };
    let radius = 75.0
        * if down {
            // Touching, show a dot sized by pressure.
            pose.pressure.get().unwrap_or(0.5)
        } else {
            // If not touching, show a large dot on hover
            // that closes in as the pen gets closer
            pose.distance
                .get()
                .map(|v| v.powf(1.5) * 0.5)
                .unwrap_or(0.5)
        };

    let tip_pos = egui::Pos2 {
        x: pose.position[0],
        y: pose.position[1],
    };

    Shape::Circle(eframe::epaint::CircleShape {
        center: tip_pos,
        radius,
        fill,
        stroke,
    })
}
