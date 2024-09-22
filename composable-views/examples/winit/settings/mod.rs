use composable_views::text::Font;
use composable_views::{Size, View};

use crate::settings;
use serde::{de::Deserializer, ser::Serializer, Deserialize, Serialize};

#[allow(non_upper_case_globals)]
const CourierPrimeSans: &[u8] = include_bytes!("Courier Prime Sans.ttf");

#[derive(Serialize, Deserialize)]
pub struct State {
    #[serde(serialize_with = "size_from_font", deserialize_with = "font_from_size")]
    font: Font<'static>,
    #[serde(skip)]
    em: Option<Size>,

    column: Column,
    page_size: PageSize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            font: font_with_size(12.75),
            em: None,

            column: Default::default(),
            page_size: Default::default(),
        }
    }
}

impl State {
    pub fn set_page_size(&mut self, page_size: PageSize) {
        self.page_size = page_size;
    }

    pub fn set_column_width(&mut self, column: Column) {
        self.column = column;
    }
    pub fn set_font_size(&mut self, size: f32) {
        self.font = font_with_size(size);
        self.em = None;
    }

    pub fn em(&mut self) -> Size {
        *self.em.get_or_insert_with(|| {
            let em_space = "\u{2003}";
            self.font.text([0; 4], em_space).size()
        })
    }

    pub fn restore_defaults(&mut self) {
        *self = Self::default()
    }
}

pub fn font_from_size<'de, D>(d: D) -> Result<Font<'static>, D::Error>
where
    D: Deserializer<'de>,
{
    let size = f32::deserialize(d)?;
    let font = font_with_size(size);

    Ok(font)
}

pub fn size_from_font<S>(font: &Font, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_f32(font.size())
}

fn font_with_size(size: f32) -> Font<'static> {
    Font::from(CourierPrimeSans)
        .unwrap()
        .weight(400.0)
        .size(size)
}

#[derive(Serialize, Deserialize, Default)]
pub enum Column {
    #[default]
    Narrow,
    Medium,
    Wide,
}

impl Column {
    pub fn width(&self) -> usize {
        match self {
            Column::Narrow => 63,
            Column::Medium => 90,
            Column::Wide => 105,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub enum PageSize {
    #[default]
    USLetter,
    A4,
}

impl PageSize {
    pub fn left_margin(&self) -> usize {
        15
    }

    pub fn right_margin(&self) -> usize {
        match self {
            PageSize::USLetter => 7,
            PageSize::A4 => 5,
        }
    }

    pub fn height(&self) -> usize {
        match self {
            PageSize::USLetter => 110,
            PageSize::A4 => 117,
        }
    }

    pub fn width_with(&self, center: &Column) -> usize {
        self.left_margin() + self.right_margin() + center.width()
    }
}

impl settings::State {
    pub fn window_size(&mut self) -> Size {
        let scale = self.em();

        let width = self.page_size.width_with(&self.column) as f32 * scale.width;
        let height = self.page_size.height() as f32 * scale.height;

        (width, height).into()
    }
}
