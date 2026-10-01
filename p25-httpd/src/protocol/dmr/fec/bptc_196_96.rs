//! BPTC(196,96) for data bursts: port of SDRTrunk `module/decode/dmr/bptc/BPTC_196_96.java`
//! and the payload extraction in `message/data/DMRDataMessageFactory.extract()`.
//!
//! Deinterleaved, bit 0 is the unused R(3); bit `row * 15 + column + 1` is a
//! 13 x 15 matrix. Rows 0..9 hold 11 bits plus Hamming(15,11) parity (row 0
//! starts with reserved R(2..0), which are dropped here); rows 9..13 are the
//! Hamming(13,9) parity of each column.
//!
//! `correct()` is SDRTrunk's turbo search (row/column paths, shadow flipping).
//! Changes:
//! - `correctRow()`'s pursuit tested `!solution2.contains(index)` on a path
//!   that always holds `index`, so it never took a found solution, then popped
//!   the wrong path entry and left the sub-solution's flips behind. It now
//!   takes the solution, as `correctColumn()` does.
//! - SDRTrunk trusts its row/column flags at the end; this port re-checks every
//!   row and column, so `Some` is always a valid BPTC codeword, and reports the
//!   exact number of flipped bits (capped at `MAX_CORRECTED`).
//! - The search is exponential on noise (millions of steps on random input), so
//!   each pass stops after `MAX_SEARCH_STEPS`; good blocks need a few hundred.
//! - The "easy bits" step flips where a 2-error row and a 2-error column both
//!   mis-point; the search then lands 5+ bits away on a wrong block. A result
//!   needing over 4 flips is retried without that step and the closer one kept.
//!
//! The search is not maximum-likelihood: about 1 in 2000 patterns of 3 or 4
//! errors still fails or (rarer) decodes wrong; the CRC catches the latter.

use super::bit_distance;
use super::hamming::{ErrorIndex, Hamming13, Hamming15};

pub const BPTC_LENGTH: usize = 196;
const MAX_ORIGINAL_INDEX: usize = 136;
const COLUMN_COUNT: usize = 15;
const ROW_COUNT: usize = 13;
/// Should be 11, but adjusted for the first pad bit.
const MESSAGE_COLUMN_COUNT: usize = 12;
const CHECKSUM_COLUMN_COUNT: usize = 4;
const MESSAGE_START_INDEX: usize = 4;
const MAXIMUM_RECURSION_TURBO_DEPTH: usize = 20;
/// Most bit flips a decode may make. SDRTrunk has no limit; legitimate decodes
/// stay well under it.
pub const MAX_CORRECTED: u32 = 12;
/// Most `correct_column()` / `correct_row()` calls per correction pass.
pub const MAX_SEARCH_STEPS: usize = 2000;

/// Deinterleaved bit `x` is transmitted bit `x * 181 mod 196` (`BPTC_DEINTERLEAVE`).
pub const BPTC_DEINTERLEAVE: [usize; BPTC_LENGTH] = build_deinterleave();
/// Message indexes of the 13 bits of each column (`COLUMN_INDEXES`).
pub const COLUMN_INDEXES: [[usize; ROW_COUNT]; COLUMN_COUNT] = build_column_indexes();

const fn build_deinterleave() -> [usize; BPTC_LENGTH] {
    let mut table = [0usize; BPTC_LENGTH];
    let mut x = 0;
    while x < BPTC_LENGTH {
        table[x] = (x * 181) % BPTC_LENGTH;
        x += 1;
    }
    table
}

const fn build_column_indexes() -> [[usize; ROW_COUNT]; COLUMN_COUNT] {
    let mut table = [[0usize; ROW_COUNT]; COLUMN_COUNT];
    let mut column = 0;
    while column < COLUMN_COUNT {
        let mut row = 0;
        while row < ROW_COUNT {
            table[column][row] = get_index(column, row);
            row += 1;
        }
        column += 1;
    }
    table
}

/// The 196 BPTC bits of a 288-bit data burst: bits 24..122 and 190..288 (98 + 98).
/// Ports `DMRDataMessageFactory.extract()`.
pub fn payload_from_burst(burst: &[u8]) -> [u8; BPTC_LENGTH] {
    let mut extracted = [0u8; BPTC_LENGTH];
    extracted[..98].copy_from_slice(&burst[24..122]);
    extracted[98..].copy_from_slice(&burst[190..288]);
    extracted
}

/// Deinterleaves the transmitted 196 bits. Ports `BPTC_196_96.deinterleave()`.
pub fn deinterleave(message: &[u8]) -> [u8; BPTC_LENGTH] {
    let mut deinterleaved = [0u8; BPTC_LENGTH];
    for (x, bit) in deinterleaved.iter_mut().enumerate() {
        *bit = message[BPTC_DEINTERLEAVE[x]];
    }
    deinterleaved
}

/// Deinterleaves, corrects and extracts the 96 info bits plus the corrected bit
/// count; `None` if uncorrectable. Ports `BPTC_196_96.extract()`.
pub fn decode(interleaved: &[u8]) -> Option<([u8; 96], u32)> {
    decode_with_budget(interleaved, MAX_SEARCH_STEPS).0
}

/// `decode()` with a search step budget per pass; also returns the steps used.
fn decode_with_budget(interleaved: &[u8], steps: usize) -> (Option<([u8; 96], u32)>, usize) {
    let received = deinterleave(interleaved);
    let mut used = 0;
    let mut best: Option<([u8; BPTC_LENGTH], u32)> = None;

    // SDRTrunk's pass, then one without the "easy bits" step unless the first
    // found a codeword within 4 bits (d = 9: then it is the nearest one).
    for easy_bits in [true, false] {
        if matches!(best, Some((_, n)) if n <= 4) {
            break;
        }
        let mut message = received;
        let mut budget = Budget(steps);
        let valid = correct(&mut message, true, easy_bits, &mut budget) && is_correct(&message);
        used += steps - budget.0;
        if valid {
            let corrected = bit_distance(&received, &message);
            if best.map_or(true, |(_, n)| corrected < n) {
                best = Some((message, corrected));
            }
        }
    }

    let (message, corrected) = match best {
        Some((message, corrected)) if corrected <= MAX_CORRECTED => (message, corrected),
        _ => return (None, used),
    };

    let mut extracted = [0u8; 96];
    let mut pointer = 0;
    let mut index = MESSAGE_START_INDEX;
    while index < MAX_ORIGINAL_INDEX {
        if index % COLUMN_COUNT < MESSAGE_COLUMN_COUNT {
            extracted[pointer] = message[index];
            pointer += 1;
            index += 1;
        } else {
            index += CHECKSUM_COLUMN_COUNT;
        }
    }
    (Some((extracted, corrected)), used)
}

/// Remaining search steps for one decode.
struct Budget(usize);

impl Budget {
    fn spend(&mut self) -> bool {
        if self.0 == 0 {
            return false;
        }
        self.0 -= 1;
        true
    }
}

/// Every row and column passes its Hamming check.
fn is_correct(message: &[u8]) -> bool {
    (0..ROW_COUNT).all(|row| is_row_correct(row, message))
        && (0..COLUMN_COUNT).all(|column| is_column_correct(column, message))
}

/// Row Hamming(15,11) error indexes. Ports `getRowErrors()`.
fn get_row_errors(message: &[u8]) -> Vec<usize> {
    (0..ROW_COUNT)
        .filter_map(|row| match get_row_error_index(row, message) {
            ErrorIndex::At(i) => Some(i),
            _ => None,
        })
        .collect()
}

/// Column Hamming(13,9) error indexes. Ports `getColumnErrors()`.
fn get_column_errors(message: &[u8]) -> Vec<usize> {
    let mut errors: Vec<usize> = (0..COLUMN_COUNT)
        .filter_map(|column| match get_column_error_index(column, message) {
            ErrorIndex::At(i) => Some(i),
            _ => None,
        })
        .collect();
    errors.sort_unstable();
    errors
}

/// Message indexes at the crossings of the flagged rows and columns. Ports `getIntersectionIndices()`.
fn get_intersection_indices(
    columns: &[bool; COLUMN_COUNT],
    rows: &[bool; ROW_COUNT],
) -> Vec<usize> {
    let mut intersections = Vec::new();
    for row in (0..ROW_COUNT).filter(|&r| rows[r]) {
        for column in (0..COLUMN_COUNT).filter(|&c| columns[c]) {
            intersections.push(get_index(column, row));
        }
    }
    intersections
}

fn any(flags: &[bool]) -> bool {
    flags.iter().any(|&f| f)
}

fn count(flags: &[bool]) -> usize {
    flags.iter().filter(|&&f| f).count()
}

/// Turbo decode: follows row/column Hamming paths, then flips 2x2..3x3
/// shadow intersections once. Ports `BPTC_196_96.correct()`.
fn correct(
    message: &mut [u8; BPTC_LENGTH],
    pursue_shadows: bool,
    easy_bits: bool,
    budget: &mut Budget,
) -> bool {
    let column_errors = get_column_errors(message);
    let row_errors = get_row_errors(message);

    if column_errors.is_empty() && row_errors.is_empty() {
        return true;
    }

    // Fix the easy bits first: where the row and column codes agree.
    if easy_bits {
        for &i in column_errors.iter().filter(|i| row_errors.contains(i)) {
            message[i] ^= 1;
        }
    }

    let mut columns = [false; COLUMN_COUNT];
    for (column, flag) in columns.iter_mut().enumerate() {
        *flag = !is_column_correct(column, message);
    }
    let mut rows = [false; ROW_COUNT];
    for (row, flag) in rows.iter_mut().enumerate() {
        *flag = !is_row_correct(row, message);
    }

    if !any(&columns) && !any(&rows) {
        return true;
    }

    // First up to 2-bit errors per column or row on single paths, then pursuing
    // multi-column and multi-row paths.
    for pursue in [false, true] {
        if pursue && !any(&columns) && !any(&rows) {
            break;
        }
        for column in 0..COLUMN_COUNT {
            if columns[column] {
                correct_column(
                    column,
                    message,
                    &mut columns,
                    &mut rows,
                    0,
                    pursue,
                    &mut Vec::new(),
                    budget,
                );
            }
        }
        if any(&columns) || any(&rows) {
            for row in 0..ROW_COUNT {
                if rows[row] {
                    correct_row(
                        row,
                        message,
                        &mut columns,
                        &mut rows,
                        0,
                        pursue,
                        &mut Vec::new(),
                        budget,
                    );
                }
            }
        }
    }

    // Double/triple row and column errors shadowing each other: flip all the
    // intersections and try once more (not recursively).
    let shadow = |n: usize| n == 2 || n == 3;
    if pursue_shadows && shadow(count(&columns)) && shadow(count(&rows)) {
        let intersections = get_intersection_indices(&columns, &rows);
        for &i in &intersections {
            message[i] ^= 1;
        }

        if correct(message, false, easy_bits, budget) {
            columns = [false; COLUMN_COUNT];
            rows = [false; ROW_COUNT];
        } else {
            for &i in &intersections {
                message[i] ^= 1;
            }
        }
    }

    !any(&columns) && !any(&rows)
}

/// Flips the column's Hamming error bit if its row is flagged, then follows
/// the row (and, when pursuing, later columns). Ports `correctColumn()`.
fn correct_column(
    column: usize,
    message: &mut [u8; BPTC_LENGTH],
    columns: &mut [bool; COLUMN_COUNT],
    rows: &mut [bool; ROW_COUNT],
    depth: usize,
    pursue: bool,
    path: &mut Vec<usize>,
    budget: &mut Budget,
) -> bool {
    if depth > MAXIMUM_RECURSION_TURBO_DEPTH || !budget.spend() {
        return false;
    }

    let index = match get_column_error_index(column, message) {
        ErrorIndex::At(index) => index,
        _ => return false,
    };
    let row = get_row(index);

    if rows[row] && !path.contains(&index) {
        message[index] ^= 1;

        if is_column_correct(column, message) {
            path.push(index);

            if is_row_correct(row, message) {
                columns[column] = false;
                rows[row] = false;
                return true;
            }

            if correct_row(row, message, columns, rows, depth + 1, pursue, path, budget) {
                columns[column] = false;
                return true;
            } else if pursue {
                for column2 in column + 1..COLUMN_COUNT {
                    if columns[column2]
                        && correct_column(
                            column2,
                            message,
                            columns,
                            rows,
                            depth + 1,
                            pursue,
                            path,
                            budget,
                        )
                    {
                        columns[column] = false;
                        rows[row] = false;
                        return true;
                    }
                }
            }

            path.pop();
        }

        message[index] ^= 1;
    }

    false
}

/// Flips the row's Hamming error bit if its column is flagged, then follows
/// the column (and, when pursuing, later rows). Ports `correctRow()`.
fn correct_row(
    row: usize,
    message: &mut [u8; BPTC_LENGTH],
    columns: &mut [bool; COLUMN_COUNT],
    rows: &mut [bool; ROW_COUNT],
    depth: usize,
    pursue: bool,
    path: &mut Vec<usize>,
    budget: &mut Budget,
) -> bool {
    if depth > MAXIMUM_RECURSION_TURBO_DEPTH || !budget.spend() {
        return false;
    }

    let index = match get_row_error_index(row, message) {
        ErrorIndex::At(index) => index,
        _ => return false,
    };
    // Bit 0 (R(3)) is in no row, but a row index is always 1..=195.
    let column = get_column(index);

    if columns[column] && !path.contains(&index) {
        message[index] ^= 1;

        if is_row_correct(row, message) {
            path.push(index);

            if is_column_correct(column, message) {
                columns[column] = false;
                rows[row] = false;
                return true;
            }

            if correct_column(
                column,
                message,
                columns,
                rows,
                depth + 1,
                pursue,
                path,
                budget,
            ) {
                rows[row] = false;
                return true;
            } else if pursue {
                for row2 in row + 1..ROW_COUNT {
                    if rows[row2]
                        && correct_row(
                            row2,
                            message,
                            columns,
                            rows,
                            depth + 1,
                            pursue,
                            path,
                            budget,
                        )
                    {
                        columns[column] = false;
                        rows[row] = false;
                        return true;
                    }
                }
            }

            path.pop();
        }

        message[index] ^= 1;
    }

    false
}

const fn get_index(column: usize, row: usize) -> usize {
    row * COLUMN_COUNT + column + 1
}

fn get_column(index: usize) -> usize {
    (index - 1) % COLUMN_COUNT
}

fn get_row(index: usize) -> usize {
    (index - 1) / COLUMN_COUNT
}

fn is_row_correct(row: usize, message: &[u8]) -> bool {
    Hamming15::get_syndrome(message, row * COLUMN_COUNT + 1) == 0
}

fn get_row_error_index(row: usize, message: &[u8]) -> ErrorIndex {
    Hamming15::get_error_index(message, row * COLUMN_COUNT + 1)
}

fn is_column_correct(column: usize, message: &[u8]) -> bool {
    Hamming13::get_syndrome(message, &COLUMN_INDEXES[column]) == 0
}

fn get_column_error_index(column: usize, message: &[u8]) -> ErrorIndex {
    Hamming13::get_error_index(message, &COLUMN_INDEXES[column])
}

#[cfg(test)]
#[path = "bptc_196_96_tests.rs"]
mod tests;
