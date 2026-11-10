//! Renders the windows without a display, with Slint's software renderer.
//!
//! For screenshots (`examples/ui_shots.rs`) and tests. It installs its own Slint platform on
//! the calling thread (Slint's platform is per thread), so don't call it on a thread that runs
//! [`super::run`].

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use slint::{
    ComponentHandle, PhysicalSize, Rgb8Pixel,
    language::ColorScheme,
    platform::{
        Platform, WindowAdapter, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};

use super::components::{Palette, SettingsWindow, SetupWindow};
use super::protocol::{UiState, Window};
use super::window::{Page, UiError};

/// Logical width of both windows (matches the `.slint` markup).
const WIDTH: f32 = 520.0;

thread_local! {
    /// The window adapter created for the most recent component on this thread.
    static LAST_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

struct Offscreen;

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        LAST_WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
        Ok(window)
    }
}

fn install() -> Result<(), UiError> {
    if INSTALLED.get() {
        return Ok(());
    }
    slint::platform::set_platform(Box::new(Offscreen))
        .map_err(|_| UiError::Offscreen("another Slint platform is active on this thread"))?;
    INSTALLED.set(true);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct ShotOptions {
    /// Force the dark colour scheme (the default follows nothing: light).
    pub dark: bool,
    /// Device pixels per logical pixel.
    pub scale: f32,
    /// Setup only: open the model list.
    pub open_model_list: bool,
}

impl Default for ShotOptions {
    fn default() -> Self {
        Self {
            dark: false,
            scale: 1.0,
            open_model_list: false,
        }
    }
}

/// A rendered window: 8-bit RGB, row by row.
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Renders `window` showing `state` at its natural height.
pub fn render(window: Window, state: &UiState, opts: &ShotOptions) -> Result<Shot, UiError> {
    install()?;
    let scheme = if opts.dark {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    match window {
        Window::Setup => {
            let ui = SetupWindow::new()?;
            ui.global::<Palette>().set_color_scheme(scheme);
            ui.apply(None, &SetupWindow::view(state));
            snapshot(&ui, opts, SetupWindow::get_content_height, |ui| {
                if opts.open_model_list {
                    ui.invoke_show_model_list();
                }
            })
        }
        Window::Settings => {
            let ui = SettingsWindow::new()?;
            ui.global::<Palette>().set_color_scheme(scheme);
            ui.apply(None, &SettingsWindow::view(state));
            snapshot(&ui, opts, SettingsWindow::get_content_height, |_| {})
        }
    }
}

fn snapshot<P: Page>(
    ui: &P,
    opts: &ShotOptions,
    content_height: impl Fn(&P) -> f32,
    before_draw: impl FnOnce(&P),
) -> Result<Shot, UiError> {
    let adapter = LAST_WINDOW
        .with(|w| w.borrow_mut().take())
        .ok_or(UiError::Offscreen("no window was created"))?;
    let scale = if opts.scale > 0.0 { opts.scale } else { 1.0 };
    ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    ui.show()?;
    let width = px(WIDTH * scale);
    // Lay out at the real width first; the natural height depends on how text wraps.
    ui.window()
        .set_size(PhysicalSize::new(width, px(2000.0 * scale)));
    let height = px(content_height(ui) * scale);
    ui.window().set_size(PhysicalSize::new(width, height));
    before_draw(ui);
    slint::platform::update_timers_and_animations();

    let (w, h) = (width as usize, height as usize);
    let mut buffer = vec![Rgb8Pixel::new(0, 0, 0); w * h];
    adapter.request_redraw();
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w);
    });
    ui.hide()?;
    Ok(Shot {
        width,
        height,
        rgb: buffer.iter().flat_map(|p| [p.r, p.g, p.b]).collect(),
    })
}

/// Logical length to whole device pixels (at least 1).
fn px(len: f32) -> u32 {
    // Window sizes are far below u32::MAX; the cast saturates anyway.
    len.ceil().max(1.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::samples;

    /// Renders every sample on this test's own thread (own Slint platform).
    #[test]
    fn windows_render_offscreen() {
        let cases = [
            (Window::Setup, samples::setup_fresh(), false),
            (Window::Setup, samples::download_error(), true),
            (Window::Setup, samples::unsupported_desktop(), false),
            (Window::Settings, samples::settings(), false),
            (Window::Settings, samples::settings_idle(), false),
        ];
        for (window, state, dark) in cases {
            let shot = render(
                window,
                &state,
                &ShotOptions {
                    dark,
                    ..Default::default()
                },
            )
            .expect("render");
            assert_eq!(shot.width, 520);
            assert!(
                (300..900).contains(&shot.height),
                "{window:?} height {}",
                shot.height
            );
            assert_eq!(shot.rgb.len(), (shot.width * shot.height * 3) as usize);
            // Something besides the background was drawn.
            let first = &shot.rgb[..3];
            assert!(shot.rgb.chunks(3).any(|p| p != first));
        }
    }

    fn press(ui: &SetupWindow, key: slint::platform::Key) {
        let text = slint::SharedString::from(key);
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }

    /// The setup page's model list works without a mouse.
    #[test]
    fn model_list_works_with_the_keyboard() {
        use slint::platform::Key;
        use std::{cell::RefCell, rc::Rc};

        install().expect("platform");
        let ui = SetupWindow::new().expect("window");
        ui.apply(None, &SetupWindow::view(&samples::setup_fresh()));
        let picked = Rc::new(RefCell::new(Vec::<String>::new()));
        let p = picked.clone();
        ui.on_select_model(move |id| p.borrow_mut().push(id.into()));
        ui.show().expect("show");
        ui.window().set_size(PhysicalSize::new(520, 420));

        // Starts on the model in use (Parakeet), then Down, Down, Up, Return.
        ui.invoke_show_model_list();
        for key in [Key::DownArrow, Key::DownArrow, Key::UpArrow, Key::Return] {
            press(&ui, key);
        }
        assert_eq!(*picked.borrow(), ["moonshine-base-en"]);

        // Escape closes without picking.
        ui.invoke_show_model_list();
        press(&ui, Key::DownArrow);
        press(&ui, Key::Escape);
        assert_eq!(picked.borrow().len(), 1);
        ui.hide().expect("hide");
    }
}
