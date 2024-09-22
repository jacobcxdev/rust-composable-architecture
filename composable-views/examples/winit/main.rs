#![allow(dead_code)]
#![allow(unused_variables)]

use composable::Store;
use futures::executor::block_on;
use rfd::{FileDialog, MessageDialog, MessageLevel};
use std::collections::BTreeMap;
use std::sync::Arc;
use window::Action;

use window::menubar::Menu;
use window::{
    ActiveEventLoop, ApplicationHandler, ControlFlow, DeviceEvents, EventLoopError, EventLoopProxy,
    LogicalSize, Size, StartCause, Window, WindowEvent, WindowId,
};

mod gpu;
mod ink;
mod script;
mod settings;
mod window;

struct State {
    stores: BTreeMap<WindowId, (Store<script::State>, Arc<Window>)>,

    proxy: EventLoopProxy,
    menubar: Menu,
}

impl State {
    fn front_window(&self) -> Option<&Window> {
        self.stores
            .values()
            .map(|tuple| &*tuple.1)
            .find(|window| window.has_focus())
    }
}

impl ApplicationHandler<Action> for State {
    fn new_events(&mut self, active: &ActiveEventLoop, cause: StartCause) {
        if cause == StartCause::Init {
            self.proxy.send_event(Action::NewWindow).unwrap()
        }
    }

    fn resumed(&mut self, active: &ActiveEventLoop) {}

    fn user_event(&mut self, active: &ActiveEventLoop, event: Action) {
        match event {
            Action::NewWindow => {
                let window = Arc::new(window::build(active));
                window::menubar::attach(&self.menubar, &window);

                let id = window.id();
                let proxy = self.proxy.clone();
                let wgpu = block_on(gpu::Surface::new(window.clone())); // must be on main thread

                let mut state = script::State::new(wgpu, proxy, id);
                let (width, height) = state.settings.window_size().into();

                // let display = window
                //     .current_monitor()
                //     .or_else(|| window.primary_monitor())
                //     .or_else(|| window.available_monitors().next())
                //     .unwrap()
                //     .size()
                //     .to_logical(window.scale_factor());
                //
                let mut size = LogicalSize::new(width, height);
                // size.height = size.height.min(display.height);
                // size.width = size.width.min(display.width);

                let _ = window.request_inner_size(size);
                window.set_min_inner_size(Some(LogicalSize::new(size.width, 256.0)));

                window.set_visible(true);

                let store = Store::with_initial(state);
                self.stores.insert(id, (store, window.clone()));

                let file = FileDialog::new()
                    .add_filter("Fountain", &["fountain", "spmd"])
                    .add_filter("Inkle", &["ink"])
                    .add_filter("Markdown", &["md"])
                    .set_parent(&window)
                    .pick_file();

                match file {
                    None => self.window_event(active, id, WindowEvent::CloseRequested),
                    Some(path) => {
                        self.stores[&id].0.send(script::Action::Parse(path));
                    }
                }
            }
            Action::ErrorDialog(description, id) => {
                if let Some(store) = self.stores.get(&id) {
                    let _ = MessageDialog::new()
                        .set_level(MessageLevel::Error)
                        .set_title("Could not open file")
                        .set_description(description)
                        .set_parent(&store.1)
                        .show();

                    self.window_event(active, id, WindowEvent::CloseRequested);
                }
            }
            Action::DefaultSize => {
                if let Some(window) = self.front_window() {
                    let size = Size::from(LogicalSize::<f32>::from(window::DEFAULT_SIZE));
                    let _ = window.request_inner_size(size);
                }
            }
        }
    }

    fn window_event(&mut self, active: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => {
                self.stores
                    .entry(id)
                    .and_modify(|store| store.0.send(script::Action::Redraw));
            }
            WindowEvent::Resized(size) => {
                self.stores.entry(id).and_modify(|store| {
                    let (width, height) = size.into();
                    let resize = script::Action::Resize { width, height };

                    store.0.sync(resize);
                });
            }
            WindowEvent::CloseRequested => {
                self.stores.remove(&id);
                if self.stores.is_empty() {
                    active.exit();
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<(), EventLoopError> {
    let (menubar, event_loop) = window::menubar::build()?;

    event_loop.listen_device_events(DeviceEvents::Never);
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();

    let mut state = State {
        stores: BTreeMap::default(),
        menubar,
        proxy,
    };

    event_loop.run_app(&mut state)
}
