//! Choosing a stone, and packing a run of them.
//!
//! The core carries fourteen cuts, two forms, three setting styles and a
//! calibrated stock table, and the phone exposed none of it: Auto pavé ran
//! `PaveSpec::default()` and Channel set built a 1.5 mm round inline, so the
//! app produced exactly one arrangement. Stone size and arc span are the design
//! decision, not a refinement — they are the difference between a full eternity
//! band and three stones on a shoulder.
//!
//! `SeatPadLayer::fit_stone` already fills the seat's diameter, elongation,
//! plan exponent and bezel height from the gem, so this is a picker over an API
//! that does the work.

use egui_mobile::egui;
use ringdesign_core::field::{SeatStyle, SideFacePick};
use ringdesign_core::gem::{Gem, GemCut, GemForm};
use ringdesign_core::pave::{PaveRegion, PaveSpec};

/// The stone being chosen, and where a fill of them would go.
#[derive(Clone, Debug)]
pub struct Pick {
    pub cut: GemCut,
    pub form: GemForm,
    /// Girdle width in mm, from the cut's own calibrated stock list.
    pub w_mm: f64,
    pub style: SeatStyle,
    pub theta_deg: f64,
    pub span_deg: f64,
    pub stagger: bool,
    /// `None` is the wider side face; `Some(v)` is a strip centred there.
    pub v_band: Option<f64>,
}

impl Default for Pick {
    fn default() -> Self {
        Self {
            cut: GemCut::Round,
            form: GemForm::Faceted,
            w_mm: 1.5,
            style: SeatStyle::GypsyMound,
            theta_deg: ringdesign_core::profile::TOP_DEG,
            span_deg: 360.0,
            stagger: true,
            v_band: None,
        }
    }
}

impl Pick {
    /// The stone itself. `Gem::calibrated` gives the cut its proper length for
    /// the chosen width, so an oval stays an oval.
    pub fn gem(&self) -> Gem {
        let mut g = Gem::calibrated(self.cut, self.w_mm);
        g.form = self.form;
        g
    }

    /// Nearest listed stock size to `mm` for this cut.
    ///
    /// Melee is sold in steps, not continuously, so a slider that lands on
    /// 1.63 mm names a stone nobody can buy.
    pub fn snap(cut: GemCut, mm: f64) -> f64 {
        cut.calibrated_mm()
            .iter()
            .copied()
            .min_by(|a, b| {
                (a - mm).abs().partial_cmp(&(b - mm).abs()).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(mm)
    }

    /// A pavé spec from this pick, keeping the fields the phone does not show
    /// at their defaults.
    pub fn spec(&self) -> PaveSpec {
        PaveSpec {
            gem: self.gem(),
            theta_deg: self.theta_deg,
            span_deg: self.span_deg,
            stagger: self.stagger,
            style: self.style,
            region: match self.v_band {
                // The default resolves to nothing on a domed band, and `fill`
                // returns None — which is the "no side face to fill" refusal.
                // A v-band is how a fill moves off the face instead.
                None => PaveRegion::SideFace(SideFacePick::Wider),
                Some(center_mm) => PaveRegion::VBand { center_mm, width_mm: self.w_mm * 2.5 },
            },
            ..PaveSpec::default()
        }
    }
}

/// The picker. Returns true when something changed.
pub fn picker(ui: &mut egui::Ui, p: &mut Pick, band_v_len_mm: f64) -> bool {
    let mut c = false;

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("cut").small().weak());
        let cur = p.cut;
        egui::ComboBox::from_id_salt("gem_cut").selected_text(cur.label()).show_ui(ui, |ui| {
            for &g in GemCut::ALL {
                if ui.selectable_label(cur == g, g.label()).clicked() && cur != g {
                    p.cut = g;
                    // Stock lists differ per cut, so carry the size to the
                    // nearest one this cut is actually sold in.
                    p.w_mm = Pick::snap(g, p.w_mm);
                    c = true;
                }
            }
        });
        for &f in GemForm::ALL {
            if ui.add(crate::theme::selectable(p.form == f, f.label())).clicked() && p.form != f {
                p.form = f;
                c = true;
            }
        }
    });

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("size").small().weak());
        for &mm in p.cut.calibrated_mm() {
            let sel = (p.w_mm - mm).abs() < 1e-9;
            if ui.add(crate::theme::selectable(sel, format!("{mm}"))).clicked() && !sel {
                p.w_mm = mm;
                c = true;
            }
        }
    });

    let g = p.gem();
    ui.label(
        egui::RichText::new(format!(
            "{} · {:.2} ct · {:.2} mm deep",
            g.display(),
            g.carats(),
            g.depth_mm()
        ))
        .small()
        .weak(),
    );

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("setting").small().weak());
        for &s in SeatStyle::ALL {
            if ui.add(crate::theme::selectable(p.style == s, s.label())).clicked() && p.style != s {
                p.style = s;
                c = true;
            }
        }
    });

    c |= ui.add(egui::Slider::new(&mut p.theta_deg, 0.0..=360.0).text("centre deg")).changed();
    c |= ui.add(egui::Slider::new(&mut p.span_deg, 10.0..=360.0).text("span deg")).changed();
    c |= ui.checkbox(&mut p.stagger, "stagger rows").changed();

    ui.horizontal_wrapped(|ui| {
        let mut on_face = p.v_band.is_none();
        if ui.checkbox(&mut on_face, "on the side face").changed() {
            p.v_band = if on_face { None } else { Some(band_v_len_mm * 0.5) };
            c = true;
        }
    });
    if let Some(v) = p.v_band.as_mut() {
        c |= ui
            .add(egui::Slider::new(v, 0.0..=band_v_len_mm.max(0.5)).text("centre v mm"))
            .changed();
        ui.label(
            egui::RichText::new(
                "Off the side face the seats sit on the dome, where their rims can lock. \
                 The verdict will say.",
            )
            .small()
            .color(egui::Color32::from_rgb(220, 170, 90)),
        );
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_snaps_to_stock_the_cut_is_actually_sold_in() {
        // Melee comes in steps; 1.55 names a stone nobody can buy.
        assert_eq!(Pick::snap(GemCut::Round, 1.55), 1.5);
        assert_eq!(Pick::snap(GemCut::Round, 1.7), 1.75);
        // And it really is nearest, not "round down": 1.63 is closer to 1.75.
        assert_eq!(Pick::snap(GemCut::Round, 1.63), 1.75);
        // A baguette's list is much shorter and stops at 3.
        assert_eq!(Pick::snap(GemCut::Baguette, 9.0), 3.0);
    }

    #[test]
    fn switching_cut_carries_the_size_into_the_new_stock_list() {
        let mut p = Pick::default();
        p.w_mm = 8.0;
        let moved = Pick::snap(GemCut::Baguette, p.w_mm);
        assert!(
            GemCut::Baguette.calibrated_mm().contains(&moved),
            "{moved} is not baguette stock"
        );
    }

    #[test]
    fn the_gem_keeps_the_chosen_form_and_cut() {
        let mut p = Pick::default();
        p.cut = GemCut::Oval;
        p.form = GemForm::Cabochon;
        p.w_mm = 5.0;
        let g = p.gem();
        assert_eq!(g.cut, GemCut::Oval);
        assert_eq!(g.form, GemForm::Cabochon);
        assert!(g.l_mm >= g.w_mm, "an oval is longer than it is wide");
    }

    /// The default is the side face, which is the whole castability story; a
    /// v-band is the deliberate way off it.
    #[test]
    fn the_default_region_is_the_wider_side_face() {
        let p = Pick::default();
        assert!(matches!(
            p.spec().region,
            PaveRegion::SideFace(SideFacePick::Wider)
        ));
        let mut off = p.clone();
        off.v_band = Some(2.0);
        assert!(matches!(off.spec().region, PaveRegion::VBand { .. }));
    }

    #[test]
    fn the_spec_carries_the_picked_arc_and_stagger() {
        let mut p = Pick::default();
        p.span_deg = 90.0;
        p.theta_deg = 45.0;
        p.stagger = false;
        let s = p.spec();
        assert_eq!(s.span_deg, 90.0);
        assert_eq!(s.theta_deg, 45.0);
        assert!(!s.stagger);
        assert_eq!(s.style, p.style);
    }
}
