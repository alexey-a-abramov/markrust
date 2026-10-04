// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Passive Split-view projection. This is never an input or undo selection.

use std::ops::Range;

use gpui::{fill, point, px, rgb, size, Bounds, Hsla, PaintQuad, Pixels};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowSelection {
    pub revision: u64,
    pub range: Range<usize>,
    pub reversed: bool,
}

impl ShadowSelection {
    pub fn caret(&self) -> usize {
        if self.reversed {
            self.range.start
        } else {
            self.range.end
        }
    }
}

// Deliberately not the active caret/accent or selection colors. The dashed
// shaft and small cap distinguish the projection even without color vision.
pub fn cursor_color() -> Hsla {
    rgb(0x7186a5).into()
}
pub fn selection_color() -> Hsla {
    cursor_color().opacity(0.16)
}

pub fn cursor_quads(bounds: Bounds<Pixels>) -> Vec<PaintQuad> {
    let color = cursor_color().opacity(0.72);
    let height = f32::from(bounds.size.height).max(1.);
    let mut quads = vec![fill(
        Bounds::new(
            point(bounds.left() - px(2.), bounds.top()),
            size(px(6.), px(2.)),
        ),
        color,
    )];
    let mut y = 3.;
    while y < height {
        quads.push(fill(
            Bounds::new(
                point(bounds.left(), bounds.top() + px(y)),
                size(px(1.), px((height - y).min(4.))),
            ),
            color,
        ));
        y += 6.;
    }
    quads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reversed_projection_keeps_the_active_edge_without_changing_the_range() {
        let shadow = ShadowSelection {
            revision: 3,
            range: 4..9,
            reversed: true,
        };
        assert_eq!(shadow.caret(), 4);
        assert_eq!(shadow.range, 4..9);
    }

    #[test]
    fn passive_cursor_is_capped_and_dashed_inside_the_native_row() {
        let bounds = Bounds::new(point(px(20.), px(30.)), size(px(2.), px(24.)));
        let quads = cursor_quads(bounds);
        assert_eq!(quads[0].bounds.size.width, px(6.));
        assert!(quads.len() > 2);
        assert!(quads
            .iter()
            .all(|quad| quad.bounds.bottom() <= bounds.bottom()));
    }
}
