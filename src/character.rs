//! Character behavior: wanders the pool, pauses, turns with a little squash,
//! bobs and tilts with the water, and leaves a wake while moving.

use crate::assets::{Facing, SpriteAtlas, WaterMode};
use crate::sim::RippleSim;

/// Tiny xorshift RNG — enough randomness for wander behavior without a dependency.
struct Rng(u64);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f32()
    }
}

enum State {
    Swimming { target_x: f32 },
    Pausing { remaining: f32 },
}

pub struct Character {
    pub x: f32,
    /// -1.0..1.0; sign is facing, magnitude eases through turns for a squash effect.
    pub facing: f32,
    state: State,
    rng: Rng,
    anim_t: f32,
    wake_t: f32,
    splash_t: f32,
    /// Heave/pitch spring state: the hull follows the water like a real
    /// floating body, overshooting and ringing briefly on sharp disturbances.
    bob: f32,
    bob_vel: f32,
    tilt: f32,
    tilt_vel: f32,
}

/// Per-frame output the renderer consumes.
pub struct Pose {
    pub x: f32,
    pub y_offset: f32,
    pub tilt: f32,
    /// Horizontal scale sign/magnitude (squashes through turns).
    pub facing: f32,
    pub frame: usize,
    pub moving: bool,
}

impl Character {
    pub fn new(pool_width: f32, seed: u64) -> Self {
        let mut rng = Rng(seed | 1);
        let x = rng.range(pool_width * 0.2, pool_width * 0.8);
        Self {
            x,
            facing: 1.0,
            state: State::Pausing { remaining: 1.0 },
            rng,
            anim_t: 0.0,
            wake_t: 0.0,
            splash_t: 8.0,
            bob: 0.0,
            bob_vel: 0.0,
            tilt: 0.0,
            tilt_vel: 0.0,
        }
    }

    pub fn update(
        &mut self,
        dt: f32,
        pool_width: f32,
        sprite_w: f32,
        splash_y: f32,
        speed_mul: f32,
        atlas: &SpriteAtlas,
        sim: &mut RippleSim,
    ) -> Pose {
        self.anim_t += dt;
        let margin = sprite_w * 0.7 + 4.0;
        let base_speed = 26.0 * speed_mul;
        let mut moving = false;

        match self.state {
            State::Pausing { ref mut remaining } => {
                *remaining -= dt;
                if *remaining <= 0.0 {
                    let target_x = self.rng.range(margin, (pool_width - margin).max(margin));
                    self.state = State::Swimming { target_x };
                }
            }
            State::Swimming { target_x } => {
                let dist = target_x - self.x;
                let want_facing = if dist >= 0.0 { 1.0 } else { -1.0 };
                // Ease facing toward the travel direction; squashing through
                // zero reads as the character turning around.
                self.facing += (want_facing - self.facing) * (dt * 6.0).min(1.0);

                // Ease in/out near the target so arrivals look deliberate.
                let arrive = (dist.abs() / (sprite_w * 1.5)).clamp(0.25, 1.0);
                let step = base_speed * arrive * dt;
                if dist.abs() <= step.max(0.5) {
                    self.x = target_x;
                    self.state = State::Pausing { remaining: self.rng.range(1.5, 6.0) };
                } else {
                    self.x += step * want_facing;
                    moving = true;
                }
            }
        }

        // A gentle wake at the waterline under the character while moving.
        if moving {
            self.wake_t -= dt;
            if self.wake_t <= 0.0 {
                self.wake_t = 0.55 / speed_mul.max(0.2);
                sim.splash(self.x, splash_y, 5.0, 0.14 * speed_mul.min(1.5));
            }
        }

        // Occasional playful splash while idling (a hop / tail flick).
        self.splash_t -= dt;
        if self.splash_t <= 0.0 {
            self.splash_t = self.rng.range(9.0, 22.0);
            sim.splash(self.x, splash_y, 9.0, -0.7);
        }

        // Buoyancy: sample the water under bow and stern and drive heave and
        // pitch with underdamped springs. A long swell lifts both ends
        // together (a gentle bob), while a sharp ripple passing underneath —
        // the mouse stirring the water close by — lifts one end first and
        // sets the hull rocking, ringing at its natural frequency until the
        // damping settles it. Distance falloff comes free: ripples flatten
        // as they spread, so faraway mouse movement barely registers.
        let half = (sprite_w * 0.35).max(4.0);
        let bow = sim.height_at(self.x + half);
        let stern = sim.height_at(self.x - half);
        // A submerged swimmer barely notices waves overhead: low pitch gain
        // and near-critical damping so it never rings.
        let (omega, zeta, pitch_gain) = match atlas.mode {
            WaterMode::Floater => (13.0, 0.3, 0.7),
            WaterMode::Swimmer => (7.0, 0.9, 0.1),
        };
        let heave = (bow + stern) * 3.0;
        let pitch = (bow - stern) * pitch_gain;
        // Semi-implicit Euler; stable since tick clamps dt ≤ 0.1 (ω·dt < 2).
        let spring =
            |x: f32, v: f32, target: f32| (target - x) * omega * omega - 2.0 * zeta * omega * v;
        self.bob_vel += spring(self.bob, self.bob_vel, heave) * dt;
        self.bob = (self.bob + self.bob_vel * dt).clamp(-9.0, 9.0);
        self.tilt_vel += spring(self.tilt, self.tilt_vel, pitch) * dt;
        self.tilt = (self.tilt + self.tilt_vel * dt).clamp(-0.5, 0.5);
        // Floaters ride visibly up and down; swimmers only drift a little.
        let bob_amp = match atlas.mode {
            WaterMode::Floater => 3.0,
            WaterMode::Swimmer => 1.2,
        };
        let idle_bob = (self.anim_t * 1.4).sin() * bob_amp;

        // Floaters sway side to side like anything bobbing on open water;
        // swimmers stay level (the tail animation carries the motion).
        let swim_wiggle = match atlas.mode {
            WaterMode::Swimmer => 0.0,
            WaterMode::Floater => (self.anim_t * 0.8).sin() * atlas.rock,
        };

        let fps = if moving { atlas.fps * 1.6 } else { atlas.fps };
        let frame = ((self.anim_t * fps) as usize) % atlas.frames.len();

        // Sheets that face left need the flip inverted so "facing right" shows
        // the art mirrored correctly.
        let art_facing = match atlas.facing {
            Facing::Right => self.facing,
            Facing::Left => -self.facing,
        };

        Pose {
            x: self.x,
            y_offset: self.bob + idle_bob,
            tilt: self.tilt + swim_wiggle,
            facing: art_facing.clamp(-1.0, 1.0),
            frame,
            moving,
        }
    }

    /// Keep the character inside the pool when the taskbar geometry changes.
    pub fn clamp_to(&mut self, pool_width: f32) {
        self.x = self.x.clamp(0.0, pool_width);
        if let State::Swimming { ref mut target_x } = self.state {
            *target_x = target_x.clamp(0.0, pool_width);
        }
    }
}
