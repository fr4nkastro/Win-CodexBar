/// Return a checked total for the three signed token lanes.
///
/// Sorting before the additions lets opposite-sign extremes cancel before a
/// final addition while still rejecting a genuinely unrepresentable result.
pub(super) fn checked_total(input: i64, cached: i64, output: i64) -> Option<i64> {
    let mut lanes = [input, cached, output];
    lanes.sort_unstable();
    lanes[0].checked_add(lanes[2])?.checked_add(lanes[1])
}

#[cfg(test)]
mod tests {
    use super::checked_total;

    #[test]
    fn signed_extremes_cancel_independently_of_lane_order() {
        let cases = [
            ([i64::MAX, 1, -1], i64::MAX),
            ([i64::MIN, -1, 1], i64::MIN),
            ([i64::MAX, i64::MIN, 1], 0),
            ([i64::MAX, i64::MAX, i64::MIN], i64::MAX - 1),
        ];
        for (values, expected) in cases {
            for order in [
                [0, 1, 2],
                [0, 2, 1],
                [1, 0, 2],
                [1, 2, 0],
                [2, 0, 1],
                [2, 1, 0],
            ] {
                assert_eq!(
                    checked_total(values[order[0]], values[order[1]], values[order[2]]),
                    Some(expected)
                );
            }
        }
    }

    #[test]
    fn rejects_unrepresentable_final_totals() {
        for values in [
            [i64::MAX, 1, 0],
            [i64::MIN, -1, 0],
            [i64::MAX, i64::MAX, 1],
            [i64::MIN, i64::MIN, -1],
        ] {
            assert_eq!(checked_total(values[0], values[1], values[2]), None);
        }
    }
}
