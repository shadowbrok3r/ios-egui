//! SD1.5 noise schedules and samplers over `ndarray`.
//!
//! Clean-room reimplementation of the diffusers / k-diffusion (Crowson) math for
//! `scaled_linear` betas: `betas = linspace(sqrt(bs), sqrt(be), 1000)^2`,
//! `sigma = sqrt((1 - cumprod(1-beta)) / cumprod(1-beta))`. Euler-ancestral uses
//! `linspace` timestep spacing; DPM++ 2M uses the Karras sigma schedule. Both run
//! in k-diffusion sigma space with `scale_model_input = x / sqrt(sigma^2 + 1)`.

use ndarray::Array1;

const NUM_TRAIN_TIMESTEPS: usize = 1000;
const BETA_START: f64 = 0.00085;
const BETA_END: f64 = 0.012;
const KARRAS_RHO: f64 = 7.0;

/// Which sampler drives [`Scheduler::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sampler {
    /// Euler ancestral, `linspace` timestep spacing (stochastic).
    EulerAncestral,
    /// DPM++ 2M with the Karras sigma schedule (deterministic).
    DpmPP2mKarras,
}

/// The full 1000-entry training sigma table, increasing with the timestep index.
fn training_sigmas() -> Array1<f64> {
    let betas = Array1::linspace(BETA_START.sqrt(), BETA_END.sqrt(), NUM_TRAIN_TIMESTEPS).mapv(|b| b * b);
    let mut cumprod = Vec::with_capacity(NUM_TRAIN_TIMESTEPS);
    let mut running = 1.0f64;
    for &b in betas.iter() {
        running *= 1.0 - b;
        cumprod.push(running);
    }
    Array1::from_vec(cumprod).mapv(|a| ((1.0 - a) / a).sqrt())
}

/// Linear interpolation of `table` at fractional index `x` in `[0, len-1]`.
fn interp(x: f64, table: &Array1<f64>) -> f64 {
    let n = table.len();
    if x <= 0.0 {
        return table[0];
    }
    if x >= (n - 1) as f64 {
        return table[n - 1];
    }
    let lo = x.floor() as usize;
    let frac = x - lo as f64;
    table[lo] * (1.0 - frac) + table[lo + 1] * frac
}

/// Continuous timestep for a sigma, log-linear interpolation over `log_sigmas`
/// (diffusers `_sigma_to_t`). `log_sigmas` is increasing.
fn sigma_to_t(sigma: f64, log_sigmas: &Array1<f64>) -> f64 {
    let log_sigma = sigma.max(1e-10).ln();
    let n = log_sigmas.len();
    let mut low = 0usize;
    for i in 0..n {
        if log_sigmas[i] <= log_sigma {
            low = i;
        } else {
            break;
        }
    }
    let low = low.min(n - 2);
    let high = low + 1;
    let w = ((log_sigmas[low] - log_sigma) / (log_sigmas[low] - log_sigmas[high])).clamp(0.0, 1.0);
    ((1.0 - w) * low as f64 + w * high as f64).clamp(0.0, (n - 1) as f64)
}

/// `linspace(0, 999, steps)` reversed — descending model timesteps.
fn linspace_timesteps(steps: usize) -> Vec<f64> {
    let last = (NUM_TRAIN_TIMESTEPS - 1) as f64;
    (0..steps)
        .map(|k| if steps == 1 { 0.0 } else { last * (steps - 1 - k) as f64 / (steps - 1) as f64 })
        .collect()
}

/// Ancestral noise split for a step (diffusers `EulerAncestralDiscreteScheduler`).
fn ancestral_sigmas(sigma_from: f32, sigma_to: f32) -> (f32, f32) {
    let (f, t) = (sigma_from as f64, sigma_to as f64);
    let up = (t * t * (f * f - t * t) / (f * f)).max(0.0).sqrt();
    let down = (t * t - up * up).max(0.0).sqrt();
    (up as f32, down as f32)
}

/// SD1.5 sampler state: precomputed sigmas + model timesteps for `steps` steps.
pub struct Scheduler {
    sampler: Sampler,
    sigmas: Vec<f32>,
    timesteps: Vec<f32>,
    prev_denoised: Option<Vec<f32>>,
}

impl Scheduler {
    /// Build the schedule for `steps` inference steps.
    pub fn new(sampler: Sampler, steps: usize) -> Self {
        let full = training_sigmas();
        let (sigmas, timesteps) = match sampler {
            Sampler::EulerAncestral => {
                let ts = linspace_timesteps(steps);
                let mut sigmas: Vec<f32> = ts.iter().map(|&t| interp(t, &full) as f32).collect();
                sigmas.push(0.0);
                (sigmas, ts.iter().map(|&t| t as f32).collect())
            }
            Sampler::DpmPP2mKarras => {
                let sigma_min = full[0];
                let sigma_max = full[NUM_TRAIN_TIMESTEPS - 1];
                let (min_inv, max_inv) = (sigma_min.powf(1.0 / KARRAS_RHO), sigma_max.powf(1.0 / KARRAS_RHO));
                let log_sigmas = full.mapv(|s| s.ln());
                let mut sigmas = Vec::with_capacity(steps + 1);
                let mut timesteps = Vec::with_capacity(steps);
                for k in 0..steps {
                    let ramp = if steps == 1 { 0.0 } else { k as f64 / (steps - 1) as f64 };
                    let sigma = (max_inv + ramp * (min_inv - max_inv)).powf(KARRAS_RHO);
                    timesteps.push(sigma_to_t(sigma, &log_sigmas) as f32);
                    sigmas.push(sigma as f32);
                }
                sigmas.push(0.0);
                (sigmas, timesteps)
            }
        };
        Self { sampler, sigmas, timesteps, prev_denoised: None }
    }

    /// Sigmas, length `steps + 1`, descending, last is 0.
    pub fn sigmas(&self) -> &[f32] {
        &self.sigmas
    }

    /// Model timesteps (UNet `timestamp` input), length `steps`.
    pub fn timesteps(&self) -> &[f32] {
        &self.timesteps
    }

    /// Number of inference steps.
    pub fn len(&self) -> usize {
        self.timesteps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timesteps.is_empty()
    }

    /// Standard deviation of the initial latent (`randn * init_noise_sigma`).
    pub fn init_noise_sigma(&self) -> f32 {
        self.sigmas[0]
    }

    /// Scale a latent for the model input: `x / sqrt(sigma^2 + 1)`.
    pub fn scale_model_input(&self, sample: &[f32], step: usize) -> Vec<f32> {
        let s = self.sigmas[step];
        let inv = 1.0 / (s * s + 1.0).sqrt();
        sample.iter().map(|&x| x * inv).collect()
    }

    /// Advance `sample` from `step` to `step + 1` given the model's epsilon output.
    /// `noise` is used only by the ancestral sampler.
    pub fn step(&mut self, model_output: &[f32], step: usize, sample: &mut [f32], noise: &[f32]) {
        let sigma = self.sigmas[step];
        let sigma_next = self.sigmas[step + 1];
        match self.sampler {
            Sampler::EulerAncestral => euler_ancestral_step(sigma, sigma_next, model_output, sample, noise),
            Sampler::DpmPP2mKarras => {
                let sigma_prev = if step > 0 { Some(self.sigmas[step - 1]) } else { None };
                let denoised = dpmpp_2m_step(sigma, sigma_next, sigma_prev, model_output, sample, self.prev_denoised.as_deref());
                self.prev_denoised = Some(denoised);
            }
        }
    }
}

/// In-place Euler-ancestral update.
fn euler_ancestral_step(sigma: f32, sigma_next: f32, eps: &[f32], sample: &mut [f32], noise: &[f32]) {
    let (sigma_up, sigma_down) = ancestral_sigmas(sigma, sigma_next);
    let dt = sigma_down - sigma;
    for i in 0..sample.len() {
        let noise_i = noise.get(i).copied().unwrap_or(0.0);
        sample[i] = sample[i] + eps[i] * dt + noise_i * sigma_up;
    }
}

/// In-place DPM++ 2M update; returns this step's `denoised` (x0) for the next step.
fn dpmpp_2m_step(
    sigma: f32,
    sigma_next: f32,
    sigma_prev: Option<f32>,
    eps: &[f32],
    sample: &mut [f32],
    prev_denoised: Option<&[f32]>,
) -> Vec<f32> {
    let denoised: Vec<f32> = sample.iter().zip(eps).map(|(&x, &e)| x - sigma * e).collect();
    if sigma_next == 0.0 {
        sample.copy_from_slice(&denoised);
        return denoised;
    }
    let (s, sn) = (sigma as f64, sigma_next as f64);
    let t = -s.ln();
    let t_next = -sn.ln();
    let h = t_next - t;
    let ratio = (sn / s) as f32;
    let coeff = (1.0 - (-h).exp()) as f32;
    match (prev_denoised, sigma_prev) {
        (Some(prev), Some(sp)) if sp > 0.0 => {
            let t_prev = -(sp as f64).ln();
            let h_last = t - t_prev;
            let r = h_last / h;
            let a = (1.0 + 1.0 / (2.0 * r)) as f32;
            let b = (1.0 / (2.0 * r)) as f32;
            for i in 0..sample.len() {
                let d = a * denoised[i] - b * prev[i];
                sample[i] = ratio * sample[i] + coeff * d;
            }
        }
        _ => {
            for i in 0..sample.len() {
                sample[i] = ratio * sample[i] + coeff * denoised[i];
            }
        }
    }
    denoised
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGMA_MAX: f32 = 14.6146;
    const SIGMA_MIN: f32 = 0.0292;

    #[test]
    fn training_sigma_endpoints_match_sd15() {
        let full = training_sigmas();
        assert!((full[999] as f32 - SIGMA_MAX).abs() < 0.05, "sigma_max={}", full[999]);
        assert!((full[0] as f32 - SIGMA_MIN).abs() < 0.002, "sigma_min={}", full[0]);
    }

    #[test]
    fn sigma_to_t_inverts_the_table() {
        let full = training_sigmas();
        let logs = full.mapv(|s| s.ln());
        for &idx in &[0usize, 250, 500, 750, 999] {
            let t = sigma_to_t(full[idx], &logs);
            assert!((t - idx as f64).abs() < 0.5, "idx={idx} t={t}");
        }
    }

    #[test]
    fn euler_schedule_shape_and_bounds() {
        let s = Scheduler::new(Sampler::EulerAncestral, 20);
        assert_eq!(s.sigmas().len(), 21);
        assert_eq!(s.timesteps().len(), 20);
        assert_eq!(*s.sigmas().last().unwrap(), 0.0);
        assert!((s.init_noise_sigma() - SIGMA_MAX).abs() < 0.05);
        assert!(s.sigmas().windows(2).all(|w| w[0] >= w[1]));
        assert!((s.timesteps()[0] - 999.0).abs() < 1e-3);
    }

    #[test]
    fn karras_schedule_shape_and_bounds() {
        let s = Scheduler::new(Sampler::DpmPP2mKarras, 25);
        assert_eq!(s.sigmas().len(), 26);
        assert_eq!(*s.sigmas().last().unwrap(), 0.0);
        assert!((s.sigmas()[0] - SIGMA_MAX).abs() < 0.05);
        assert!((s.sigmas()[24] - SIGMA_MIN).abs() < 0.01, "min sigma={}", s.sigmas()[24]);
        assert!(s.sigmas().windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn scale_model_input_divides_by_sqrt_sigma_sq_plus_one() {
        let s = Scheduler::new(Sampler::EulerAncestral, 2);
        let sigma = s.sigmas()[0];
        let scaled = s.scale_model_input(&[sigma, 2.0 * sigma], 0);
        let expect = 1.0 / (sigma * sigma + 1.0).sqrt();
        assert!((scaled[0] - sigma * expect).abs() < 1e-5);
        assert!((scaled[1] - 2.0 * sigma * expect).abs() < 1e-5);
    }

    #[test]
    fn ancestral_sigmas_match_reference_formula() {
        let (up, down) = ancestral_sigmas(2.0, 1.0);
        assert!((up - 0.8660254).abs() < 1e-5, "up={up}");
        assert!((down - 0.5).abs() < 1e-5, "down={down}");
    }

    #[test]
    fn euler_ancestral_step_deterministic_part() {
        // noise = 0 -> prev = sample + eps * (sigma_down - sigma_from)
        let mut sample = vec![10.0f32];
        euler_ancestral_step(2.0, 1.0, &[1.0], &mut sample, &[0.0]);
        let (_, down) = ancestral_sigmas(2.0, 1.0);
        assert!((sample[0] - (10.0 + 1.0 * (down - 2.0))).abs() < 1e-5, "got {}", sample[0]);
    }

    #[test]
    fn euler_ancestral_last_step_reaches_x0() {
        // sigma_next = 0 -> up = down = 0, prev = sample - sigma*eps = x0.
        let mut sample = vec![5.0f32];
        euler_ancestral_step(3.0, 0.0, &[1.0], &mut sample, &[0.0]);
        assert!((sample[0] - (5.0 - 3.0 * 1.0)).abs() < 1e-5, "got {}", sample[0]);
    }

    #[test]
    fn dpmpp_first_step_equals_sigma_space_ddim() {
        // First step (no history): x = x0 + sigma_next * eps.
        let mut sample = vec![10.0f32];
        let denoised = dpmpp_2m_step(2.0, 1.0, None, &[1.0], &mut sample, None);
        assert!((denoised[0] - 8.0).abs() < 1e-5);
        assert!((sample[0] - 9.0).abs() < 1e-4, "got {}", sample[0]);
    }

    #[test]
    fn dpmpp_last_step_reaches_x0() {
        let mut sample = vec![5.0f32];
        let denoised = dpmpp_2m_step(3.0, 0.0, Some(4.0), &[1.0], &mut sample, Some(&[7.0]));
        assert!((sample[0] - 2.0).abs() < 1e-5);
        assert!((denoised[0] - 2.0).abs() < 1e-5);
    }
}
