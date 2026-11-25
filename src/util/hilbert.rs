pub(crate) fn hilbert_key(lon_e7: i32, lat_e7: i32) -> u64 {
    let x = (i64::from(lon_e7) + 1_800_000_000).clamp(0, u32::MAX as i64) as u64;
    let y = ((i64::from(lat_e7) + 900_000_000) * 2).clamp(0, u32::MAX as i64) as u64;
    hilbert_index(x, y, 32)
}

fn hilbert_index(mut x: u64, mut y: u64, bits: u32) -> u64 {
    let n = 1u64 << bits;
    let mut index = 0u64;
    let mut s = n >> 1;
    while s > 0 {
        let rx = u64::from(x & s != 0);
        let ry = u64::from(y & s != 0);
        index += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_visits_every_cell_once_through_adjacent_steps() {
        let bits = 4;
        let side = 1u64 << bits;
        let mut cells = Vec::new();
        for x in 0..side {
            for y in 0..side {
                cells.push((hilbert_index(x, y, bits), x, y));
            }
        }
        cells.sort_unstable();
        for (expected, (index, _, _)) in cells.iter().enumerate() {
            assert_eq!(*index, expected as u64);
        }
        for pair in cells.windows(2) {
            let (_, x0, y0) = pair[0];
            let (_, x1, y1) = pair[1];
            assert_eq!(x0.abs_diff(x1) + y0.abs_diff(y1), 1);
        }
    }

    #[test]
    fn nearby_points_share_long_key_prefixes() {
        let toronto = hilbert_key(-793_832_000, 436_532_000);
        let next_door = hilbert_key(-793_832_100, 436_532_100);
        let sydney = hilbert_key(1_512_093_000, -338_688_000);
        assert!(toronto.abs_diff(next_door) < toronto.abs_diff(sydney));
        assert_eq!(
            hilbert_key(i32::MIN, i32::MIN),
            hilbert_key(-1_800_000_000, -900_000_000)
        );
    }
}
