pub mod color;

use iced::{
    application,
    widget::{button, container, progress_bar, rule, text},
    Border,
};

#[derive(Debug, Copy, Clone, Eq, PartialEq, Default)]
pub enum Theme {
    #[default]
    Dark,
}

impl application::StyleSheet for Theme {
    type Style = ();

    fn appearance(&self, _style: &Self::Style) -> application::Appearance {
        application::Appearance {
            background_color: color::LIGHT_BLACK,
            text_color: color::WHITE,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub enum Text {
    #[default]
    Default,
    Color(iced::Color),
}

impl text::StyleSheet for Theme {
    type Style = Text;

    fn appearance(&self, style: Self::Style) -> text::Appearance {
        match style {
            Text::Default => Default::default(),
            Text::Color(c) => text::Appearance { color: Some(c) },
        }
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub enum Container {
    #[default]
    Frame,
}

impl container::StyleSheet for Theme {
    type Style = Container;

    fn appearance(&self, style: &Self::Style) -> container::Appearance {
        match style {
            Container::Frame => container::Appearance {
                border: Border {
                    color: color::WHITE,
                    width: 2.0,
                    radius: 5.0.into(),
                },
                ..container::Appearance::default()
            },
        }
    }
}

impl button::StyleSheet for Theme {
    type Style = ();

    fn active(&self, _style: &Self::Style) -> button::Appearance {
        button::Appearance {
            background: Some(color::TRANSPARENT.into()),
            text_color: color::GREY_2,
            border: Border {
                color: color::GREY_7,
                width: 1.0,
                radius: 25.0.into(),
            },
            ..button::Appearance::default()
        }
    }

    fn hovered(&self, _style: &Self::Style) -> button::Appearance {
        button::Appearance {
            background: Some(color::GREEN.into()),
            text_color: color::LIGHT_BLACK,
            border: Border {
                color: color::TRANSPARENT,
                width: 0.0,
                radius: 25.0.into(),
            },
            ..button::Appearance::default()
        }
    }
}

impl progress_bar::StyleSheet for Theme {
    type Style = ();

    fn appearance(&self, _style: &Self::Style) -> progress_bar::Appearance {
        progress_bar::Appearance {
            background: color::GREY_6.into(),
            bar: color::GREEN.into(),
            border_radius: 10.0.into(),
        }
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub enum Rule {
    #[default]
    Simple,
    Light,
}

impl rule::StyleSheet for Theme {
    type Style = Rule;

    fn appearance(&self, style: &Self::Style) -> rule::Appearance {
        rule::Appearance {
            color: color::WHITE,
            width: match style {
                Rule::Simple => 2,
                Rule::Light => 1,
            },
            radius: Default::default(),
            fill_mode: rule::FillMode::Full,
        }
    }
}
