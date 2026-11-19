const EXPAND: f64 = 0.420;

fn unit(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

pub fn next_frame(now: f64, hz: f64) -> f64 {
    let hz = if hz.is_finite() && hz >= 1.0 {
        hz
    } else {
        60.0
    };
    ((now * hz).floor() + 1.0) / hz
}

pub fn bezier(x: f64, a: f64, b: f64, c: f64, d: f64) -> f64 {
    let x = unit(x);
    if x == 0.0 || x == 1.0 {
        return x;
    }
    let curve = |t: f64, p: f64, q: f64| {
        let s = 1.0 - t;
        3.0 * s * s * t * p + 3.0 * s * t * t * q + t * t * t
    };
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..17 {
        let mid = (lo + hi) * 0.5;
        if curve(mid, a, c) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    curve((lo + hi) * 0.5, b, d)
}

pub fn out(t: f64) -> f64 {
    bezier(t, 0.2, 0.8, 0.3, 1.0)
}
fn smooth(t: f64) -> f64 {
    bezier(t, 0.42, 0.0, 0.58, 1.0)
}

#[derive(Clone, Copy)]
struct Spin {
    start: f64,
    angle: f64,
    from: f64,
    to: f64,
    span: f64,
}

impl Spin {
    fn speed(&self, t: f64) -> f64 {
        let p = unit((t - self.start) / self.span);
        let v = if self.to > self.from {
            bezier(p, 0.42, 0.0, 1.0, 1.0)
        } else {
            bezier(p, 0.0, 0.0, 0.58, 1.0)
        };
        self.from + (self.to - self.from) * v
    }
    fn angle_at(&self, t: f64) -> f64 {
        let dt = (t - self.start).max(0.0);
        let ramp = dt.min(self.span);
        let avg = (0..8)
            .map(|i| self.speed(self.start + ramp * (i as f64 + 0.5) / 8.0))
            .sum::<f64>()
            / 8.0;
        (self.angle + 22.5 * (ramp * avg + (dt - ramp) * self.to)).rem_euclid(360.0)
    }
    fn set(&mut self, playing: bool, t: f64) {
        let target = if playing { 1.0 } else { 0.0 };
        if target == self.to {
            return;
        }
        *self = Self {
            start: t,
            angle: self.angle_at(t),
            from: self.speed(t),
            to: target,
            span: if playing { 0.300 } else { 0.400 },
        };
    }
}

pub struct Anim {
    pub expanded: bool,
    pub closing: Option<f64>,
    pub hovered: bool,
    pub reduced: bool,
    hover_at: Option<f64>,
    pub collapse_at: Option<f64>,
    width_at: f64,
    width_from: f64,
    content_from: f64,
    ripple_at: f64,
    spin: Spin,
}

pub struct Pose {
    pub width: f32,
    pub opacity: f32,
    pub dx: f32,
    pub dy: f32,
    pub scale: (f32, f32),
    pub angle: f32,
    pub ripple: f32,
    pub splash: f32,
    pub content: f32,
    pub tip: f32,
    pub breath: f32,
}

impl Anim {
    pub fn new(reduced: bool) -> Self {
        Self {
            expanded: false,
            closing: None,
            hovered: false,
            reduced,
            hover_at: None,
            collapse_at: None,
            width_at: -2.0,
            width_from: 44.0,
            content_from: 0.0,
            ripple_at: 0.120,
            spin: Spin {
                start: -1.0,
                angle: 0.0,
                from: 0.0,
                to: 0.0,
                span: 0.4,
            },
        }
    }
    pub fn interactive(&self, t: f64) -> bool {
        self.closing.is_none() && t >= if self.reduced { 0.2 } else { 0.65 }
    }
    pub fn hover(&mut self, inside: bool, t: f64) {
        if self.hovered == inside {
            return;
        }
        self.hovered = inside;
        self.hover_at = (inside && !self.expanded).then_some(t);
        if self.expanded {
            self.collapse_at = (!inside).then_some(t + 4.0);
        }
    }
    pub fn set_expanded(&mut self, expanded: bool, t: f64) {
        if self.closing.is_some() {
            return;
        }
        if self.expanded != expanded {
            self.width_from = self.width(t);
            self.content_from = self.content(t);
            self.width_at = t;
            self.expanded = expanded;
        }
        self.hover_at = None;
        self.collapse_at = (expanded && !self.hovered).then_some(t + 4.0);
    }
    pub fn reveal(&mut self, t: f64) {
        if self.closing.is_none() {
            self.ripple_at = t;
            self.set_expanded(true, t);
        }
    }
    pub fn close(&mut self, t: f64) {
        if self.closing.is_none() {
            self.set_expanded(false, t);
            self.closing = Some(t);
        }
    }
    pub fn finished(&self, t: f64) -> bool {
        self.closing
            .is_some_and(|s| t - s >= if self.reduced { 0.2 } else { 1.3 })
    }
    pub fn advance(&mut self, t: f64) {
        if self.collapse_at.is_some_and(|end| t >= end) {
            self.set_expanded(false, t);
        }
    }
    pub fn playback(&mut self, playing: bool, t: f64) {
        self.spin.set(playing, t)
    }
    fn width(&self, t: f64) -> f64 {
        let target = if self.expanded { 216.0 } else { 44.0 };
        let p = if self.reduced {
            1.0
        } else {
            bezier((t - self.width_at) / EXPAND, 0.2, 0.9, 0.25, 1.12)
        };
        self.width_from + (target - self.width_from) * p
    }
    fn content(&self, t: f64) -> f64 {
        if self.reduced {
            return if self.expanded { 1.0 } else { 0.0 };
        }
        if self.expanded {
            self.content_from + (1.0 - self.content_from) * out((t - self.width_at - 0.16) / 0.2)
        } else {
            self.content_from * (1.0 - out((t - self.width_at) / 0.12))
        }
    }
    pub fn pose(&self, t: f64) -> Pose {
        let mut p = Pose {
            width: self.width(t) as f32,
            opacity: 1.0,
            dx: 0.0,
            dy: 0.0,
            scale: (1.0, 1.0),
            angle: if self.reduced {
                0.0
            } else {
                self.spin.angle_at(t) as f32
            },
            ripple: -1.0,
            splash: -1.0,
            content: self.content(t) as f32,
            tip: 0.0,
            breath: if self.reduced {
                0.85
            } else {
                (0.5 + 0.5 * smooth(1.0 - (t.rem_euclid(2.4) / 1.2 - 1.0).abs())) as f32
            },
        };
        if self.reduced {
            p.opacity = unit(t / 0.2) as f32;
        } else if t < 0.650 {
            let n = t / 0.650;
            let (lo, hi, frac) = if n < 0.55 {
                ((-42.0, 0.3, 0.0), (3.0, 1.07, 1.0), n / 0.55)
            } else {
                ((3.0, 1.07, 1.0), (0.0, 1.0, 1.0), (n - 0.55) / 0.45)
            };
            let f = bezier(frac, 0.2, 0.9, 0.3, 1.2);
            p.dx = (lo.0 + (hi.0 - lo.0) * f) as f32;
            let s = (lo.1 + (hi.1 - lo.1) * f) as f32;
            p.scale = (s, s);
            p.opacity = unit(lo.2 + (hi.2 - lo.2) * f) as f32;
        }
        let ripple = (t - self.ripple_at) / 0.760;
        if !self.reduced && (0.0..=1.0).contains(&ripple) {
            p.ripple = out(ripple) as f32;
        }
        if let Some(s) = self.hover_at {
            let d = t - s - 0.5;
            if d >= 0.0 {
                p.tip = if self.reduced {
                    1.0
                } else {
                    out(d / 0.18) as f32
                };
            }
        }
        if let Some(s) = self.closing {
            let dt = t - s;
            p.tip = 0.0;
            p.ripple = -1.0;
            if self.reduced {
                p.opacity = (1.0 - unit(dt / 0.2)) as f32;
            } else if dt >= 0.260 {
                let n = unit((dt - 0.260) / 0.560);
                let (a, b, f) = if n < 0.35 {
                    ((0.0, 1.0, 1.0, 1.0), (2.0, 0.92, 1.1, 1.0), n / 0.35)
                } else {
                    (
                        (2.0, 0.92, 1.1, 1.0),
                        (30.0, 0.18, 0.3, 0.0),
                        (n - 0.35) / 0.65,
                    )
                };
                let f = bezier(f, 0.55, 0.0, 0.75, 0.3);
                p.dy = (a.0 + (b.0 - a.0) * f) as f32;
                p.scale = (
                    (a.1 + (b.1 - a.1) * f) as f32,
                    (a.2 + (b.2 - a.2) * f) as f32,
                );
                p.opacity = unit(a.3 + (b.3 - a.3) * f) as f32;
                if (0.560..1.260).contains(&dt) {
                    p.splash = out((dt - 0.560) / 0.7) as f32;
                }
            }
        }
        p
    }
    pub fn animating(&self, t: f64, has_media: bool) -> bool {
        let dt = t - self.width_at;
        let tip_wait = self.hover_at.map(|s| s + 0.5 - t);
        self.closing.is_some()
            || t < if self.reduced { 0.2 } else { 0.880 }
            || (!self.reduced
                && ((0.0..EXPAND).contains(&dt)
                    || (0.0..0.760).contains(&(t - self.ripple_at))
                    || tip_wait.is_some_and(|v| (-0.180..=0.0).contains(&v))
                    || t < self.spin.start + self.spin.span
                    || !has_media
                    || self.spin.to > 0.0))
    }

    pub fn hint_deadline(&self, t: f64) -> Option<f64> {
        self.hover_at.map(|at| at + 0.5).filter(|at| *at > t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn easing_preserves_endpoints_and_overshoot() {
        assert_eq!(bezier(0.0, 0.2, 0.9, 0.25, 1.12), 0.0);
        assert_eq!(bezier(1.0, 0.2, 0.9, 0.25, 1.12), 1.0);
        assert!(bezier(0.90, 0.2, 0.9, 0.25, 1.12) > 1.0);
    }
    #[test]
    fn hovering_delays_collapse_and_closing_ignores_reveal() {
        let mut a = Anim::new(false);
        a.set_expanded(true, 1.0);
        a.hover(true, 2.0);
        a.advance(10.0);
        assert!(a.expanded);
        a.hover(false, 10.0);
        a.advance(13.0);
        assert!(a.expanded);
        a.advance(14.1);
        assert!(!a.expanded);
        a.close(15.0);
        a.reveal(15.2);
        assert!(!a.expanded && !a.finished(16.0) && a.finished(16.31));
    }
    #[test]
    fn paused_disc_stops_at_its_current_angle_and_idles() {
        let mut a = Anim::new(false);
        a.playback(true, 2.0);
        let angle = a.pose(5.0).angle;
        a.playback(false, 5.0);
        assert_eq!(a.pose(5.0).angle, angle);
        assert_eq!(a.pose(6.0).angle, a.pose(10.0).angle);
        assert!(!a.animating(10.0, true));
        a.playback(true, 10.0);
        assert_eq!(a.pose(10.0).angle, a.pose(10.001).angle);
    }
    #[test]
    fn display_deadlines_keep_phase_after_a_late_frame() {
        for hz in [59.94, 60.0, 144.0, 240.0] {
            for frame in [1.0, 50.0, 10_000.0] {
                let now = (frame + 0.8) / hz;
                assert!((next_frame(now, hz) - (frame + 1.0) / hz).abs() < 1e-10);
                assert!((next_frame(now + 2.3 / hz, hz) - (frame + 4.0) / hz).abs() < 1e-10);
            }
        }
    }
}
