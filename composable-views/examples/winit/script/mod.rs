use crate::{gpu, settings, window};

use crate::ink::grammer;
use composable::dependencies::with_dependency;
use composable::{Effects, From, Reducer, TryInto};
use composable_views::gpu::Output;
use composable_views::ui::spacer;
use composable_views::View;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

mod margin;

pub struct State {
    wgpu: gpu::Surface<'static>,
    window: window::WindowId,
    proxy: window::EventLoopProxy,

    pub settings: settings::State,
    margin: margin::State,
}

#[derive(Clone, Debug, From, TryInto)]
pub enum Action {
    Parse(PathBuf),
    Margin(margin::Action),

    Resize { width: u32, height: u32 },
    Redraw,
}

impl Reducer for State {
    type Action = Action;
    type Output = Self;

    fn reduce(&mut self, action: Action, send: impl Effects<Action>) {
        use Action::*;
        match action {
            Parse(path) => {
                if let Err(description) = self.parse(path) {
                    self.proxy
                        .send_event(window::Action::ErrorDialog(description, self.window))
                        .unwrap()
                }
            }

            Margin(_) => {}
            Redraw => self.redraw(send),
            Resize { width, height } => {
                self.wgpu.resize(width, height);
                self.redraw(send);
            }
        }
    }
}

impl State {
    pub fn new(
        wgpu: gpu::Surface<'static>,
        proxy: window::EventLoopProxy,
        window: window::WindowId,
    ) -> Self {
        let margin = Default::default();
        let settings = Default::default(); // TODO: new()

        Self {
            wgpu,
            proxy,
            window,
            margin,
            settings,
        }
    }

    pub fn redraw(&mut self, send: impl Effects<Action>) {
        with_dependency(self.wgpu.transform(), || {
            let mut output = Output::new(8.0);
            self.view(send).draw(self.wgpu.bounds(), &mut output);

            let (vertices, indices) = output.into_inner();
            self.wgpu.render(&vertices, &indices).ok();
        })
    }

    pub fn view(&self, send: impl Effects<Action>) -> impl View {
        (spacer::fill(),)
    }
}

impl State {
    fn parse(&mut self, path: PathBuf) -> Result<(), String> {
        let mut script = String::new();

        let mut file = File::open(path).map_err(|err| err.to_string())?;
        file.read_to_string(&mut script)
            .map_err(|err| err.to_string())?;

        use chumsky::Parser;
        let parser = grammer::parser();
        let result = parser.parse(&script).into_output_errors();
        println!("{:#?}", result);

        Ok(())
    }
}
