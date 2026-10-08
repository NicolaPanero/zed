//! A colored badge with a project's initials next to its name in the
//! sidebar, from this fork (as in Superset), so projects are told apart at a
//! glance.

use gpui::{AnyElement, App, FontWeight, Hsla, IntoElement, ParentElement, Styled, div, hsla, px};
use ui::{ActiveTheme as _, prelude::*};

pub(crate) fn workspace_badge(name: &str, cx: &App) -> AnyElement {
    let foreground = if cx.theme().appearance().is_light() {
        gpui::white()
    } else {
        hsla(0., 0., 0.98, 1.)
    };
    div()
        .flex_none()
        .size(rems(1.125))
        .rounded(px(4.))
        .bg(badge_color(name))
        .flex()
        .items_center()
        .justify_center()
        .child(
            Label::new(initials(name))
                .size(LabelSize::XSmall)
                .weight(FontWeight::BOLD)
                .color(Color::Custom(foreground)),
        )
        .into_any_element()
}

/// "project-btm" → "PB", "superset" → "S".
fn initials(name: &str) -> String {
    let mut words = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    let first = words.next().and_then(|word| word.chars().next());
    let second = words.next().and_then(|word| word.chars().next());
    first
        .into_iter()
        .chain(second)
        .flat_map(char::to_uppercase)
        .collect()
}

/// A stable color per name, so a project keeps its badge color.
fn badge_color(name: &str) -> Hsla {
    let hash = name.bytes().fold(2166136261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16777619)
    });
    let hue = (hash % 360) as f32 / 360.;
    hsla(hue, 0.5, 0.42, 1.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_come_from_the_first_two_words() {
        assert_eq!(initials("project-btm"), "PB");
        assert_eq!(initials("superset"), "S");
        assert_eq!(initials("Zed"), "Z");
        assert_eq!(initials("my app, v2"), "MA");
        assert_eq!(initials("—"), "");
        assert_eq!(badge_color("zed"), badge_color("zed"));
    }
}
