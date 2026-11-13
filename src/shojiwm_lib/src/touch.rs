//! Touchscreens: each finger goes to the surface it came down on, as
//! `wl_touch`, and the config hears every touch after the fact
//! (`COMPOSITOR.event.onTouchAsync`). A config that decides the fingers are
//! making a gesture of its own takes the touch
//! (`COMPOSITOR.input.takeTouch`): the clients are sent `wl_touch.cancel` and
//! nothing more until the last finger has lifted, while the config goes on
//! hearing the touch to its end.
//!
//! A touchscreen is mapped onto the built-in panel, or the first output when
//! there is none. Under the session lock a touch reaches a lock surface or
//! nothing, and the config hears none of it.

use std::collections::HashMap;

use smithay::{
    backend::input::{
        AbsolutePositionEvent, InputBackend, TouchEvent as BackendTouchEvent, TouchSlot,
    },
    input::touch::{DownEvent, MotionEvent, UpEvent},
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{
    ssd::{PointerMovePointSnapshot, TouchEventSnapshot, TouchPhaseSnapshot},
    state::ShojiWM,
};

#[derive(Debug, Default)]
pub struct TouchState {
    /// The fingers that are down, and where.
    points: HashMap<TouchSlot, Point<f64, Logical>>,
    /// The config took this touch: the clients hear no more of it.
    taken: bool,
    /// The touch began under the session lock and stays out of the config's
    /// sight to its end.
    locked: bool,
}

impl TouchState {
    pub fn fingers(&self) -> usize {
        self.points.len()
    }

    pub fn taken(&self) -> bool {
        self.taken
    }
}

/// The connectors of a panel built into the machine.
fn built_in(name: &str) -> bool {
    ["eDP", "LVDS", "DSI"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

impl ShojiWM {
    /// The output a touchscreen lies on.
    fn touch_output(&self) -> Option<Output> {
        self.space
            .outputs()
            .find(|output| built_in(&output.name()))
            .or_else(|| self.space.outputs().next())
            .cloned()
    }

    /// Where a touch event is, global logical.
    fn touch_position<I, E>(&self, event: &E) -> Option<Point<f64, Logical>>
    where
        I: InputBackend,
        E: AbsolutePositionEvent<I>,
    {
        let geometry = self.space.output_geometry(&self.touch_output()?)?;
        Some(event.position_transformed(geometry.size) + geometry.loc.to_f64())
    }

    fn touch_surface_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.session_lock_active {
            self.session_lock_surface_under(position)
        } else {
            self.surface_under(position)
        }
    }

    fn report_touch(
        &mut self,
        phase: TouchPhaseSnapshot,
        slot: TouchSlot,
        position: Option<Point<f64, Logical>>,
        time_msec: u32,
    ) {
        if !self.runtime_touch_async_enabled || self.touch.locked {
            return;
        }
        let output_name = position
            .and_then(|position| self.output_at_point(position))
            .map(|output| output.name());
        let event = TouchEventSnapshot {
            phase,
            id: i32::from(slot),
            position: position.map(|position| PointerMovePointSnapshot {
                x: position.x,
                y: position.y,
            }),
            output_name,
            window_id: position.and_then(|position| self.window_id_under(position)),
            fingers: self.touch.points.len() as u32,
            taken: self.touch.taken,
            timestamp: u64::from(time_msec),
        };
        let now_ms = std::time::Duration::from(self.clock.now()).as_millis() as u64;
        self.config_runtime.touch_async(event, now_ms);
    }

    pub(crate) fn touch_down<I, E>(&mut self, event: &E)
    where
        I: InputBackend,
        E: BackendTouchEvent<I> + AbsolutePositionEvent<I>,
    {
        let Some(position) = self.touch_position(event) else {
            return;
        };
        self.touch_down_at(event.slot(), position, event.time_msec());
    }

    pub fn touch_down_at(
        &mut self,
        slot: TouchSlot,
        position: Point<f64, Logical>,
        time_msec: u32,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        if self.touch.points.is_empty() {
            self.touch.taken = false;
            self.touch.locked = self.session_lock_active;
        }
        self.touch.points.insert(slot, position);
        if !self.touch.taken {
            let serial = SERIAL_COUNTER.next_serial();
            let under = self.touch_surface_under(position);
            // The first finger on a window is a press on it: it takes the
            // keyboard, as it does from the pointer.
            if !self.session_lock_active
                && self.touch.points.len() == 1
                && let Some((surface, _)) = under.as_ref()
                && let Some(window) = self.window_for_pointer_surface(surface)
            {
                self.focus_window_at_surface(&window, Some(surface), serial);
            }
            touch.down(
                self,
                under,
                &DownEvent {
                    slot,
                    location: position,
                    serial,
                    time: time_msec,
                },
            );
        }
        self.report_touch(TouchPhaseSnapshot::Down, slot, Some(position), time_msec);
    }

    pub(crate) fn touch_motion<I, E>(&mut self, event: &E)
    where
        I: InputBackend,
        E: BackendTouchEvent<I> + AbsolutePositionEvent<I>,
    {
        let Some(position) = self.touch_position(event) else {
            return;
        };
        self.touch_motion_at(event.slot(), position, event.time_msec());
    }

    pub fn touch_motion_at(
        &mut self,
        slot: TouchSlot,
        position: Point<f64, Logical>,
        time_msec: u32,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        if self.touch.points.insert(slot, position).is_none() {
            // A finger whose press was never seen (it was down before the
            // session took the seat).
            self.touch.points.remove(&slot);
            return;
        }
        if !self.touch.taken {
            let under = self.touch_surface_under(position);
            touch.motion(
                self,
                under,
                &MotionEvent {
                    slot,
                    location: position,
                    time: time_msec,
                },
            );
        }
        self.report_touch(TouchPhaseSnapshot::Motion, slot, Some(position), time_msec);
    }

    pub fn touch_up(&mut self, slot: TouchSlot, time_msec: u32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let Some(position) = self.touch.points.remove(&slot) else {
            return;
        };
        if !self.touch.taken {
            touch.up(
                self,
                &UpEvent {
                    slot,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: time_msec,
                },
            );
        }
        self.report_touch(TouchPhaseSnapshot::Up, slot, Some(position), time_msec);
        if self.touch.points.is_empty() {
            self.touch.taken = false;
            self.touch.locked = false;
        }
    }

    pub fn touch_frame(&mut self) {
        if self.touch.taken {
            return;
        }
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    /// The device gave the touch up: every finger is gone at once.
    pub fn touch_cancel(&mut self, time_msec: u32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        if !self.touch.taken {
            touch.cancel(self);
        }
        // In the order of their ids, so a config hears the same touch end the
        // same way every time.
        let mut slots: Vec<TouchSlot> = self.touch.points.keys().copied().collect();
        slots.sort_by_key(|slot| i32::from(*slot));
        self.touch.points.clear();
        for slot in slots {
            self.report_touch(TouchPhaseSnapshot::Cancel, slot, None, time_msec);
        }
        self.touch.taken = false;
        self.touch.locked = false;
    }

    /// `COMPOSITOR.input.takeTouch`: the fingers that are down belong to the
    /// config until the last has lifted.
    pub fn take_touch(&mut self) {
        if self.touch.taken || self.touch.points.is_empty() || self.touch.locked {
            return;
        }
        self.touch.taken = true;
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
        let _ = self.display_handle.flush_clients();
    }
}

#[cfg(test)]
mod tests {
    use super::built_in;

    #[test]
    fn a_laptop_panel_is_told_from_an_external_display() {
        assert!(built_in("eDP-1"));
        assert!(built_in("LVDS-1"));
        assert!(built_in("DSI-1"));
        assert!(!built_in("DP-3"));
        assert!(!built_in("HDMI-A-1"));
        assert!(!built_in("HEADLESS-1"));
    }
}
