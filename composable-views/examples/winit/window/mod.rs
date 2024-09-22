pub use winit::application::ApplicationHandler;
pub use winit::dpi::{LogicalSize, Position, Size};
pub use winit::error::EventLoopError;
pub use winit::event::{StartCause, WindowEvent};
pub use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents};
pub use winit::window::{Window, WindowId};

use winit::window::Theme;

pub type EventLoopProxy = winit::event_loop::EventLoopProxy<Action>;

pub mod menubar;

pub const DEFAULT_SIZE: (f32, f32) = (1366.0, 1024.0);

#[derive(Clone, Debug)]
pub enum Action {
    NewWindow,
    DefaultSize,
    ErrorDialog(String, WindowId),
}

pub fn build(active: &ActiveEventLoop) -> Window {
    let attributes = Window::default_attributes()
        .with_title("")
        .with_theme(Some(Theme::Light)) // None → current
        .with_position(Position::Logical(Default::default()))
        .with_visible(false);

    let window = active.create_window(attributes).expect("create_window");

    window
}
