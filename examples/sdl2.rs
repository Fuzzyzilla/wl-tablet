//! Adapted from the `sdl2` `demo.rs` example.
//!
//! Provides simple access to a manager, and does little more.

use std::error::Error;

// SDL2 supports raw-window-handle 0.5.0, but octotablet requires 0.6.0 due to stronger
// lifetime and safety guarantees. Here, we build an unsafe bridge betweem the two, pinky promising that
// we're upholding those missing safety contracts ourselves!

// In the future, this should be cleaned up...
mod rwh_bridge {
    struct RwhBridge<W>(W);
    impl<W: rwh_05::HasRawDisplayHandle> raw_window_handle::HasDisplayHandle for RwhBridge<W> {
        fn display_handle(
            &self,
        ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
            // Convert rwh_05 -> rwh_06
            let raw = match self.0.raw_display_handle() {
                // Wayland...
                rwh_05::RawDisplayHandle::Wayland(rwh_05::WaylandDisplayHandle {
                    display, ..
                }) => raw_window_handle::WaylandDisplayHandle::new(
                    std::ptr::NonNull::new(display).expect("null wayland handle"),
                )
                .into(),
                // Xlib...
                rwh_05::RawDisplayHandle::Xlib(rwh_05::XlibDisplayHandle {
                    display,
                    screen,
                    ..
                }) => raw_window_handle::XlibDisplayHandle::new(
                    std::ptr::NonNull::new(display),
                    screen,
                )
                .into(),
                // Xcb...
                rwh_05::RawDisplayHandle::Xcb(rwh_05::XcbDisplayHandle {
                    connection,
                    screen,
                    ..
                }) => raw_window_handle::XcbDisplayHandle::new(
                    std::ptr::NonNull::new(connection),
                    screen,
                )
                .into(),
                // Windows 32... Has no display handle!
                rwh_05::RawDisplayHandle::Windows(_) => {
                    raw_window_handle::WindowsDisplayHandle::new().into()
                }
                // Octotablet has limited platform support, we don't need to worry about these as they'd fail anyway.
                _ => unimplemented!("unsupported display system"),
            };

            // Safety: guaranteed by precondition, see [rwh_bridge::bridge]
            Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(raw) })
        }
    }
    impl<W: rwh_05::HasRawWindowHandle> raw_window_handle::HasWindowHandle for RwhBridge<W> {
        fn window_handle(
            &self,
        ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
            // Convert rwh_05 -> rwh_06
            let raw = match self.0.raw_window_handle() {
                // Wayland...
                rwh_05::RawWindowHandle::Wayland(rwh_05::WaylandWindowHandle {
                    surface, ..
                }) => raw_window_handle::WaylandWindowHandle::new(
                    std::ptr::NonNull::new(surface).expect("null wayland handle"),
                )
                .into(),
                // Xlib...
                rwh_05::RawWindowHandle::Xlib(rwh_05::XlibWindowHandle {
                    window,
                    visual_id,
                    ..
                }) => {
                    let mut rwh = raw_window_handle::XlibWindowHandle::new(window);
                    rwh.visual_id = visual_id;
                    rwh
                }
                .into(),
                // Xcb...
                rwh_05::RawWindowHandle::Xcb(rwh_05::XcbWindowHandle {
                    window, visual_id, ..
                }) => {
                    let mut rwh = raw_window_handle::XcbWindowHandle::new(
                        std::num::NonZeroU32::new(window).expect("null xcb window"),
                    );
                    rwh.visual_id = std::num::NonZeroU32::new(visual_id);
                    rwh
                }
                .into(),
                // Windows 32...
                rwh_05::RawWindowHandle::Win32(rwh_05::Win32WindowHandle {
                    hinstance,
                    hwnd,
                    ..
                }) => {
                    let mut new = raw_window_handle::Win32WindowHandle::new(
                        (hwnd as isize).try_into().expect("null hwnd"),
                    );
                    new.hinstance = (hinstance as isize).try_into().ok();
                    new.into()
                }
                // Octotablet has limited platform support, we don't need to worry about these as they'd fail anyway.
                _ => unimplemented!("unsupported display system"),
            };

            // Safety: guaranteed by precondition, see [rwh_bridge::bridge]
            Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
        }
    }

    /// Bridge between rwh_05 and rwh_06.
    /// # Safety:
    /// The window and display handles must remain valid for the entire lifetime of the returned opaque object.
    pub unsafe fn bridge<W>(
        window: W,
    ) -> impl raw_window_handle::HasDisplayHandle + raw_window_handle::HasWindowHandle
    where
        W: rwh_05::HasRawDisplayHandle + rwh_05::HasRawWindowHandle,
    {
        RwhBridge(window)
    }
}

pub fn main() -> Result<(), Box<dyn Error>> {
    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;
    // For this simple example, our drawing code is bad, don't double buffer or it'll flicker.
    video.gl_attr().set_double_buffer(false);

    let window = video
        .window("Octotablet", 800, 600)
        .position_centered()
        .opengl()
        .build()
        .map_err(|e| e.to_string())?;

    let mut canvas = window.into_canvas().build()?;

    // Safety: `manager` must not outlive our window.
    // Drop order provides this guarantee! However, this isn't quite strong enough, as SDL could internally thrash it's
    // window handles if it so desires. Hence the unsafe. We trust this isn't the case :P
    let mut manager =
        unsafe { octotablet::Builder::new().build_raw(rwh_bridge::bridge(canvas.window()))? };

    canvas.set_draw_color(sdl2::pixels::Color::BLACK);
    canvas.clear();
    canvas.set_draw_color(sdl2::pixels::Color::WHITE);
    canvas.present();

    let mut event_pump = sdl_context.event_pump()?;
    let mut stylus_down = false;

    'running: loop {
        use sdl2::{event::Event, keyboard::Keycode};
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => break 'running,
                _ => {}
            }
        }
        for event in manager.pump()? {
            use octotablet::events::{Event, ToolEvent};
            // For this simple example, ignore everything else
            let Event::Tool { event, .. } = event else {
                continue;
            };
            match event {
                ToolEvent::Down => stylus_down = true,
                ToolEvent::Up | ToolEvent::Out => stylus_down = false,

                ToolEvent::Pose(p) => {
                    if stylus_down {
                        let size = p.pressure.get().unwrap_or(1.0) * 10.0;
                        let size = size as u32;
                        let rect = sdl2::rect::Rect::from_center(
                            (p.position[0] as i32, p.position[1] as i32),
                            size,
                            size,
                        );
                        canvas.draw_rect(rect)?;
                    }
                }
                _ => (),
            }
        }

        canvas.present();

        ::std::thread::sleep(std::time::Duration::from_millis(16));
    }

    // Be very particular of our drop order.
    // This ensures the user cannot accidentally move out of canvas without also destroying manager.
    drop(manager);
    drop(canvas);

    Ok(())
}
