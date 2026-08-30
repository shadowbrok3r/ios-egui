//! What the build already knows, said out loud.
//!
//! The phone computed the whole report and put it in `on_hover_text` — a
//! press-and-hold nobody performs on glass. Metal weight in grams and
//! pennyweight is the number a jeweller quotes from, and until now the only way
//! to see it here was to export the casting sheet and open it through the share
//! sheet: a round trip to read a number already in memory.
//!
//! Three blocks from the desktop's report panel — dimensions, the alloy table,
//! and the per-seat stone check. The class-area bar and the draft-tick work stay
//! on the desktop; they want a wide strip. The Chvorinov hot spot stays too,
//! because `modulus_scan` is not run on the phone.

use egui_mobile::egui;
use ringdesign_core::mesh::Report;
use ringdesign_core::stones::{SeatFooting, StonesReport};

const WARN: egui::Color32 = egui::Color32::from_rgb(220, 170, 90);

/// Grams per pennyweight — the unit the US trade actually quotes in.
const GRAMS_PER_DWT: f64 = 1.555_173_84;

pub fn sheet(
    ui: &mut egui::Ui,
    report: Option<&Report>,
    stones: Option<&StonesReport>,
    size: &str,
    on_close: &mut bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("report").small().weak());
        if ui.small_button("close").clicked() {
            *on_close = true;
        }
    });
    ui.separator();

    let Some(r) = report else {
        ui.label(egui::RichText::new("no build yet").weak());
        return;
    };

    dimensions(ui, r, size);
    ui.add_space(6.0);
    metals(ui, r);
    ui.add_space(6.0);
    stones_section(ui, stones);
}

fn row(ui: &mut egui::Ui, k: &str, v: String) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(k).small().weak());
        ui.label(egui::RichText::new(v).strong());
    });
}

fn dimensions(ui: &mut egui::Ui, r: &Report, size: &str) {
    ui.label(egui::RichText::new("dimensions").small().weak());
    row(ui, "size", size.to_string());
    row(ui, "inside Ø", format!("{:.2} mm", r.inner_diameter_mm));
    row(ui, "outside Ø", format!("{:.2} mm", r.outer_diameter_mm));
    row(ui, "band width", format!("{:.2} mm", r.band_width_mm));
    row(
        ui,
        "relief",
        format!("+{:.2} / {:.2} mm", r.max_relief_mm, r.min_relief_mm),
    );
    row(ui, "volume", format!("{:.1} mm³", r.volume_mm3));
    row(ui, "surface", format!("{:.1} mm²", r.surface_area_mm2));
}

/// Every alloy the core knows, in grams and pennyweight.
///
/// No prices: `prices.json` is a desktop file beside the designs folder and has
/// no equivalent here, so the phone shows weights alone — which is what the
/// desktop's own table degrades to when the file is absent.
fn metals(ui: &mut egui::Ui, r: &Report) {
    ui.label(egui::RichText::new("cast weight").small().weak());
    egui::Grid::new("report_metals").num_columns(3).striped(true).show(ui, |ui| {
        ui.label(egui::RichText::new("metal").small().weak());
        ui.label(egui::RichText::new("g").small().weak());
        ui.label(egui::RichText::new("dwt").small().weak());
        ui.end_row();
        for m in &r.metals {
            ui.label(m.metal);
            ui.label(format!("{:.2}", m.grams));
            ui.label(format!("{:.2}", m.grams / GRAMS_PER_DWT));
            ui.end_row();
        }
    });
    ui.label(
        egui::RichText::new("Casting weight only — no sprue, button or finishing loss.")
            .small()
            .weak(),
    );
}

fn stones_section(ui: &mut egui::Ui, stones: Option<&StonesReport>) {
    ui.label(egui::RichText::new("stones").small().weak());
    let Some(s) = stones.filter(|s| !s.seats.is_empty()) else {
        ui.label(egui::RichText::new("none set").weak());
        return;
    };

    row(
        ui,
        "total",
        format!("{} stones · {:.2} ct", s.stone_count, s.total_carats),
    );

    for seat in &s.seats {
        ui.add_space(4.0);
        let count = if seat.count > 1 { format!(" x{}", seat.count) } else { String::new() };
        let stone = seat
            .gem
            .as_ref()
            .map(|g| g.display())
            .unwrap_or_else(|| "no stone".to_string());
        ui.label(egui::RichText::new(format!("{}{count} — {stone}", seat.label)).strong());
        ui.label(
            egui::RichText::new(format!(
                "{:.2} mm {} · {}",
                seat.seat_diameter_mm,
                seat.style.label(),
                match seat.footing {
                    // A face square to the pull is castable by construction;
                    // on the crown the number is what decides.
                    SeatFooting::SideFace => "on a side face".to_string(),
                    SeatFooting::Crown(deg) => format!("on the crown, {deg:.1}° draft"),
                }
            ))
            .small(),
        );
        ui.label(
            egui::RichText::new(format!(
                "edge {:.2} mm · pavilion room {:.2} mm{}",
                seat.edge_clearance_mm,
                seat.depth_available_mm,
                seat.bridge_mm.map(|b| format!(" · bridge {b:.2} mm")).unwrap_or_default()
            ))
            .small()
            .weak(),
        );
        for w in &seat.warnings {
            ui.label(egui::RichText::new(w).small().color(WARN));
        }
    }

    if !s.crowding.is_empty() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("tightest neighbours").small().weak());
        for p in &s.crowding {
            // The deep gap is the one that decides: the ring's own curvature
            // closes the arc in below the girdle, and step cuts keep their
            // width all the way down.
            let worst = p.gap_mm.min(p.gap_deep_mm);
            ui.label(
                egui::RichText::new(format!(
                    "{} to {} — {:.2} mm at the girdle, {:.2} deep",
                    p.a, p.b, p.gap_mm, p.gap_deep_mm
                ))
                .small()
                .color(if worst < 0.3 { WARN } else { egui::Color32::from_rgb(150, 190, 150) }),
            );
        }
        if s.tight_pairs > s.crowding.len() {
            ui.label(
                egui::RichText::new(format!(
                    "and {} more pairs under the bench floor",
                    s.tight_pairs - s.crowding.len()
                ))
                .small()
                .color(WARN),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    /// The trade quotes in pennyweight and the core reports grams; a wrong
    /// constant here is a wrong quote, silently.
    #[test]
    fn grams_convert_to_pennyweight_at_the_trade_constant() {
        let g = 15.55173840;
        assert!((g / super::GRAMS_PER_DWT - 10.0).abs() < 1e-9);
    }
}
