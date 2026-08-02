#[derive(Debug, Clone, PartialEq)]
pub struct LodPolicy {
    pub stage_block: f32,
    pub stage_cell: f32,
    pub stage_cell_label: f32,
    pub block_min_px: f32,
    pub block_icon_min_px: f32,
    pub region_label_min_px: f32,
    pub block_label_min_px: f32,
    pub block_label_min_zoom: f32,
    pub cell_label_min_px: f32,
    pub block_chrome_min_px: f32,

    pub stage_exit: f32,
    pub sat_min_px: f32,
    pub sat_label_min_px: f32,
    pub max_labels: usize,
    pub max_icons: usize,
    pub max_edges: usize,
    pub max_curves: usize,
    pub max_cells_per_block: usize,
    pub sat_curves: bool,

    pub stress: bool,

    pub stress_curves: bool,
}

impl LodPolicy {
    const MIN_AGGREGATE_VISIBLE_FRACTION: f32 = 0.125;

    pub fn stage_for_zoom(&self, zoom: f32) -> u8 {
        let stage = if zoom >= self.stage_cell {
            2 + (zoom >= self.stage_cell_label) as u8
        } else {
            (zoom >= self.stage_block) as u8
        };
        if self.stress || self.stress_curves {
            stage.max(2)
        } else {
            stage
        }
    }

    pub fn stage_threshold(&self, stage: u8) -> f32 {
        match stage {
            0 => 0.0,
            1 => self.stage_block,
            2 => self.stage_cell,
            _ => self.stage_cell_label,
        }
    }

    pub fn stage_target(&self, current: u8, zoom: f32) -> u8 {
        let raw = self.stage_for_zoom(zoom);
        if raw >= current {
            return raw;
        }
        let mut stage = current;
        while stage > raw && zoom < self.stage_threshold(stage) * self.stage_exit {
            stage -= 1;
        }
        stage
    }

    #[inline]
    pub fn block_painted(&self, block_w: f32, zoom: f32) -> bool {
        block_w * zoom >= self.block_min_px || self.stress
    }

    #[inline]
    pub fn block_icon_shown(&self, block_w: f32, zoom: f32) -> bool {
        block_w * zoom >= self.block_icon_min_px && !self.stress
    }

    #[inline]
    pub fn region_label_shown(&self, region_w: f32, zoom: f32) -> bool {
        region_w * zoom > self.region_label_min_px
    }

    #[inline]
    pub fn block_label_shown(&self, block_w: f32, zoom: f32) -> bool {
        block_w * zoom > self.block_label_min_px && zoom > self.block_label_min_zoom
    }

    #[inline]
    pub fn cell_label_shown(&self, cell_w: f32, zoom: f32) -> bool {
        cell_w * zoom >= self.cell_label_min_px
    }

    #[inline]
    pub fn block_chrome_shown(&self, inner_w: f32, zoom: f32) -> bool {
        inner_w * zoom >= self.block_chrome_min_px && !self.stress && !self.stress_curves
    }

    #[inline]
    pub fn sat_painted(&self, sat_w: f32, zoom: f32) -> bool {
        sat_w * zoom >= self.sat_min_px || self.stress_curves
    }

    #[inline]
    pub fn sat_icon_shown(&self) -> bool {
        !self.stress && !self.stress_curves
    }

    #[inline]
    pub fn sat_label_shown(&self, sat_w: f32, zoom: f32) -> bool {
        sat_w * zoom >= self.sat_label_min_px && !self.stress && !self.stress_curves
    }

    #[inline]
    pub fn curve_budget(&self) -> usize {
        if self.stress_curves {
            usize::MAX
        } else {
            self.max_curves
        }
    }

    #[inline]
    pub fn cells_aggregated(&self, cells: usize, visible_fraction: f32) -> bool {
        cells > self.max_cells_per_block
            && visible_fraction >= Self::MIN_AGGREGATE_VISIBLE_FRACTION
            && !self.stress
            && !self.stress_curves
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageBlend {
    pub from: u8,
    pub to: u8,
    pub t: f32,
}

impl StageBlend {
    pub fn settled(stage: u8) -> Self {
        StageBlend {
            from: stage,
            to: stage,
            t: 1.0,
        }
    }

    pub fn is_settled(&self) -> bool {
        self.from == self.to
    }

    pub fn walk_stage(&self) -> u8 {
        self.from.max(self.to)
    }

    pub fn fade_alpha(&self) -> f32 {
        let t = self.t.clamp(0.0, 1.0);
        let eased = t * t * (3.0 - 2.0 * t);
        if self.to >= self.from {
            eased
        } else {
            1.0 - eased
        }
    }

    pub fn stage_alpha(&self, stage: u8) -> f32 {
        if stage <= self.from.min(self.to) {
            1.0
        } else {
            self.fade_alpha()
        }
    }
}

#[derive(Debug)]
pub struct StageMachine {
    fade_secs: f32,
    blend: Option<StageBlend>,
}

impl StageMachine {
    pub fn new(fade_secs: f32) -> Self {
        StageMachine {
            fade_secs,
            blend: None,
        }
    }

    pub fn update(&mut self, policy: &LodPolicy, zoom: f32, dt: f32) -> StageBlend {
        let Some(mut b) = self.blend else {
            let b = StageBlend::settled(policy.stage_for_zoom(zoom));
            self.blend = Some(b);
            return b;
        };
        if !b.is_settled() {
            b.t = if self.fade_secs > 0.0 {
                (b.t + dt / self.fade_secs).min(1.0)
            } else {
                1.0
            };
            if b.t >= 1.0 {
                b = StageBlend::settled(b.to);
            }
        }
        let target = policy.stage_target(b.to, zoom);
        if target != b.to {
            b = if !b.is_settled() && target == b.from {
                StageBlend {
                    from: b.to,
                    to: target,
                    t: 1.0 - b.t,
                }
            } else {
                StageBlend {
                    from: b.to,
                    to: target,
                    t: 0.0,
                }
            };
        }
        self.blend = Some(b);
        b
    }

    pub fn animating(&self) -> bool {
        self.blend.is_some_and(|b| !b.is_settled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> LodPolicy {
        LodPolicy {
            stage_block: 0.09,
            stage_cell: 0.55,
            stage_cell_label: 3.0,
            block_min_px: 4.0,
            block_icon_min_px: 14.0,
            region_label_min_px: 70.0,
            block_label_min_px: 60.0,
            block_label_min_zoom: 0.22,
            cell_label_min_px: 34.0,
            block_chrome_min_px: 34.0,
            stage_exit: 0.85,
            sat_min_px: 5.0,
            sat_label_min_px: 30.0,
            max_labels: 400,
            max_icons: 512,
            max_edges: 3000,
            max_curves: 1500,
            max_cells_per_block: 1024,
            sat_curves: true,
            stress: false,
            stress_curves: false,
        }
    }

    #[test]
    fn stage_target_enters_at_threshold_and_exits_below_band() {
        let pol = policy();
        assert_eq!(pol.stage_target(0, 0.0899), 0);
        assert_eq!(pol.stage_target(0, 0.09), 1);
        assert_eq!(pol.stage_target(1, 0.55), 2);
        assert_eq!(pol.stage_target(2, 0.50), 2, "0.50 >= 0.55 * 0.85");
        assert_eq!(pol.stage_target(2, 0.468), 2);
        assert_eq!(pol.stage_target(2, 0.467), 1);
        assert_eq!(pol.stage_target(3, 2.6), 3, "2.6 >= 3.0 * 0.85");
        assert_eq!(pol.stage_target(3, 2.5), 2);
        assert_eq!(pol.stage_target(3, 0.01), 0);
        assert_eq!(pol.stage_target(3, 0.5), 2);
    }

    #[test]
    fn stage_target_respects_stress_floor() {
        let mut pol = policy();
        pol.stress = true;
        assert_eq!(pol.stage_target(0, 0.01), 2);
        assert_eq!(pol.stage_target(3, 0.01), 2);
    }

    #[test]
    fn aggregate_lod_requires_both_fan_out_and_visible_area() {
        let mut pol = policy();
        assert!(!pol.cells_aggregated(pol.max_cells_per_block, 1.0));
        assert!(!pol.cells_aggregated(pol.max_cells_per_block + 1, 0.124));
        assert!(pol.cells_aggregated(pol.max_cells_per_block + 1, 0.125));

        pol.stress = true;
        assert!(!pol.cells_aggregated(usize::MAX, 1.0));
        pol.stress = false;
        pol.stress_curves = true;
        assert!(!pol.cells_aggregated(usize::MAX, 1.0));
    }

    #[test]
    fn blend_alphas() {
        let settled = StageBlend::settled(2);
        assert!(settled.is_settled());
        assert_eq!(settled.walk_stage(), 2);
        assert_eq!(settled.stage_alpha(1), 1.0);
        assert_eq!(settled.stage_alpha(2), 1.0);

        let fade_in = StageBlend {
            from: 1,
            to: 2,
            t: 0.5,
        };
        assert_eq!(fade_in.walk_stage(), 2);
        assert_eq!(fade_in.stage_alpha(1), 1.0);
        assert_eq!(fade_in.stage_alpha(2), 0.5);
        assert_eq!(
            StageBlend {
                from: 1,
                to: 2,
                t: 0.0
            }
            .stage_alpha(2),
            0.0
        );

        let fade_out = StageBlend {
            from: 2,
            to: 1,
            t: 0.0,
        };
        assert_eq!(fade_out.walk_stage(), 2);
        assert_eq!(fade_out.stage_alpha(2), 1.0);
        assert_eq!(
            StageBlend {
                from: 2,
                to: 1,
                t: 1.0
            }
            .stage_alpha(2),
            0.0
        );

        let jump = StageBlend {
            from: 3,
            to: 1,
            t: 0.5,
        };
        assert_eq!(jump.walk_stage(), 3);
        assert_eq!(jump.stage_alpha(1), 1.0);
        assert_eq!(jump.stage_alpha(2), 0.5);
        assert_eq!(jump.stage_alpha(3), 0.5);
    }

    #[test]
    fn machine_settles_instantly_on_first_update_then_fades() {
        let pol = policy();
        let mut m = StageMachine::new(0.2);
        let b = m.update(&pol, 1.0, 0.0);
        assert_eq!(b, StageBlend::settled(2), "no fade on startup");
        assert!(!m.animating());

        let b = m.update(&pol, 3.5, 0.016);
        assert_eq!((b.from, b.to, b.t), (2, 3, 0.0));
        assert!(m.animating());
        let b = m.update(&pol, 3.5, 0.1);
        assert_eq!((b.from, b.to), (2, 3));
        assert!((b.t - 0.5).abs() < 1e-4);
        let b = m.update(&pol, 3.5, 0.15);
        assert_eq!(b, StageBlend::settled(3));
        assert!(!m.animating());
    }

    #[test]
    fn machine_holds_inside_hysteresis_band_and_mirrors_reversals() {
        let pol = policy();
        let mut m = StageMachine::new(0.2);
        m.update(&pol, 1.0, 0.0);
        m.update(&pol, 3.5, 0.0);
        let b = m.update(&pol, 3.5, 0.1);
        assert!((b.t - 0.5).abs() < 1e-4);

        let b = m.update(&pol, 2.7, 0.02);
        assert_eq!((b.from, b.to), (2, 3));

        let alpha_before = b.stage_alpha(3);
        let b = m.update(&pol, 2.4, 0.0);
        assert_eq!((b.from, b.to), (3, 2));
        assert!((b.stage_alpha(3) - alpha_before).abs() < 1e-3);
        let b = m.update(&pol, 2.4, 1.0);
        assert_eq!(b, StageBlend::settled(2));
    }
}
