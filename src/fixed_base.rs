#![allow(non_snake_case)]
use group::Curve;
use halo2_base::{gates::GateInstructions, utils::CurveAffineExt, AssignedValue, Context};
use halo2_ecc::{
    bigint::CRTInteger,
    ecc::{fixed_base::FixedEcPoint, EcPoint},
    fields::{PrimeField, PrimeFieldChip, Selectable},
};
use itertools::Itertools;
use std::cmp::min;

use super::ecc::{ec_add, ec_select, ec_select_from_bits};

// computes `[scalar] * P` on y^2 = x^3 + b where `P` is fixed (constant)
// - `scalar` is represented as a reference array of `AssignedCell`s
// - `scalar = sum_i scalar_i * 2^{max_bits * i}`
// - an array of length > 1 is needed when `scalar` exceeds the modulus of scalar field `F`
// assumes:
// - `scalar_i < 2^{max_bits} for all i` (constrained by num_to_bits)
// - `max_bits <= modulus::<F>.bits()`
pub fn scalar_multiply<F, FC, C>(
    chip: &FC,
    ctx: &mut Context<F>,
    point: &C,
    scalar: Vec<AssignedValue<F>>,
    max_bits: usize,
    window_bits: usize,
) -> EcPoint<F, FC::FieldPoint>
where
    F: PrimeField,
    C: CurveAffineExt,
    C::Base: PrimeField,
    FC: PrimeFieldChip<F, FieldType = C::Base, FieldPoint = CRTInteger<F>>
        + Selectable<F, Point = FC::FieldPoint>,
{
    if point.is_identity().into() {
        let point = FixedEcPoint::from_curve(*point, chip.num_limbs(), chip.limb_bits());
        return FixedEcPoint::assign(point, chip, ctx);
    }
    debug_assert!(!scalar.is_empty());
    debug_assert!((max_bits as u32) <= F::NUM_BITS);

    let total_bits = max_bits * scalar.len();
    let num_windows = (total_bits + window_bits - 1) / window_bits;

    // Jacobian coordinate
    let base_pt = point.to_curve();
    // cached_points[i * 2^w + j] holds `[j * 2^(i * w)] * point` for j in {0, ..., 2^w - 1}

    // first we compute all cached points in Jacobian coordinates since it's fastest
    let mut increment = base_pt;
    let cached_points_jacobian = (0..num_windows)
        .flat_map(|i| {
            let mut curr = increment;
            // start with increment at index 0 instead of identity just as a dummy value to avoid divide by 0 issues
            let cache_vec = std::iter::once(increment)
                .chain(
                    (1..(1usize << min(window_bits, total_bits - i * window_bits))).map(|_| {
                        let prev = curr;
                        curr += increment;
                        prev
                    }),
                )
                .collect::<Vec<_>>();
            increment = curr;
            cache_vec
        })
        .collect::<Vec<_>>();
    // for use in circuits we need affine coordinates, so we do a batch normalize: this is much more efficient than calling `to_affine` one by one since field inversion is very expensive
    // initialize to all 0s
    let mut cached_points_affine = vec![C::default(); cached_points_jacobian.len()];
    C::Curve::batch_normalize(&cached_points_jacobian, &mut cached_points_affine);

    // TODO: do not assign and use select_from_bits on Constant(_) QuantumCells
    let cached_points = cached_points_affine
        .into_iter()
        .map(|point| {
            let point = FixedEcPoint::from_curve(point, chip.num_limbs(), chip.limb_bits());
            FixedEcPoint::assign(point, chip, ctx)
        })
        .collect_vec();

    let bits = scalar
        .into_iter()
        .flat_map(|scalar_chunk| chip.gate().num_to_bits(ctx, scalar_chunk, max_bits))
        .collect::<Vec<_>>();

    let cached_point_window_rev = cached_points.chunks(1usize << window_bits).rev();
    let bit_window_rev = bits.chunks(window_bits).rev();
    let mut curr_point = None;
    // `is_started` is just a way to deal with if `curr_point` is actually identity
    let mut is_started = ctx.load_zero();
    for (cached_point_window, bit_window) in cached_point_window_rev.zip(bit_window_rev) {
        let bit_sum = chip.gate().sum(ctx, bit_window.iter().copied());
        // are we just adding a window of all 0s? if so, skip
        let is_zero_window = chip.gate().is_zero(ctx, bit_sum);
        let add_point = ec_select_from_bits::<F, _>(chip, ctx, cached_point_window, bit_window);
        curr_point = if let Some(curr_point) = curr_point {
            let sum = ec_add::<F, FC, C>(chip, ctx, &curr_point, &add_point);
            let zero_sum = ec_select(chip, ctx, &curr_point, &sum, is_zero_window);
            Some(ec_select(chip, ctx, &zero_sum, &add_point, is_started))
        } else {
            Some(add_point)
        };
        is_started = {
            // is_started || !is_zero_window
            // (a || !b) = (1-b) + a*b
            let not_zero_window = chip.gate().not(ctx, is_zero_window);
            chip.gate()
                .mul_add(ctx, is_started, is_zero_window, not_zero_window)
        };
    }
    curr_point.unwrap()
}
