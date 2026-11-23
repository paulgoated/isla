/// Critically-damped-ish spring used for the notch size, giving the
/// slightly bouncy "island" feel.
#[derive(Clone, Copy)]
pub struct Spring {
    pub v: f32,
    pub vel: f32,
    pub target: f32,
}

impl Spring {
    pub fn new(v: f32) -> Self {
        Spring { v, vel: 0.0, target: v }
    }

    /// Integrates in small fixed sub-steps so large frame gaps stay stable.
    pub fn step(&mut self, dt: f32, stiffness: f32, damping: f32) {
        let mut left = dt;
        while left > 0.0 {
            let h = left.min(0.004);
            let acc = stiffness * (self.target - self.v) - damping * self.vel;
            self.vel += acc * h;
            self.v += self.vel * h;
            left -= h;
        }
        if self.settled(0.05) {
            self.v = self.target;
            self.vel = 0.0;
        }
    }

    pub fn settled(&self, eps: f32) -> bool {
        (self.target - self.v).abs() < eps && self.vel.abs() < eps * 10.0
    }
}

/// Exponential approach toward a target, for simple fades.
pub fn approach(v: f32, target: f32, rate: f32, dt: f32) -> f32 {
    let k = 1.0 - (-rate * dt).exp();
    let n = v + (target - v) * k;
    if (n - target).abs() < 0.002 {
        target
    } else {
        n
    }
}
