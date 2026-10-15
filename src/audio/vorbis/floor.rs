//! Vorbis floor type 1: a piecewise-linear spectral envelope in the log (dB) domain.

use super::bits::{bit_length, BitReader};
use super::codebook::Codebook;
use anyhow::{anyhow, Result};

/// Floor amplitude range for each multiplier setting.
const RANGES: [i32; 4] = [256, 128, 86, 64];

/// A Vorbis floor 1 configuration.
pub struct Floor {
    /// Partition class for each partition.
    partition_classes: Vec<usize>,
    /// Number of X values each class contributes.
    class_dimensions: Vec<usize>,
    /// log2 of the number of subclasses in each class.
    class_subclass_bits: Vec<u32>,
    /// Codebook used to choose the subclass for each class.
    class_master_books: Vec<usize>,
    /// Codebook for each subclass, or -1 for none.
    subclass_books: Vec<Vec<i32>>,
    /// Y value multiplier (1-4).
    multiplier: i32,
    /// X position of each floor point, before sorting.
    x_list: Vec<i32>,
}

/// Per-packet floor point values, produced by [`Floor::decode_packet`] and consumed by
/// [`Floor::synthesize`].
pub struct FloorPoints {
    x: Vec<i32>,
    y: Vec<i32>,
    used: Vec<bool>,
}

impl Floor {
    /// Decodes a floor configuration from the setup header.
    pub fn decode(bits: &mut BitReader) -> Result<Self> {
        let floor_type = bits.read_bits(16);
        if floor_type != 1 {
            return Err(anyhow!("Unsupported Vorbis floor type {}", floor_type));
        }

        let partitions = bits.read_bits(5) as usize;
        let mut class_count = 0;
        let mut partition_classes = Vec::with_capacity(partitions);
        for _ in 0..partitions {
            let class = bits.read_bits(4) as usize;
            partition_classes.push(class);
            if class >= class_count {
                class_count = class + 1;
            }
        }

        let mut class_dimensions = vec![0; class_count];
        let mut class_subclass_bits = vec![0; class_count];
        let mut class_master_books = vec![0; class_count];
        let mut subclass_books = vec![Vec::new(); class_count];
        for class in 0..class_count {
            class_dimensions[class] = bits.read_bits(3) as usize + 1;
            let subclass_bits = bits.read_bits(2);
            class_subclass_bits[class] = subclass_bits;
            if subclass_bits != 0 {
                class_master_books[class] = bits.read_bits(8) as usize;
            }
            subclass_books[class] = (0..1 << subclass_bits)
                .map(|_| bits.read_bits(8) as i32 - 1)
                .collect();
        }

        let multiplier = bits.read_bits(2) as i32 + 1;
        let range_bits = bits.read_bits(4);
        let mut x_list = vec![0, 1 << range_bits];
        for &class in &partition_classes {
            for _ in 0..class_dimensions[class] {
                x_list.push(bits.read_bits(range_bits) as i32);
            }
        }

        Ok(Floor {
            partition_classes,
            class_dimensions,
            class_subclass_bits,
            class_master_books,
            subclass_books,
            multiplier,
            x_list,
        })
    }

    /// Decodes the floor's points for one packet, or `None` if the floor is unused (the packet
    /// is silent).
    pub fn decode_packet(
        &self,
        bits: &mut BitReader,
        codebooks: &[Codebook],
    ) -> Option<FloorPoints> {
        if !bits.read_flag() {
            return None;
        }
        let count = self.x_list.len();
        let mut y = vec![0i32; count];
        let range = RANGES[(self.multiplier - 1) as usize];
        let y_bits = bit_length(range - 1);
        y[0] = bits.read_bits(y_bits) as i32;
        y[1] = bits.read_bits(y_bits) as i32;

        let mut offset = 2;
        for &class in &self.partition_classes {
            let dimensions = self.class_dimensions[class];
            let subclass_bits = self.class_subclass_bits[class];
            let subclass_mask = (1u32 << subclass_bits) - 1;
            let mut class_value = 0u32;
            if subclass_bits > 0 {
                class_value = codebooks[self.class_master_books[class]].decode_scalar(bits) as u32;
            }
            for _ in 0..dimensions {
                let book = self.subclass_books[class][(class_value & subclass_mask) as usize];
                class_value >>= subclass_bits;
                y[offset] = if book >= 0 {
                    codebooks[book as usize].decode_scalar(bits) as i32
                } else {
                    0
                };
                offset += 1;
            }
        }

        Some(FloorPoints {
            x: self.x_list.clone(),
            y,
            used: vec![false; count],
        })
    }

    /// Multiplies the first `n` entries of `vector` (the residue spectrum) by the floor curve.
    pub fn synthesize(&self, points: &mut FloorPoints, vector: &mut [f32], n: usize) {
        let count = self.x_list.len();
        let range = RANGES[(self.multiplier - 1) as usize];
        let FloorPoints { x, y, used } = points;

        // Step 1: amplitude value synthesis. Each point's value is a delta from the line between
        // its already-decoded low and high neighbours.
        used[0] = true;
        used[1] = true;
        for i in 2..count {
            let low = low_neighbour(x, i);
            let high = high_neighbour(x, i);
            let predicted = render_point(x[low], y[low], x[high], y[high], x[i]);
            let value = y[i];
            let high_room = range - predicted;
            let room = high_room.min(predicted) << 1;
            if value == 0 {
                used[i] = false;
                y[i] = predicted;
            } else {
                used[low] = true;
                used[high] = true;
                used[i] = true;
                if value >= room {
                    y[i] = if high_room > predicted {
                        value
                    } else {
                        predicted - value + high_room - 1
                    };
                } else {
                    y[i] = if value & 1 == 0 {
                        predicted + value / 2
                    } else {
                        predicted - (value + 1) / 2
                    };
                }
            }
        }

        // Step 2: render line segments between used points, in X order.
        sort_points(x, y, used, 0, count as i32 - 1);
        let n = n as i32;
        let mut low_x = 0;
        let mut low_y = y[0] * self.multiplier;
        for i in 1..count {
            if used[i] {
                let high_x = x[i];
                let high_y = y[i] * self.multiplier;
                render_line(low_x, low_y, high_x, high_y, vector, n);
                if high_x >= n {
                    return;
                }
                low_x = high_x;
                low_y = high_y;
            }
        }
        let factor = INVERSE_DB_TABLE[low_y as usize];
        for value in vector.iter_mut().take(n as usize).skip(low_x as usize) {
            *value *= factor;
        }
    }
}

/// Index of the point before `index` with the greatest X less than `x[index]`.
fn low_neighbour(x: &[i32], index: usize) -> usize {
    let target = x[index];
    let mut result = usize::MAX;
    let mut best = i32::MIN;
    for (i, &value) in x.iter().enumerate().take(index) {
        if value < target && value > best {
            result = i;
            best = value;
        }
    }
    result
}

/// Index of the point before `index` with the smallest X greater than `x[index]`.
fn high_neighbour(x: &[i32], index: usize) -> usize {
    let target = x[index];
    let mut result = usize::MAX;
    let mut best = i32::MAX;
    for (i, &value) in x.iter().enumerate().take(index) {
        if value > target && value < best {
            result = i;
            best = value;
        }
    }
    result
}

/// Y value at `x` on the line from (x0, y0) to (x1, y1), with integer rounding toward y0.
fn render_point(x0: i32, y0: i32, x1: i32, y1: i32, x: i32) -> i32 {
    let dy = y1 - y0;
    let adx = x1 - x0;
    let ady = dy.abs();
    let offset = ady * (x - x0) / adx;
    if dy < 0 {
        y0 - offset
    } else {
        y0 + offset
    }
}

/// Multiplies `vector[x0..min(x1, n)]` by the dB curve along the integer line (Bresenham-style)
/// from (x0, y0) to (x1, y1).
fn render_line(x0: i32, y0: i32, x1: i32, y1: i32, vector: &mut [f32], n: i32) {
    let dy = y1 - y0;
    let adx = x1 - x0;
    let mut ady = dy.abs();
    let base = dy / adx;
    let mut y = y0;
    let mut error = 0;
    let step = if dy < 0 { base - 1 } else { base + 1 };
    ady -= base.abs() * adx;
    vector[x0 as usize] *= INVERSE_DB_TABLE[y0 as usize];
    let end = x1.min(n);
    for x in x0 + 1..end {
        error += ady;
        if error >= adx {
            error -= adx;
            y += step;
        } else {
            y += base;
        }
        vector[x as usize] *= INVERSE_DB_TABLE[y as usize];
    }
}

/// Sorts the points by X (keeping Y and the used flags with them) using a quicksort.
fn sort_points(x: &mut [i32], y: &mut [i32], used: &mut [bool], low: i32, high: i32) {
    if low >= high {
        return;
    }
    let mut pivot_index = low;
    let pivot_x = x[low as usize];
    let pivot_y = y[low as usize];
    let pivot_used = used[low as usize];
    for i in low + 1..=high {
        let (i, p) = (i as usize, pivot_index as usize);
        if x[i] < pivot_x {
            x[p] = x[i];
            y[p] = y[i];
            used[p] = used[i];
            pivot_index += 1;
            let p = pivot_index as usize;
            x[i] = x[p];
            y[i] = y[p];
            used[i] = used[p];
        }
    }
    let p = pivot_index as usize;
    x[p] = pivot_x;
    y[p] = pivot_y;
    used[p] = pivot_used;
    sort_points(x, y, used, low, pivot_index - 1);
    sort_points(x, y, used, pivot_index + 1, high);
}

/// dB-to-linear lookup table (floor1_inverse_dB_table in the Vorbis specification).
#[rustfmt::skip]
static INVERSE_DB_TABLE: [f32; 256] = [
    1.0649863E-7f32, 1.1341951E-7f32, 1.2079015E-7f32, 1.2863978E-7f32, 1.369995E-7f32, 1.459025E-7f32, 1.5538409E-7f32, 1.6548181E-7f32,
    1.7623574E-7f32, 1.8768856E-7f32, 1.998856E-7f32, 2.128753E-7f32, 2.2670913E-7f32, 2.4144197E-7f32, 2.5713223E-7f32, 2.7384212E-7f32,
    2.9163792E-7f32, 3.1059022E-7f32, 3.307741E-7f32, 3.5226967E-7f32, 3.7516213E-7f32, 3.995423E-7f32, 4.255068E-7f32, 4.5315863E-7f32,
    4.8260745E-7f32, 5.1397E-7f32, 5.4737063E-7f32, 5.829419E-7f32, 6.208247E-7f32, 6.611694E-7f32, 7.041359E-7f32, 7.4989464E-7f32,
    7.98627E-7f32, 8.505263E-7f32, 9.057983E-7f32, 9.646621E-7f32, 1.0273513E-6f32, 1.0941144E-6f32, 1.1652161E-6f32, 1.2409384E-6f32,
    1.3215816E-6f32, 1.4074654E-6f32, 1.4989305E-6f32, 1.5963394E-6f32, 1.7000785E-6f32, 1.8105592E-6f32, 1.9282195E-6f32, 2.053526E-6f32,
    2.1869757E-6f32, 2.3290977E-6f32, 2.4804558E-6f32, 2.6416496E-6f32, 2.813319E-6f32, 2.9961443E-6f32, 3.1908505E-6f32, 3.39821E-6f32,
    3.619045E-6f32, 3.8542307E-6f32, 4.1047006E-6f32, 4.371447E-6f32, 4.6555283E-6f32, 4.958071E-6f32, 5.280274E-6f32, 5.623416E-6f32,
    5.988857E-6f32, 6.3780467E-6f32, 6.7925284E-6f32, 7.2339453E-6f32, 7.704048E-6f32, 8.2047E-6f32, 8.737888E-6f32, 9.305725E-6f32,
    9.910464E-6f32, 1.0554501E-5f32, 1.1240392E-5f32, 1.1970856E-5f32, 1.2748789E-5f32, 1.3577278E-5f32, 1.4459606E-5f32, 1.5399271E-5f32,
    1.6400005E-5f32, 1.7465769E-5f32, 1.8600793E-5f32, 1.9809577E-5f32, 2.1096914E-5f32, 2.2467912E-5f32, 2.3928002E-5f32, 2.5482977E-5f32,
    2.7139005E-5f32, 2.890265E-5f32, 3.078091E-5f32, 3.2781227E-5f32, 3.4911533E-5f32, 3.718028E-5f32, 3.9596467E-5f32, 4.2169668E-5f32,
    4.491009E-5f32, 4.7828602E-5f32, 5.0936775E-5f32, 5.424693E-5f32, 5.7772202E-5f32, 6.152657E-5f32, 6.552491E-5f32, 6.9783084E-5f32,
    7.4317984E-5f32, 7.914758E-5f32, 8.429104E-5f32, 8.976875E-5f32, 9.560242E-5f32, 1.0181521E-4f32, 1.0843174E-4f32, 1.1547824E-4f32,
    1.2298267E-4f32, 1.3097477E-4f32, 1.3948625E-4f32, 1.4855085E-4f32, 1.5820454E-4f32, 1.6848555E-4f32, 1.7943469E-4f32, 1.9109536E-4f32,
    2.0351382E-4f32, 2.167393E-4f32, 2.3082423E-4f32, 2.4582449E-4f32, 2.6179955E-4f32, 2.7881275E-4f32, 2.9693157E-4f32, 3.1622787E-4f32,
    3.3677815E-4f32, 3.5866388E-4f32, 3.8197188E-4f32, 4.0679457E-4f32, 4.3323037E-4f32, 4.613841E-4f32, 4.913675E-4f32, 5.2329927E-4f32,
    5.573062E-4f32, 5.935231E-4f32, 6.320936E-4f32, 6.731706E-4f32, 7.16917E-4f32, 7.635063E-4f32, 8.1312325E-4f32, 8.6596457E-4f32,
    9.2223985E-4f32, 9.821722E-4f32, 0.0010459992f32, 0.0011139743f32, 0.0011863665f32, 0.0012634633f32, 0.0013455702f32, 0.0014330129f32,
    0.0015261382f32, 0.0016253153f32, 0.0017309374f32, 0.0018434235f32, 0.0019632196f32, 0.0020908006f32, 0.0022266726f32, 0.0023713743f32,
    0.0025254795f32, 0.0026895993f32, 0.0028643848f32, 0.0030505287f32, 0.003248769f32, 0.0034598925f32, 0.0036847359f32, 0.0039241905f32,
    0.0041792067f32, 0.004450795f32, 0.004740033f32, 0.005048067f32, 0.0053761187f32, 0.005725489f32, 0.0060975635f32, 0.0064938175f32,
    0.0069158226f32, 0.0073652514f32, 0.007843887f32, 0.008353627f32, 0.008896492f32, 0.009474637f32, 0.010090352f32, 0.01074608f32,
    0.011444421f32, 0.012188144f32, 0.012980198f32, 0.013823725f32, 0.014722068f32, 0.015678791f32, 0.016697686f32, 0.017782796f32,
    0.018938422f32, 0.020169148f32, 0.021479854f32, 0.022875736f32, 0.02436233f32, 0.025945531f32, 0.027631618f32, 0.029427277f32,
    0.031339627f32, 0.03337625f32, 0.035545226f32, 0.037855156f32, 0.0403152f32, 0.042935107f32, 0.045725275f32, 0.048696756f32,
    0.05186135f32, 0.05523159f32, 0.05882085f32, 0.062643364f32, 0.06671428f32, 0.07104975f32, 0.075666964f32, 0.08058423f32,
    0.08582105f32, 0.09139818f32, 0.097337745f32, 0.1036633f32, 0.11039993f32, 0.11757434f32, 0.12521498f32, 0.13335215f32,
    0.14201812f32, 0.15124726f32, 0.16107617f32, 0.1715438f32, 0.18269168f32, 0.19456401f32, 0.20720787f32, 0.22067343f32,
    0.23501402f32, 0.25028655f32, 0.26655158f32, 0.28387362f32, 0.3023213f32, 0.32196787f32, 0.34289113f32, 0.36517414f32,
    0.3889052f32, 0.41417846f32, 0.44109413f32, 0.4697589f32, 0.50028646f32, 0.53279793f32, 0.5674221f32, 0.6042964f32,
    0.64356697f32, 0.6853896f32, 0.72993004f32, 0.777365f32, 0.8278826f32, 0.88168305f32, 0.9389798f32, 1.0f32,
];
