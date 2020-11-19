//! Performs a simple taint analysis, to find all live ranges that are reftyped.

use crate::data_structures::{
    InstPoint, Map, MoveInfo, MoveInfoElem, RangeFrag, RangeFragIx, RangeId, RealRange,
    RealRangeIx, Reg, RegClass, RegToRangesMaps, TypedIxVec, VirtualRange, VirtualRangeIx,
    VirtualReg,
};
use crate::sparse_set::{SparseSet, SparseSetU};
use std::{fmt, hash::Hash};

use log::debug;
use smallvec::SmallVec;

/// Parameters to configure a reftype analysis.
pub(crate) trait ReftypeAnalysis {
    /// An unified representation of a range, for both virtual and real ranges.
    type RangeId: Eq + Hash + Copy + fmt::Debug;

    /// Find the RangeId related to `reg` and containing `pt`. May panic if the point isn't
    /// actually present in any range of the given register.
    fn find_range_id_for_reg(&self, pt: InstPoint, reg: Reg) -> Self::RangeId;

    /// Add all the ranges associated to this vreg into the set of reftyped ranges.
    fn insert_reffy_ranges(&self, vreg: VirtualReg, set: &mut SparseSet<Self::RangeId>);

    /// Mark a given RangeId as being reffy.
    fn mark_reffy(&mut self, range_id: &Self::RangeId);
}

/// Implementation of the reftype analysis for the backtracking algorithm.
struct BacktrackingReftypeAnalysis<'a> {
    rlr_env: &'a mut TypedIxVec<RealRangeIx, RealRange>,
    vlr_env: &'a mut TypedIxVec<VirtualRangeIx, VirtualRange>,
    frag_env: &'a TypedIxVec<RangeFragIx, RangeFrag>,
    reg_to_ranges_maps: &'a RegToRangesMaps,
}

impl<'a> ReftypeAnalysis for BacktrackingReftypeAnalysis<'a> {
    type RangeId = RangeId;

    #[inline(always)]
    fn find_range_id_for_reg(&self, pt: InstPoint, reg: Reg) -> Self::RangeId {
        if reg.is_real() {
            for &rlrix in &self.reg_to_ranges_maps.rreg_to_rlrs_map[reg.get_index() as usize] {
                if self.rlr_env[rlrix]
                    .sorted_frags
                    .contains_pt(self.frag_env, pt)
                {
                    return RangeId::new_real(rlrix);
                }
            }
        } else {
            for &vlrix in &self.reg_to_ranges_maps.vreg_to_vlrs_map[reg.get_index() as usize] {
                if self.vlr_env[vlrix].sorted_frags.contains_pt(pt) {
                    return RangeId::new_virtual(vlrix);
                }
            }
        }
        panic!("do_reftypes_analysis::find_range_for_reg: can't find range");
    }

    #[inline(always)]
    fn mark_reffy(&mut self, range: &Self::RangeId) {
        if range.is_real() {
            let rrange = &mut self.rlr_env[range.to_real()];
            debug_assert!(!rrange.is_ref);
            debug!(" -> rrange {:?} is reffy", range.to_real());
            rrange.is_ref = true;
        } else {
            let vrange = &mut self.vlr_env[range.to_virtual()];
            debug_assert!(!vrange.is_ref);
            debug!(" -> rrange {:?} is reffy", range.to_virtual());
            vrange.is_ref = true;
        }
    }

    #[inline(always)]
    fn insert_reffy_ranges(&self, vreg: VirtualReg, set: &mut SparseSet<Self::RangeId>) {
        for vlr_ix in &self.reg_to_ranges_maps.vreg_to_vlrs_map[vreg.get_index()] {
            debug!("range {:?} is reffy due to reffy vreg {:?}", vlr_ix, vreg);
            set.insert(RangeId::new_virtual(*vlr_ix));
        }
    }
}

pub(crate) fn do_reftypes_analysis(
    // From dataflow/liveness analysis.  Modified by setting their is_ref bit.
    rlr_env: &mut TypedIxVec<RealRangeIx, RealRange>,
    vlr_env: &mut TypedIxVec<VirtualRangeIx, VirtualRange>,
    // From dataflow analysis
    frag_env: &TypedIxVec<RangeFragIx, RangeFrag>,
    reg_to_ranges_maps: &RegToRangesMaps,
    move_info: &MoveInfo,
    // As supplied by the client
    reftype_class: RegClass,
    reftyped_vregs: &Vec<VirtualReg>,
) {
    let mut analysis = BacktrackingReftypeAnalysis {
        rlr_env,
        vlr_env,
        frag_env,
        reg_to_ranges_maps,
    };
    core_reftypes_analysis(&mut analysis, move_info, reftype_class, reftyped_vregs);
}

pub(crate) fn core_reftypes_analysis<RA: ReftypeAnalysis>(
    analysis: &mut RA,
    move_info: &MoveInfo,
    // As supplied by the client
    reftype_class: RegClass,
    reftyped_vregs: &Vec<VirtualReg>,
) {
    // The game here is: starting with `reftyped_vregs`, find *all* the VirtualRanges and
    // RealRanges to which refness can flow, via instructions which the client's `is_move`
    // function considers to be moves.

    // This is done in three stages:
    //
    // (1) Create a mapping from source (virtual or real) ranges to sets of destination ranges.
    //     We have `move_info`, which tells us which (virtual or real) regs are connected by
    //     moves.  However, that's not directly useful -- we need to know which *ranges* are
    //     connected by moves.  `move_info` as supplied helpfully indicates both source and
    //     destination regs and ranges, so we can simply use that.
    //
    // (2) Similarly, convert `reftyped_vregs` into a set of reftyped ranges by consulting
    //     `reg_to_ranges_maps`.
    //
    // (3) Compute the transitive closure of (1) starting from the ranges in (2).  This is done
    //     by a depth first search of the graph implied by (1).

    // ====== Compute (1) above ======
    // Each entry in `succ` maps from `src` to a `SparseSet<dsts>`, so to speak.  That is, for
    // `d1`, `d2`, etc, in `dsts`, the function contains moves `d1 := src`, `d2 := src`, etc.
    let mut succ = Map::<RA::RangeId, SparseSetU<[RA::RangeId; 4]>>::default();
    for &MoveInfoElem { dst, src, iix, .. } in move_info.iter() {
        // Don't waste time processing moves which can't possibly be of reftyped values.
        debug_assert!(dst.get_class() == src.get_class());
        if dst.get_class() != reftype_class {
            continue;
        }
        let src_range = analysis.find_range_id_for_reg(InstPoint::new_use(iix), src);
        let dst_range = analysis.find_range_id_for_reg(InstPoint::new_def(iix), dst);
        debug!(
            "move from {:?} (range {:?}) to {:?} (range {:?}) at inst {:?}",
            src, src_range, dst, dst_range, iix
        );
        match succ.get_mut(&src_range) {
            Some(dst_ranges) => dst_ranges.insert(dst_range),
            None => {
                // Re `; 4`: we expect most copies copy a register to only a few destinations.
                let mut dst_ranges = SparseSetU::<[RA::RangeId; 4]>::empty();
                dst_ranges.insert(dst_range);
                let r = succ.insert(src_range, dst_ranges);
                assert!(r.is_none());
            }
        }
    }

    // ====== Compute (2) above ======
    let mut reftyped_ranges = SparseSet::<RA::RangeId>::empty();
    for vreg in reftyped_vregs {
        // If this fails, the client has been telling is that some virtual reg is reftyped, yet
        // it doesn't belong to the class of regs that it claims can carry refs.  So the client
        // is buggy.
        debug_assert!(vreg.get_class() == reftype_class);
        analysis.insert_reffy_ranges(*vreg, &mut reftyped_ranges);
    }

    // ====== Compute (3) above ======
    // Almost all chains of copies will be less than 64 long, I would guess.
    let mut stack = SmallVec::<[RA::RangeId; 64]>::new();
    let mut visited = reftyped_ranges.clone();
    for start_point_range in reftyped_ranges.iter() {
        // Perform DFS from `start_point_range`.
        stack.clear();
        stack.push(*start_point_range);
        while let Some(src_range) = stack.pop() {
            visited.insert(src_range);
            if let Some(dst_ranges) = succ.get(&src_range) {
                for dst_range in dst_ranges.iter() {
                    if !visited.contains(*dst_range) {
                        stack.push(*dst_range);
                    }
                }
            }
        }
    }

    // Finally, annotate the results of the analysis.
    for range in visited.iter() {
        analysis.mark_reffy(range);
    }
}
