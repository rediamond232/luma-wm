//! Session locking is enforced by the compositor, independent of shell survival.
use crate::{AnvilState, focus::KeyboardFocusTarget, state::Backend};
use smithay::{
    backend::input::InputTime,
    input::{
        pointer::{CursorImageStatus, MotionEvent},
        tablet::{TabletSeatTrait, tool::ProximityOutEvent},
    },
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};
use std::sync::Mutex;
#[derive(Debug, Default)]
pub struct LockOutput {
    pub locked: bool,
    pub surface: Option<WlSurface>,
    pub generation: u64,
    pub presentations: u8,
}
impl LockOutput {
    fn record_presented(&mut self, generation: u64) -> bool {
        if !self.locked || self.generation != generation {
            return false;
        }
        self.presentations = self.presentations.saturating_add(1);
        self.presentations < 2
    }
    fn ready(&self, generation: u64) -> bool {
        self.locked && self.generation == generation && self.presentations >= 2
    }
}
#[derive(Debug, Default)]
pub struct LockState {
    pub locked: bool,
    pub generation: u64,
    pub confirmation: Option<SessionLocker>,
}
impl<B: Backend + 'static> SessionLockHandler for AnvilState<B> {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }
    fn lock(&mut self, confirmation: SessionLocker) {
        if !B::SUPPORTS_SESSION_LOCK || self.lock.locked || !self.desktop.active {
            return;
        }
        self.lock.locked = true;
        // Stop existing streams before changing input focus or presenting lock
        // content. Unlock requires clients to request new capture sessions.
        self.capture_sessions.clear();
        self.lock.generation = self.lock.generation.wrapping_add(1);
        self.lock.confirmation = Some(confirmation);
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.unset_grab(self);
        keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        self.release_all_keys();
        self.suppressed_keys.clear();
        let pointer = self.pointer.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        pointer.unset_grab(self, serial, time);
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location: pointer.current_location(),
                serial,
                time,
            },
        );
        pointer.frame(self);
        if let Some(touch) = self.seat.get_touch() {
            touch.unset_grab(self);
            touch.cancel(self);
        }
        // Tablet tools retain independent focus and implicit tip/button grabs.
        // End the old proximity sequence so subsequent events cannot use it.
        for tool in self.seat.tablet_seat().get_tools().into_values() {
            tool.unset_grab(self, serial, time);
            if tool.current_tablet().is_some() {
                tool.proximity_out(self, &ProximityOutEvent { serial, time });
                tool.frame(self, time);
            }
        }
        self.dnd_icon = None;
        self.cursor_status = CursorImageStatus::default_named();
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for o in outputs {
            o.user_data()
                .insert_if_missing(|| Mutex::new(LockOutput::default()));
            *o.user_data()
                .get::<Mutex<LockOutput>>()
                .unwrap()
                .lock()
                .unwrap() = LockOutput {
                locked: true,
                generation: self.lock.generation,
                ..Default::default()
            };
            self.backend_data.reset_buffers(&o);
        }
        self.desktop.redraw = true;
    }
    fn unlock(&mut self) {
        self.lock.locked = false;
        self.lock.confirmation = None;
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for o in outputs {
            if let Some(s) = o.user_data().get::<Mutex<LockOutput>>() {
                *s.lock().unwrap() = LockOutput::default();
            }
            self.backend_data.reset_buffers(&o);
        }
        self.desktop.redraw = true;
    }
    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        if let Some(o) = Output::from_resource(&output) {
            if let Some(mode) = o.current_mode() {
                surface.with_pending_state(|s| {
                    s.size = Some(
                        mode.size
                            .to_f64()
                            .to_logical(o.current_scale().fractional_scale())
                            .to_i32_round(),
                    )
                });
                surface.send_configure();
            }
            o.user_data()
                .insert_if_missing(|| Mutex::new(LockOutput::default()));
            let mut s = o
                .user_data()
                .get::<Mutex<LockOutput>>()
                .unwrap()
                .lock()
                .unwrap();
            s.locked = true;
            s.generation = self.lock.generation;
            s.surface = Some(surface.wl_surface().clone());
            drop(s);
            let k = self.seat.get_keyboard().unwrap();
            k.set_focus(
                self,
                Some(KeyboardFocusTarget::Lock(surface.wl_surface().clone())),
                SERIAL_COUNTER.next_serial(),
            );
            self.desktop.redraw = true;
        }
    }
}
impl<B: Backend + 'static> AnvilState<B> {
    pub fn lock_presented(&mut self, output: &Output, generation: u64) {
        if !self.lock.locked || self.lock.generation != generation {
            return;
        }
        if let Some(s) = output.user_data().get::<Mutex<LockOutput>>() {
            let mut s = s.lock().unwrap();
            if s.record_presented(generation) {
                drop(s);
                self.backend_data.reset_buffers(output);
                self.desktop.redraw = true;
            }
        }
        if self.space.outputs().all(|o| {
            o.user_data()
                .get::<Mutex<LockOutput>>()
                .is_some_and(|s| s.lock().unwrap().ready(generation))
        }) {
            if let Some(confirmation) = self.lock.confirmation.take() {
                confirmation.lock();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_confirmation_rejects_old_and_unlocked_frames() {
        let mut output = LockOutput {
            locked: true,
            generation: 2,
            ..Default::default()
        };
        assert!(!output.record_presented(1));
        assert_eq!(output.presentations, 0);
        assert!(output.record_presented(2));
        assert!(!output.ready(2));
        assert!(!output.record_presented(1));
        assert_eq!(output.presentations, 1);
        assert!(!output.record_presented(2));
        assert!(output.ready(2));
        assert!(!output.ready(3));
        output.locked = false;
        assert!(!output.record_presented(2));
        assert!(!output.ready(2));
        let mut hotplug = LockOutput {
            locked: true,
            generation: 2,
            ..Default::default()
        };
        assert!(!hotplug.ready(2));
        assert!(hotplug.record_presented(2));
        assert!(!hotplug.ready(2));
    }
}
