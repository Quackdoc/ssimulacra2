use std::{f64::consts::PI, mem::size_of};

use aligned::{Aligned, A16};
use nalgebra::base::{Matrix3, Matrix3x1};
use num_traits::PrimInt;
use wide::f32x4;

pub struct Blur {
    kernel: RecursiveGaussian,
    temp: Vec<f32>,
    width: usize,
    height: usize,
}

impl Blur {
    pub fn new(width: usize, height: usize) -> Self {
        Blur {
            kernel: RecursiveGaussian::new(1.5),
            temp: vec![0.0f32; width * height],
            width,
            height,
        }
    }

    pub fn shrink_to(&mut self, width: usize, height: usize) {
        self.temp.truncate(width * height);
        self.width = width;
        self.height = height;
    }

    pub fn blur(&mut self, img: &[Vec<f32>; 3]) -> [Vec<f32>; 3] {
        [
            self.blur_plane(&img[0]),
            self.blur_plane(&img[1]),
            self.blur_plane(&img[2]),
        ]
    }

    fn blur_plane(&mut self, plane: &[f32]) -> Vec<f32> {
        let mut out = vec![0f32; self.width * self.height];
        self.kernel
            .fast_gaussian_horizontal(plane, &mut self.temp, self.width, self.height);
        self.kernel
            .fast_gaussian_vertical(&self.temp, &mut out, self.width, self.height);
        out
    }
}

const V_CACHE_LINE_LANES: usize = 64 / size_of::<f32>();
const V_MAX_LANES: usize = 4;
const V_CACHE_LINE_VECTORS: usize = V_CACHE_LINE_LANES / V_MAX_LANES;
const V_TOTAL_LANES: usize = V_CACHE_LINE_VECTORS * V_MAX_LANES;
const V_MOD: usize = 4;
const V_PREFETCH_ROWS: usize = 8;

/// Implements "Recursive Implementation of the Gaussian Filter Using Truncated
/// Cosine Functions" by Charalampidis [2016].
struct RecursiveGaussian {
    radius: usize,
    /// For k={1,3,5} in that order, each broadcasted 4x for LoadDup128. Used
    /// only for vertical passes.
    n2: Aligned<A16, [f32; 3 * 4]>,
    d1: Aligned<A16, [f32; 3 * 4]>,
    /// We unroll horizontal passes 4x - one output per lane. These are each
    /// lane's multiplier for the previous output (relative to the first of
    /// the four outputs). Indexing: 4 * 0..2 (for {1,3,5}) + 0..3 for the
    /// lane index.
    mul_prev: Aligned<A16, [f32; 3 * 4]>,
    /// Ditto for the second to last output.
    mul_prev2: Aligned<A16, [f32; 3 * 4]>,
    /// We multiply a vector of inputs 0..3 by a vector shifted from this array.
    /// in=0 uses all 4 (nonzero) terms; for in=3, the lower three lanes are 0.
    mul_in: Aligned<A16, [f32; 3 * 4]>,
}

impl RecursiveGaussian {
    pub fn new(sigma: f64) -> Self {
        // (57), "N"
        let radius = 3.2795f64.mul_add(sigma, 0.2546);

        // Table I, first row
        let pi_div_2r = PI / (2.0f64 * radius);
        let omega = [pi_div_2r, 3.0f64 * pi_div_2r, 5.0f64 * pi_div_2r];

        // (37), k={1,3,5}
        let p_1 = 1.0f64 / (0.5 * omega[0]).tan();
        let p_3 = -1.0f64 / (0.5 * omega[1]).tan();
        let p_5 = 1.0f64 / (0.5 * omega[2]).tan();

        // (44), k={1,3,5}
        let r_1 = p_1 * p_1 / omega[0].sin();
        let r_3 = -p_3 * p_3 / omega[1].sin();
        let r_5 = p_5 * p_5 / omega[2].sin();

        // (50), k={1,3,5}
        let neg_half_sigma2 = -0.5f64 * sigma * sigma;
        let recip_radius = 1.0f64 / radius;
        let mut rho = [0.0f64; 3];
        for i in 0..3 {
            rho[i] = (neg_half_sigma2 * omega[i] * omega[i]).exp() * recip_radius;
        }

        // second part of (52), k1,k2 = 1,3; 3,5; 5,1
        let d_13 = p_1 * r_3 - r_1 * p_3;
        let d_35 = p_3 * r_5 - r_3 * p_5;
        let d_51 = p_5 * r_1 - r_5 * p_1;

        // (52), k=5
        let recip_d13 = 1.0f64 / d_13;
        let zeta_15 = d_35 * recip_d13;
        let zeta_35 = d_51 * recip_d13;

        // (56)
        let a = Matrix3::from_row_slice(&[p_1, p_3, p_5, r_1, r_3, r_5, zeta_15, zeta_35, 1.0f64]);
        assert!(a.try_inverse().is_some());
        // (55)
        let gamma = Matrix3x1::from_column_slice(&[
            1.0f64,
            radius * radius - sigma * sigma,
            zeta_15.mul_add(rho[0], zeta_35 * rho[1]) + rho[2],
        ]);
        // (53)
        let beta = a * gamma;

        // Sanity check: correctly solved for beta (IIR filter weights are normalized)
        // (39)
        let sum = beta[2].mul_add(p_5, beta[0].mul_add(p_1, beta[1] * p_3));
        assert!((sum - 1.0).abs() < 1E-12f64);

        let mut n2 = [0f64; 3];
        let mut d1 = [0f64; 3];
        let mut rg_n2 = [0f32; 3 * 4];
        let mut rg_d1 = [0f32; 3 * 4];
        let mut mul_prev = [0f32; 3 * 4];
        let mut mul_prev2 = [0f32; 3 * 4];
        let mut mul_in = [0f32; 3 * 4];
        for i in 0..3 {
            // (33)
            n2[i] = -beta[i] * (omega[i] * (radius + 1.0)).cos();
            d1[i] = -2.0f64 * omega[i].cos();

            for lane in 0..4 {
                rg_n2[4 * i + lane] = n2[i] as f32;
                rg_d1[4 * i + lane] = d1[i] as f32;
            }

            let d_2 = d1[i] * d1[i];

            // Obtained by expanding (35) for four consecutive outputs via
            // sympy: n, d, p, pp = symbols('n d p pp')
            // i0, i1, i2, i3 = symbols('i0 i1 i2 i3')
            // o0, o1, o2, o3 = symbols('o0 o1 o2 o3')
            // o0 = n*i0 - d*p - pp
            // o1 = n*i1 - d*o0 - p
            // o2 = n*i2 - d*o1 - o0
            // o3 = n*i3 - d*o2 - o1
            // Then expand(o3) and gather terms for p(prev), pp(prev2) etc.
            mul_prev[4 * i] = -d1[i] as f32;
            mul_prev[4 * i + 1] = (d_2 - 1.0f64) as f32;
            mul_prev[4 * i + 2] = (-d_2).mul_add(d1[i], 2.0f64 * d1[i]) as f32;
            mul_prev[4 * i + 3] = (d_2 * d_2 - 3.0f64 * d_2 + 1.0f64) as f32;
            mul_prev2[4 * i] = -1.0f32;
            mul_prev2[4 * i + 1] = d1[i] as f32;
            mul_prev2[4 * i + 2] = (-d_2 + 1.0f64) as f32;
            mul_prev2[4 * i + 3] = (d_2 * d1[i] - 2.0f64 * d1[i]) as f32;
            mul_in[4 * i] = n2[i] as f32;
            mul_in[4 * i + 1] = (-d1[i] * n2[i]) as f32;
            mul_in[4 * i + 2] = (d_2 * n2[i] - n2[i]) as f32;
            mul_in[4 * i + 3] = (-d_2 * d1[i]).mul_add(n2[i], 2.0f64 * d1[i] * n2[i]) as f32;
        }

        Self {
            radius: radius as usize,
            n2: Aligned(rg_n2),
            d1: Aligned(rg_d1),
            mul_prev: Aligned(mul_prev),
            mul_prev2: Aligned(mul_prev2),
            mul_in: Aligned(mul_in),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn fast_gaussian_horizontal(
        &self,
        input: &[f32],
        output: &mut [f32],
        width: usize,
        height: usize,
    ) {
        assert_eq!(input.len(), output.len());

        let radius = self.radius as isize;
        for y in 0..height {
            let input = &input[(y * width)..][..width];
            let output = &mut output[(y * width)..][..width];

            // Although the current output depends on the previous output, we can unroll
            // up to 4x by precomputing up to fourth powers of the constants. Beyond that,
            // numerical precision might become a problem.
            //
            // Rust optimization: Casting from a slice requires a match statement to know
            // the length of the input by the `wide` crate. Using a static size array allows
            // a direct cast.
            let mul_in_1 = f32x4::from([
                self.mul_in[0],
                self.mul_in[1],
                self.mul_in[2],
                self.mul_in[3],
            ]);
            let mul_in_3 = f32x4::from([
                self.mul_in[4],
                self.mul_in[5],
                self.mul_in[6],
                self.mul_in[7],
            ]);
            let mul_in_5 = f32x4::from([
                self.mul_in[8],
                self.mul_in[9],
                self.mul_in[10],
                self.mul_in[11],
            ]);
            let mul_prev_1 = f32x4::from([
                self.mul_prev[0],
                self.mul_prev[1],
                self.mul_prev[2],
                self.mul_prev[3],
            ]);
            let mul_prev_3 = f32x4::from([
                self.mul_prev[4],
                self.mul_prev[5],
                self.mul_prev[6],
                self.mul_prev[7],
            ]);
            let mul_prev_5 = f32x4::from([
                self.mul_prev[8],
                self.mul_prev[9],
                self.mul_prev[10],
                self.mul_prev[11],
            ]);
            let mul_prev2_1 = f32x4::from([
                self.mul_prev2[0],
                self.mul_prev2[1],
                self.mul_prev2[2],
                self.mul_prev2[3],
            ]);
            let mul_prev2_3 = f32x4::from([
                self.mul_prev2[4],
                self.mul_prev2[5],
                self.mul_prev2[6],
                self.mul_prev2[7],
            ]);
            let mul_prev2_5 = f32x4::from([
                self.mul_prev2[8],
                self.mul_prev2[9],
                self.mul_prev2[10],
                self.mul_prev2[11],
            ]);
            let mut prev_1 = f32x4::ZERO;
            let mut prev_3 = f32x4::ZERO;
            let mut prev_5 = f32x4::ZERO;
            let mut prev2_1 = f32x4::ZERO;
            let mut prev2_3 = f32x4::ZERO;
            let mut prev2_5 = f32x4::ZERO;

            let mut n = -radius + 1;
            // Left side with bounds checks and only write output after n >= 0.
            let first_aligned = round_up_to(radius, 4);
            while n < (first_aligned.min(width as isize)) {
                let left = n - radius - 1;
                let right = n + radius - 1;
                let left_val = if left >= 0 {
                    input[left as usize]
                } else {
                    0f32
                };
                let right_val = if right < width as isize {
                    input[right as usize]
                } else {
                    0f32
                };
                let sum = left_val + right_val;
                let sum = f32x4::from([sum; 4]);

                // (Only processing a single lane here, no need to broadcast)
                let mut out_1 = sum * mul_in_1;
                let mut out_3 = sum * mul_in_3;
                let mut out_5 = sum * mul_in_5;

                out_1 = mul_prev2_1.mul_add(prev2_1, out_1);
                out_3 = mul_prev2_3.mul_add(prev2_3, out_3);
                out_5 = mul_prev2_5.mul_add(prev2_5, out_5);
                prev2_1 = prev_1;
                prev2_3 = prev_3;
                prev2_5 = prev_5;

                out_1 = mul_prev_1.mul_add(prev_1, out_1);
                out_3 = mul_prev_3.mul_add(prev_3, out_3);
                out_5 = mul_prev_5.mul_add(prev_5, out_5);
                prev_1 = out_1;
                prev_3 = out_3;
                prev_5 = out_5;

                if n >= 0 {
                    output[n as usize] = (out_1 + out_3 + out_5).to_array()[0];
                }

                n += 1;
            }

            // The above loop is effectively scalar but it is convenient to use the same
            // prev/prev2 variables, so broadcast to each lane before the unrolled loop.
            prev2_1 = f32x4::from([prev2_1.to_array()[0]; 4]);
            prev2_3 = f32x4::from([prev2_3.to_array()[0]; 4]);
            prev2_5 = f32x4::from([prev2_5.to_array()[0]; 4]);
            prev_1 = f32x4::from([prev_1.to_array()[0]; 4]);
            prev_3 = f32x4::from([prev_3.to_array()[0]; 4]);
            prev_5 = f32x4::from([prev_5.to_array()[0]; 4]);

            // Unrolled, no bounds checking needed.
            while n < width as isize - radius + 1 - (4 - 1) {
                let in1 = &input[(n - radius - 1) as usize..][..4];
                let in2 = &input[(n + radius - 1) as usize..][..4];
                let sum = f32x4::from([in1[0], in1[1], in1[2], in1[3]])
                    + f32x4::from([in2[0], in2[1], in2[2], in2[3]]);

                // To get a vector of output(s), we multiply broadcasted vectors (of each
                // input plus the two previous outputs) and add them all together.
                // Incremental broadcasting and shifting is expected to be cheaper than
                // horizontal adds or transposing 4x4 values because they run on a different
                // port, concurrently with the FMA.
                let in0 = f32x4::from([sum.to_array()[0]; 4]);
                let mut out_1 = in0 * mul_in_1;
                let mut out_3 = in0 * mul_in_3;
                let mut out_5 = in0 * mul_in_5;

                let in1 = f32x4::from([sum.to_array()[1]; 4]);
                out_1 = shift_left_lanes::<1>(mul_in_1).mul_add(in1, out_1);
                out_3 = shift_left_lanes::<1>(mul_in_3).mul_add(in1, out_3);
                out_5 = shift_left_lanes::<1>(mul_in_5).mul_add(in1, out_5);

                let in2 = f32x4::from([sum.to_array()[2]; 4]);
                out_1 = shift_left_lanes::<2>(mul_in_1).mul_add(in2, out_1);
                out_3 = shift_left_lanes::<2>(mul_in_3).mul_add(in2, out_3);
                out_5 = shift_left_lanes::<2>(mul_in_5).mul_add(in2, out_5);

                let in3 = f32x4::from([sum.to_array()[3]; 4]);
                out_1 = shift_left_lanes::<3>(mul_in_1).mul_add(in3, out_1);
                out_3 = shift_left_lanes::<3>(mul_in_3).mul_add(in3, out_3);
                out_5 = shift_left_lanes::<3>(mul_in_5).mul_add(in3, out_5);

                out_1 = mul_prev2_1.mul_add(prev2_1, out_1);
                out_3 = mul_prev2_3.mul_add(prev2_3, out_3);
                out_5 = mul_prev2_5.mul_add(prev2_5, out_5);

                out_1 = mul_prev_1.mul_add(prev_1, out_1);
                out_3 = mul_prev_3.mul_add(prev_3, out_3);
                out_5 = mul_prev_5.mul_add(prev_5, out_5);

                prev2_1 = f32x4::from([out_1.to_array()[2]; 4]);
                prev2_3 = f32x4::from([out_3.to_array()[2]; 4]);
                prev2_5 = f32x4::from([out_5.to_array()[2]; 4]);
                prev_1 = f32x4::from([out_1.to_array()[3]; 4]);
                prev_3 = f32x4::from([out_3.to_array()[3]; 4]);
                prev_5 = f32x4::from([out_5.to_array()[3]; 4]);

                output[n as usize..][..4].copy_from_slice(&(out_1 + out_3 + out_5).to_array());

                n += 4;
            }

            // Remainder handling with bounds checks
            while n < width as isize {
                let left = n - self.radius as isize - 1;
                let right = n + self.radius as isize - 1;
                let left_val = if left >= 0 {
                    input[left as usize]
                } else {
                    0.0f32
                };
                let right_val = if right < width as isize {
                    input[right as usize]
                } else {
                    0.0f32
                };
                let sum = f32x4::from([left_val + right_val; 4]);

                // (Only processing a single lane here, no need to broadcast)
                let mut out_1 = sum * mul_in_1;
                let mut out_3 = sum * mul_in_3;
                let mut out_5 = sum * mul_in_5;

                out_1 = mul_prev2_1.mul_add(prev2_1, out_1);
                out_3 = mul_prev2_3.mul_add(prev2_3, out_3);
                out_5 = mul_prev2_5.mul_add(prev2_5, out_5);
                prev2_1 = prev_1;
                prev2_3 = prev_3;
                prev2_5 = prev_5;

                out_1 = mul_prev_1.mul_add(prev_1, out_1);
                out_3 = mul_prev_3.mul_add(prev_3, out_3);
                out_5 = mul_prev_5.mul_add(prev_5, out_5);
                prev_1 = out_1;
                prev_3 = out_3;
                prev_5 = out_5;

                output[n as usize] = (out_1 + out_3 + out_5).to_array()[0];

                n += 1;
            }
        }
    }

    // Apply 1D vertical scan to multiple columns (one per vector lane).
    pub fn fast_gaussian_vertical(
        &self,
        input: &[f32],
        output: &mut [f32],
        width: usize,
        height: usize,
    ) {
        assert_eq!(input.len(), output.len());

        let mut x = 0;
        while x + V_TOTAL_LANES <= width {
            self.vertical_strip::<V_CACHE_LINE_VECTORS>(input, x, output, width, height);
            x += V_TOTAL_LANES;
        }
        while x < width {
            self.vertical_strip::<1>(input, x, output, width, height);
            x += V_MAX_LANES;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn vertical_strip<const VECTORS: usize>(
        &self,
        input: &[f32],
        x: usize,
        output: &mut [f32],
        width: usize,
        height: usize,
    ) {
        // We're iterating vertically, so use multiple full-length vectors (each
        // lane is one column of row n).
        //
        // More cache-friendly to process an entirely cache line at a time
        let d1_1 = f32x4::from([self.d1[0], self.d1[1], self.d1[2], self.d1[3]]);
        let d1_3 = f32x4::from([self.d1[4], self.d1[5], self.d1[6], self.d1[7]]);
        let d1_5 = f32x4::from([self.d1[8], self.d1[9], self.d1[10], self.d1[11]]);
        let n2_1 = f32x4::from([self.n2[0], self.n2[1], self.n2[2], self.n2[3]]);
        let n2_3 = f32x4::from([self.n2[4], self.n2[5], self.n2[6], self.n2[7]]);
        let n2_5 = f32x4::from([self.n2[8], self.n2[9], self.n2[10], self.n2[11]]);

        let mut ctr = 0usize;
        let mut ring_buffer: Aligned<A16, _> = Aligned([0f32; 3 * V_TOTAL_LANES * V_MOD]);
        let zero: Aligned<A16, _> = Aligned([0f32; V_TOTAL_LANES]);

        // Warmup: top is out of bounds (zero padded), bottom is usually
        // in-bounds.
        let mut n = -(self.radius as isize) + 1;
        while n < 0 {
            // bottom is always non-negative since n is initialized in -N + 1.
            let bottom = n + self.radius as isize - 1;
            vertical_block::<VECTORS>(
                d1_1,
                d1_3,
                d1_5,
                n2_1,
                n2_3,
                n2_5,
                &VertBlockInput::SingleInput(if bottom < height as isize {
                    &input[(bottom as usize * width + x)..]
                } else {
                    zero.as_slice()
                }),
                &mut ctr,
                &mut ring_buffer,
                &mut VertBlockOutput::None,
            );
            n += 1;
        }

        // Start producing output; top is still out of bounds.
        while (n as usize) < (self.radius + 1).min(height) {
            let bottom = n + self.radius as isize - 1;
            vertical_block::<VECTORS>(
                d1_1,
                d1_3,
                d1_5,
                n2_1,
                n2_3,
                n2_5,
                &VertBlockInput::SingleInput(if bottom < height as isize {
                    &input[(bottom as usize * width + x)..]
                } else {
                    zero.as_slice()
                }),
                &mut ctr,
                &mut ring_buffer,
                &mut VertBlockOutput::Store(&mut output[(n as usize * width + x)..]),
            );
            n += 1;
        }

        // Interior outputs with prefetching and without bounds checks.
        while n < (height - self.radius + 1 - V_PREFETCH_ROWS) as isize {
            let top = n - self.radius as isize - 1;
            let bottom = n + self.radius as isize - 1;
            vertical_block::<VECTORS>(
                d1_1,
                d1_3,
                d1_5,
                n2_1,
                n2_3,
                n2_5,
                &VertBlockInput::TwoInputs((
                    &input[(top as usize * width + x)..],
                    &input[(bottom as usize * width + x)..],
                )),
                &mut ctr,
                &mut ring_buffer,
                &mut VertBlockOutput::Store(&mut output[(n as usize * width + x)..]),
            );
            // TODO: Use https://doc.rust-lang.org/std/intrinsics/fn.prefetch_read_data.html when stabilized
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                #[cfg(target_arch = "x86")]
                use core::arch::x86::{_mm_prefetch, _MM_HINT_T0};
                #[cfg(target_arch = "x86_64")]
                use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};

                // SAFETY: We checked the target arch before calling this
                unsafe {
                    _mm_prefetch(
                        input[((top as usize + V_PREFETCH_ROWS) * width + x)..]
                            .as_ptr()
                            .cast(),
                        _MM_HINT_T0,
                    );
                    _mm_prefetch(
                        input[((bottom as usize + V_PREFETCH_ROWS) * width + x)..]
                            .as_ptr()
                            .cast(),
                        _MM_HINT_T0,
                    );
                }
            }
            n += 1;
        }

        // Bottom border without prefetching and with bounds checks.
        while (n as usize) < height {
            let top = n - self.radius as isize - 1;
            let bottom = n + self.radius as isize - 1;
            vertical_block::<VECTORS>(
                d1_1,
                d1_3,
                d1_5,
                n2_1,
                n2_3,
                n2_5,
                &VertBlockInput::TwoInputs((
                    &input[(top as usize * width + x)..],
                    if (bottom as usize) < height {
                        &input[(bottom as usize * width + x)..]
                    } else {
                        zero.as_slice()
                    },
                )),
                &mut ctr,
                &mut ring_buffer,
                &mut VertBlockOutput::Store(&mut output[(n as usize * width + x)..]),
            );
            n += 1;
        }
    }
}

#[inline(always)]
fn round_up_to<T: PrimInt>(val: T, target: T) -> T {
    div_ceil(val, target) * target
}

#[inline(always)]
fn div_ceil<T: PrimInt>(a: T, b: T) -> T {
    (a + b - T::one()) / b
}

#[inline(always)]
fn shift_left_lanes<const LANES: usize>(data: f32x4) -> f32x4 {
    assert!(LANES <= 4);

    let mut output = [0f32; 4];
    output[..(4 - LANES)].copy_from_slice(&data.to_array()[LANES..]);
    f32x4::from(output)
}

// Block := `VECTORS` consecutive full vectors (one cache line except on the
// right boundary, where we can only rely on having one vector). Unrolling to
// the cache line size improves cache utilization.
#[allow(clippy::too_many_arguments)]
fn vertical_block<const VECTORS: usize>(
    d1_1: f32x4,
    d1_3: f32x4,
    d1_5: f32x4,
    n2_1: f32x4,
    n2_3: f32x4,
    n2_5: f32x4,
    input: &VertBlockInput,
    ctr: &mut usize,
    ring_buffer: &mut Aligned<A16, [f32; 3 * V_TOTAL_LANES * V_MOD]>,
    output: &mut VertBlockOutput,
) {
    let mut ring_chunks = ring_buffer.chunks_exact_mut(V_TOTAL_LANES * V_MOD);
    let y_1 = ring_chunks.next().expect("there are 3 chunks");
    let y_3 = ring_chunks.next().expect("there are 3 chunks");
    let y_5 = ring_chunks.next().expect("there are 3 chunks");

    *ctr += 1;
    let n_0 = *ctr % V_MOD;
    let n_1 = (*ctr - 1) % V_MOD;
    let n_2 = (*ctr - 2) % V_MOD;

    for idx_vec in 0..VECTORS {
        let sum = input.get(idx_vec * V_MAX_LANES);

        let y_n1_1 = &y_1[(V_TOTAL_LANES * n_1 + idx_vec * V_MAX_LANES)..];
        let y_n1_1 = f32x4::from([y_n1_1[0], y_n1_1[1], y_n1_1[2], y_n1_1[3]]);
        let y_n1_3 = &y_3[(V_TOTAL_LANES * n_1 + idx_vec * V_MAX_LANES)..];
        let y_n1_3 = f32x4::from([y_n1_3[0], y_n1_3[1], y_n1_3[2], y_n1_3[3]]);
        let y_n1_5 = &y_5[(V_TOTAL_LANES * n_1 + idx_vec * V_MAX_LANES)..];
        let y_n1_5 = f32x4::from([y_n1_5[0], y_n1_5[1], y_n1_5[2], y_n1_5[3]]);
        let y_n2_1 = &y_1[(V_TOTAL_LANES * n_2 + idx_vec * V_MAX_LANES)..];
        let y_n2_1 = f32x4::from([y_n2_1[0], y_n2_1[1], y_n2_1[2], y_n2_1[3]]);
        let y_n2_3 = &y_3[(V_TOTAL_LANES * n_2 + idx_vec * V_MAX_LANES)..];
        let y_n2_3 = f32x4::from([y_n2_3[0], y_n2_3[1], y_n2_3[2], y_n2_3[3]]);
        let y_n2_5 = &y_5[(V_TOTAL_LANES * n_2 + idx_vec * V_MAX_LANES)..];
        let y_n2_5 = f32x4::from([y_n2_5[0], y_n2_5[1], y_n2_5[2], y_n2_5[3]]);

        // (35)
        let y1 = n2_1.mul_add(sum, d1_1.mul_neg_sub(y_n1_1, y_n2_1));
        let y3 = n2_3.mul_add(sum, d1_3.mul_neg_sub(y_n1_3, y_n2_3));
        let y5 = n2_5.mul_add(sum, d1_5.mul_neg_sub(y_n1_5, y_n2_5));
        y_1[(V_TOTAL_LANES * n_0 + idx_vec * V_MAX_LANES)..][..4].copy_from_slice(&y1.to_array());
        y_3[(V_TOTAL_LANES * n_0 + idx_vec * V_MAX_LANES)..][..4].copy_from_slice(&y3.to_array());
        y_5[(V_TOTAL_LANES * n_0 + idx_vec * V_MAX_LANES)..][..4].copy_from_slice(&y5.to_array());
        output.write(y1 + y3 + y5, idx_vec * V_MAX_LANES);
    }
    // NOTE: flushing cache line out_pos hurts performance - less so with
    // clflushopt than clflush but still a significant slowdown.
}

enum VertBlockInput<'a> {
    SingleInput(&'a [f32]),
    TwoInputs((&'a [f32], &'a [f32])),
}

impl<'a> VertBlockInput<'a> {
    pub fn get(&self, index: usize) -> f32x4 {
        match *self {
            Self::SingleInput(input) => {
                let input = &input[index..][..4];
                f32x4::from([input[0], input[1], input[2], input[3]])
            }
            Self::TwoInputs((input1, input2)) => {
                let input1 = &input1[index..][..4];
                let input2 = &input2[index..][..4];
                let input1 = f32x4::from([input1[0], input1[1], input1[2], input1[3]]);
                let input2 = f32x4::from([input2[0], input2[1], input2[2], input2[3]]);
                input1 + input2
            }
        }
    }
}

enum VertBlockOutput<'a> {
    None,
    Store(&'a mut [f32]),
}

impl<'a> VertBlockOutput<'a> {
    pub fn write(&mut self, data: f32x4, index: usize) {
        match *self {
            Self::None => (),
            Self::Store(ref mut output) => {
                output[index..][..4].copy_from_slice(&data.to_array());
            }
        }
    }
}
