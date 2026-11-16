//! StatusNotifierItem tray (ksni). Icons are drawn in code, one per state, so no image decoder
//! is needed.

use crate::model::{Aggregate, TrayState};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, Status, ToolTip};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Show,
    SyncNow,
    TogglePause,
    Quit,
}

pub struct GroveTray {
    agg: Aggregate,
    on_action: Arc<dyn Fn(Action) + Send + Sync>,
}

pub type TrayHandle = Handle<GroveTray>;

/// Starts the tray service. Fails when there is no D-Bus session.
pub fn spawn(on_action: Arc<dyn Fn(Action) + Send + Sync>) -> Result<TrayHandle, ksni::Error> {
    GroveTray {
        agg: Aggregate::default(),
        on_action,
    }
    .spawn()
}

pub fn set_state(handle: &TrayHandle, agg: &Aggregate) {
    let agg = agg.clone();
    handle.update(move |t| {
        if t.agg != agg {
            t.agg = agg;
        }
    });
}

impl ksni::Tray for GroveTray {
    fn id(&self) -> String {
        "grove".into()
    }

    fn title(&self) -> String {
        "Grove".into()
    }

    fn status(&self) -> Status {
        match self.agg.state {
            TrayState::Attention => Status::NeedsAttention,
            _ => Status::Active,
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        icons(self.agg.state)
    }

    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        icons(TrayState::Attention)
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: format!("Grove: {}", self.agg.text),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on_action)(Action::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let online = self.agg.state != TrayState::Offline;
        let paused = self.agg.state == TrayState::Paused;
        let item = |label: &str, enabled: bool, a: Action| {
            StandardItem {
                label: label.into(),
                enabled,
                activate: Box::new(move |t: &mut Self| (t.on_action)(a)),
                ..Default::default()
            }
            .into()
        };
        vec![
            StandardItem {
                label: self.agg.text.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            item("Open Grove", true, Action::Show),
            item("Sync now", online && !paused, Action::SyncNow),
            item(
                if paused { "Resume sync" } else { "Pause sync" },
                online,
                Action::TogglePause,
            ),
            MenuItem::Separator,
            item("Quit", true, Action::Quit),
        ]
    }
}

// ---- icon drawing ----------------------------------------------------------------------------

fn colour(s: TrayState) -> [u8; 3] {
    match s {
        TrayState::Ok => [0x2e, 0x9e, 0x5b],
        TrayState::Syncing => [0x2f, 0x7d, 0xd1],
        TrayState::Attention => [0xd9, 0x48, 0x3b],
        TrayState::Paused => [0x8a, 0x8f, 0x98],
        TrayState::Offline => [0x4a, 0x4f, 0x58],
    }
}

fn seg(px: f32, py: f32, a: (f32, f32), b: (f32, f32), half: f32) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((px - a.0) * dx + (py - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    (px - cx).hypot(py - cy) <= half
}

/// Glyph in a 32x32 box, white on the coloured disc.
fn glyph(s: TrayState, x: f32, y: f32) -> bool {
    match s {
        TrayState::Ok => {
            seg(x, y, (9.0, 16.5), (14.0, 21.5), 1.6) || seg(x, y, (14.0, 21.5), (23.0, 11.0), 1.6)
        }
        TrayState::Syncing => {
            // Ring with a gap at the upper right, like a refresh symbol.
            let (dx, dy) = (x - 16.0, y - 16.0);
            let r = dx.hypot(dy);
            let gap = dx > 0.0 && dy < 0.0 && dx.abs() * 0.6 < dy.abs() * 1.6;
            (5.0..=8.0).contains(&r) && !gap
        }
        TrayState::Attention => {
            (14.4..=17.6).contains(&x) && (8.0..=18.0).contains(&y)
                || (x - 16.0).hypot(y - 22.5) <= 2.0
        }
        TrayState::Paused => {
            (10.0..=14.0).contains(&x) && (9.0..=23.0).contains(&y)
                || (18.0..=22.0).contains(&x) && (9.0..=23.0).contains(&y)
        }
        TrayState::Offline => {
            seg(x, y, (10.0, 10.0), (22.0, 22.0), 1.5) || seg(x, y, (22.0, 10.0), (10.0, 22.0), 1.5)
        }
    }
}

/// ARGB32 (network byte order) pixmap of `size` x `size`, 3x3 supersampled.
pub fn render(s: TrayState, size: u32) -> Icon {
    let [r, g, b] = colour(s);
    let scale = 32.0 / size as f32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut disc, mut mark) = (0u32, 0u32);
            for sy in 0..3 {
                for sx in 0..3 {
                    let x = (px as f32 + (sx as f32 + 0.5) / 3.0) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / 3.0) * scale;
                    if (x - 16.0).hypot(y - 16.0) <= 15.0 {
                        disc += 1;
                        if glyph(s, x, y) {
                            mark += 1;
                        }
                    }
                }
            }
            let a = (disc * 255 / 9) as u8;
            // Blend the white glyph over the disc colour by its share of the covered samples.
            let w = (mark * 255).checked_div(disc).unwrap_or(0);
            let mix = |c: u8| ((c as u32 * (255 - w) + 255 * w) / 255) as u8;
            data.extend_from_slice(&[a, mix(r), mix(g), mix(b)]);
        }
    }
    Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

fn icons(s: TrayState) -> Vec<Icon> {
    vec![render(s, 22), render(s, 32), render(s, 64)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [TrayState; 5] = [
        TrayState::Ok,
        TrayState::Syncing,
        TrayState::Attention,
        TrayState::Paused,
        TrayState::Offline,
    ];

    #[test]
    fn every_state_has_a_distinct_non_empty_icon() {
        let imgs: Vec<_> = ALL.iter().map(|s| render(*s, 32)).collect();
        for (i, a) in imgs.iter().enumerate() {
            assert_eq!(a.data.len(), 32 * 32 * 4);
            assert!(a.data.chunks(4).any(|p| p[0] == 255), "opaque pixels");
            assert_eq!(a.data[0], 0, "corner is transparent");
            for b in &imgs[i + 1..] {
                assert_ne!(a.data, b.data);
            }
        }
    }

    #[test]
    fn glyph_is_drawn_in_white() {
        let ok = render(TrayState::Ok, 32);
        let white = ok
            .data
            .chunks(4)
            .filter(|p| p[0] == 255 && p[1] == 255 && p[2] == 255)
            .count();
        assert!(white > 20, "{white}");
    }

    #[test]
    fn pixmap_sizes() {
        let sizes: Vec<_> = icons(TrayState::Ok).iter().map(|i| i.width).collect();
        assert_eq!(sizes, [22, 32, 64]);
    }
}
