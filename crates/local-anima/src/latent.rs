//! Wan 2.1 16-channel latent normalization. The VAE scale factor is 1.0; the
//! SD 0.18215 factor does not apply here.

/// Latent channel count.
pub const LATENT_CHANNELS: usize = 16;

/// VAE downscale factor from pixels to latents.
pub const VAE_SCALE: usize = 8;

/// Per-channel latent means.
pub const WAN_MEAN: [f32; LATENT_CHANNELS] = [
    -0.7571, -0.7089, -0.9113, 0.1075, -0.1745, 0.9653, -0.1517, 1.5508,
    0.4134, -0.0715, 0.5517, -0.3632, -0.1922, -0.9497, 0.2503, -0.2921,
];

/// Per-channel latent standard deviations.
pub const WAN_STD: [f32; LATENT_CHANNELS] = [
    2.8184, 1.4541, 2.3275, 2.6558, 1.2196, 1.7708, 2.6052, 2.0743,
    3.2687, 2.1526, 2.8652, 1.5579, 1.6382, 1.1253, 2.8251, 1.9160,
];

/// Denormalize model-space latents to VAE space: `v * STD[c] + MEAN[c]`.
pub fn model_to_vae(latents: &[f32], plane: usize) -> Vec<f32> {
    latents
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let c = (i / plane) % LATENT_CHANNELS;
            v * WAN_STD[c] + WAN_MEAN[c]
        })
        .collect()
}

/// Normalize VAE-space latents to model space: `(v - MEAN[c]) / STD[c]`.
pub fn vae_to_model(latents: &[f32], plane: usize) -> Vec<f32> {
    latents
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let c = (i / plane) % LATENT_CHANNELS;
            (v - WAN_MEAN[c]) / WAN_STD[c]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_recovers_the_input() {
        let plane = 4;
        let n = LATENT_CHANNELS * plane;
        let src: Vec<f32> = (0..n).map(|i| (i as f32) * 0.017 - 1.3).collect();
        let back = vae_to_model(&model_to_vae(&src, plane), plane);
        for (a, b) in src.iter().zip(&back) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn model_to_vae_applies_the_channel_stats() {
        let plane = 2;
        let src = vec![1.0f32; LATENT_CHANNELS * plane];
        let out = model_to_vae(&src, plane);
        for c in 0..LATENT_CHANNELS {
            let want = WAN_STD[c] + WAN_MEAN[c];
            assert!((out[c * plane] - want).abs() < 1e-6, "c={c}");
            assert!((out[c * plane + 1] - want).abs() < 1e-6, "c={c}");
        }
    }

    #[test]
    fn zero_latent_maps_to_the_channel_means() {
        let plane = 1;
        let out = model_to_vae(&vec![0.0f32; LATENT_CHANNELS], plane);
        assert_eq!(out, WAN_MEAN.to_vec());
    }

    #[test]
    fn scale_factor_is_not_the_sd_constant() {
        // The SD 0.18215 latent scale must not appear anywhere in the stats.
        assert!(WAN_STD.iter().all(|&s| (s - 0.18215).abs() > 1e-3));
    }
}
