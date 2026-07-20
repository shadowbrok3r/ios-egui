//! Anima flow-match euler-ancestral schedule.
//!
//! Rectified-flow sigma space: `sigma == timestep`, and that value is the raw
//! `timestamp` graph input. `eta = 0` ("euler") is deterministic; `eta = 1`
//! ("euler_ancestral") renoises each step. `scale_model_input` is the identity
//! and the initial latent is unscaled (`init_noise_sigma == 1`).

/// Flow-match time-shift factor.
pub const SHIFT: f32 = 3.0;
/// Timestep scale divisor.
pub const MULTIPLIER: f32 = 1.0;

/// SNR time shift: `a * t / (1 + (a - 1) * t)`.
fn time_snr_shift(a: f32, t: f32) -> f32 {
    if a == 1.0 { t } else { a * t / (1.0 + (a - 1.0) * t) }
}

/// Sigma for a normalized timestep.
fn sigma_of(ts: f32) -> f32 {
    time_snr_shift(SHIFT, ts / MULTIPLIER)
}

/// `steps + 1` sigmas: `sigma_of(linspace(1.0, shift(1/1000), steps))` then 0.
pub fn sigma_schedule(steps: usize) -> Vec<f32> {
    let start = 1.0f32;
    let end = time_snr_shift(SHIFT, 1.0 / 1000.0);
    let mut sigmas = Vec::with_capacity(steps + 1);
    for i in 0..steps {
        let t = if steps <= 1 { start } else { start + (end - start) * i as f32 / (steps - 1) as f32 };
        sigmas.push(sigma_of(t));
    }
    sigmas.push(0.0);
    sigmas
}

/// `eta` for a scheduler name; anything but `"euler"` is ancestral.
pub fn eta_for(name: &str) -> f32 {
    if name.eq_ignore_ascii_case("euler") { 0.0 } else { 1.0 }
}

/// Flow-match euler-ancestral sampler state.
#[derive(Clone, Debug)]
pub struct Scheduler {
    sigmas: Vec<f32>,
    idx: usize,
    eta: f32,
    s_noise: f32,
}

impl Scheduler {
    /// Build the schedule for `steps` steps with the given ancestral `eta`.
    pub fn new(steps: usize, eta: f32) -> Self {
        Self { sigmas: sigma_schedule(steps), idx: 0, eta, s_noise: 1.0 }
    }

    /// Build from a config.json scheduler name (`"euler"` -> eta 0).
    pub fn from_name(name: &str, steps: usize) -> Self {
        Self::new(steps, eta_for(name))
    }

    /// Sigmas, length `steps + 1`, strictly decreasing, first 1.0, last 0.0.
    pub fn sigmas(&self) -> &[f32] {
        &self.sigmas
    }

    /// The `timestamp` graph input per step, length `steps`.
    pub fn timesteps(&self) -> &[f32] {
        &self.sigmas[..self.sigmas.len() - 1]
    }

    /// Number of inference steps.
    pub fn len(&self) -> usize {
        self.sigmas.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Index of the next step to run.
    pub fn index(&self) -> usize {
        self.idx
    }

    /// Ancestral noise factor; 0 means [`Scheduler::step`] ignores `noise`.
    pub fn eta(&self) -> f32 {
        self.eta
    }

    /// Standard deviation of the initial latent.
    pub fn init_noise_sigma(&self) -> f32 {
        1.0
    }

    /// Identity for rectified flow.
    pub fn scale_model_input(&self, sample: &[f32]) -> Vec<f32> {
        sample.to_vec()
    }

    /// Advance one step, returning `(prev_sample, denoised)`. `noise` is read
    /// only when `eta > 0`.
    pub fn step(&mut self, model_output: &[f32], sample: &[f32], noise: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let si = self.sigmas[self.idx];
        let sn = self.sigmas[self.idx + 1];
        let denoised: Vec<f32> = sample.iter().zip(model_output).map(|(&x, &v)| x - v * si).collect();
        let prev = if sn == 0.0 {
            denoised.clone()
        } else {
            let ratio = 1.0 + (sn / si - 1.0) * self.eta;
            let s_down = sn * ratio;
            let (a_n, a_d) = (1.0 - sn, 1.0 - s_down);
            let renoise = (sn * sn - s_down * s_down * a_n * a_n / (a_d * a_d)).max(0.0).sqrt();
            let r = s_down / si;
            let mut p: Vec<f32> = sample.iter().zip(&denoised).map(|(&x, &d)| x * r + d * (1.0 - r)).collect();
            if self.eta > 0.0 {
                let k = a_n / a_d;
                let amp = self.s_noise * renoise;
                for (i, v) in p.iter_mut().enumerate() {
                    *v = *v * k + noise.get(i).copied().unwrap_or(0.0) * amp;
                }
            }
            p
        };
        self.idx += 1;
        (prev, denoised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_snr_shift_identity_at_one() {
        assert_eq!(time_snr_shift(1.0, 0.37), 0.37);
    }

    #[test]
    fn schedule_endpoints_and_length() {
        let s = sigma_schedule(10);
        assert_eq!(s.len(), 11);
        assert_eq!(s[0], 1.0);
        assert_eq!(*s.last().unwrap(), 0.0);
    }

    #[test]
    fn schedule_is_strictly_decreasing() {
        for steps in [1usize, 2, 4, 10, 20, 50] {
            let s = sigma_schedule(steps);
            assert_eq!(s.len(), steps + 1);
            assert!(s.windows(2).all(|w| w[0] > w[1]), "steps={steps} sigmas={s:?}");
        }
    }

    #[test]
    fn schedule_last_nonzero_sigma_matches_shifted_end() {
        let s = sigma_schedule(10);
        // sigma_of(time_snr_shift(3, 1/1000)) = 3*0.002994012/(1+2*0.002994012).
        assert!((s[9] - 0.00892857).abs() < 1e-6, "got {}", s[9]);
    }

    #[test]
    fn single_step_schedule_is_one_then_zero() {
        assert_eq!(sigma_schedule(1), vec![1.0, 0.0]);
    }

    #[test]
    fn timesteps_alias_the_sigmas() {
        let s = Scheduler::new(8, 0.0);
        assert_eq!(s.timesteps(), &s.sigmas()[..8]);
        assert_eq!(s.len(), 8);
        assert_eq!(s.init_noise_sigma(), 1.0);
    }

    #[test]
    fn eta_from_scheduler_name() {
        assert_eq!(eta_for("euler"), 0.0);
        assert_eq!(eta_for("Euler"), 0.0);
        assert_eq!(eta_for("euler_ancestral"), 1.0);
    }

    #[test]
    fn scale_model_input_is_identity() {
        let s = Scheduler::new(4, 0.0);
        assert_eq!(s.scale_model_input(&[1.5, -2.0]), vec![1.5, -2.0]);
    }

    #[test]
    fn deterministic_step_is_lerp_of_sample_and_denoised() {
        let mut s = Scheduler::new(4, 0.0);
        let (si, sn) = (s.sigmas()[0], s.sigmas()[1]);
        let sample = [3.0f32, -1.0];
        let out = [0.5f32, 2.0];
        let (prev, denoised) = s.step(&out, &sample, &[]);
        let r = sn / si;
        for i in 0..2 {
            assert!((denoised[i] - (sample[i] - out[i] * si)).abs() < 1e-6);
            assert!((prev[i] - (r * sample[i] + (1.0 - r) * denoised[i])).abs() < 1e-5, "i={i}");
        }
        assert_eq!(s.index(), 1);
    }

    #[test]
    fn last_step_returns_denoised() {
        let mut s = Scheduler::new(2, 0.0);
        s.step(&[0.0, 0.0], &[1.0, 1.0], &[]);
        let si = s.sigmas()[1];
        let (prev, denoised) = s.step(&[1.0, -1.0], &[4.0, 4.0], &[]);
        assert_eq!(prev, denoised);
        assert!((prev[0] - (4.0 - si)).abs() < 1e-6);
        assert!((prev[1] - (4.0 + si)).abs() < 1e-6);
    }

    #[test]
    fn ancestral_step_uses_noise() {
        let mut a = Scheduler::new(4, 1.0);
        let mut b = Scheduler::new(4, 1.0);
        let sample = [3.0f32, -1.0];
        let out = [0.5f32, 2.0];
        let (p0, _) = a.step(&out, &sample, &[0.0, 0.0]);
        let (p1, _) = b.step(&out, &sample, &[1.0, 1.0]);
        assert_ne!(p0, p1);
    }

    #[test]
    fn ancestral_step_with_zero_noise_scales_by_alpha_ratio() {
        let mut a = Scheduler::new(4, 1.0);
        let (si, sn) = (a.sigmas()[0], a.sigmas()[1]);
        let sample = [3.0f32];
        let out = [0.5f32];
        let (prev, denoised) = a.step(&out, &sample, &[0.0]);
        // eta = 1 -> s_down = sn * sn / si.
        let s_down = sn * (1.0 + (sn / si - 1.0));
        let r = s_down / si;
        let expect = (r * sample[0] + (1.0 - r) * denoised[0]) * ((1.0 - sn) / (1.0 - s_down));
        assert!((prev[0] - expect).abs() < 1e-5, "got {} want {expect}", prev[0]);
    }
}
