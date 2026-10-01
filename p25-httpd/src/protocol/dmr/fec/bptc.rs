//! Block product turbo codes with Hamming rows and one even-parity row: ports
//! of SDRTrunk `module/decode/dmr/bptc/BPTCBase.java`, `BPTC_68_36.java`
//! (short LC from 4 CACH fragments) and `BPTC_128_77.java` (embedded LC from
//! the 32-bit fragments of voice bursts B..E).
//!
//! `correct()` follows SDRTrunk's steps, with three changes:
//! - SDRTrunk trusts its row/column flags at the end; this port re-checks every
//!   row and column, so `Some` is always a valid BPTC codeword.
//! - If the steps fail, they are retried without step 1. Hamming(17,12) turns
//!   two errors in one row into a wrong third flip, which step 2 would have
//!   fixed from the column parity (SDRTrunk corrects ~88% of double errors in
//!   BPTC(68,36); with the retry, all of them).
//! - Step 3 rebuilds a bad row from the parity of the others, so SDRTrunk
//!   "corrects" ~30% of random 68-bit blocks (and ~9% of random 128-bit ones)
//!   with 5..18 flips. Decodes needing more than `MAX_CORRECTED_68_36` /
//!   `MAX_CORRECTED_128_77` flips are rejected; no random block passes then.

use super::bit_distance;
use super::hamming::{ErrorIndex, Hamming16, Hamming17, IHamming};

/// BPTC with `row_count` Hamming-protected rows of `column_count` bits, the
/// last row being even column parity. Ports `BPTCBase`.
pub struct BPTCBase<H: IHamming> {
    hamming: H,
    column_count: usize,
    row_count: usize,
    /// `canCorrectMultiRow2BitErrors()`: true for BPTC(128,77) only.
    multi_row_2_bit_errors: bool,
}

fn count(flags: &[bool]) -> usize {
    flags.iter().filter(|&&f| f).count()
}

impl<H: IHamming> BPTCBase<H> {
    /// Ports the `BPTCBase` constructor (plus the `canCorrectMultiRow2BitErrors()` override).
    pub fn new(
        hamming: H,
        column_count: usize,
        row_count: usize,
        multi_row_2_bit_errors: bool,
    ) -> Self {
        BPTCBase {
            hamming,
            column_count,
            row_count,
            multi_row_2_bit_errors,
        }
    }

    /// Corrects the deinterleaved block in place; true when every row and
    /// column then checks. Ports `BPTCBase.correct()` (plus the retry above).
    pub fn correct(&self, message: &mut [u8]) -> bool {
        let received = message.to_vec();
        if self.correct_steps(message, true) {
            return true;
        }
        message.copy_from_slice(&received);
        self.correct_steps(message, false)
    }

    /// SDRTrunk's correction steps; `row_fixes` enables step 1.
    fn correct_steps(&self, message: &mut [u8], row_fixes: bool) -> bool {
        let mut columns = self.get_column_errors(message);
        let mut rows = self.get_row_errors(message);

        if count(&columns) == 0 && count(&rows) == 0 {
            return true;
        }

        //1: fix non-shadowed single bit errors in each row
        for row in 0..self.row_count {
            if row_fixes && rows[row] {
                self.correct_1_bit_errors(row, message, &mut rows);
            }
        }

        columns = self.get_column_errors(message);

        if count(&columns) == 0 && count(&rows) == 0 {
            return true;
        }

        //2: fix non-shadowed multi bit errors (up to 2) in each row
        if count(&columns) <= 2 {
            for row in 0..self.row_count {
                if rows[row] {
                    self.correct_multi_bit_errors(row, message, &mut columns, &mut rows);
                }
            }
        }

        //3: fix single row, multi-column errors
        if count(&rows) == 1 && count(&columns) > 0 {
            if let Some(row) = rows.iter().position(|&r| r) {
                self.correct_multi_bit_errors(row, message, &mut columns, &mut rows);
            }
        }

        //4: fix one or more rows that each have 2 bit errors, not shadowing each other
        if self.multi_row_2_bit_errors && count(&rows) > 0 && count(&columns) / count(&rows) == 2 {
            self.correct_multiple_row_two_bit_errors_not_shadowing(
                &mut columns,
                &mut rows,
                message,
            );
        }

        count(&columns) == 0 && count(&rows) == 0 && self.is_correct(message)
    }

    /// Every row passes Hamming and every column has even parity.
    fn is_correct(&self, message: &[u8]) -> bool {
        (0..self.row_count).all(|row| self.is_row_correct(row, message))
            && (0..self.column_count).all(|column| self.is_column_correct(column, message))
    }

    /// Ports `isColumnCorrect()`.
    fn is_column_correct(&self, column: usize, message: &[u8]) -> bool {
        (0..self.row_count).fold(0, |acc, row| {
            acc ^ message[row * self.column_count + column]
        }) == 0
    }

    fn get_column(&self, index: usize) -> usize {
        index % self.column_count
    }

    fn get_index(&self, column: usize, row: usize) -> usize {
        row * self.column_count + column
    }

    fn is_row_correct(&self, row: usize, message: &[u8]) -> bool {
        self.get_row_error_index(row, message) == ErrorIndex::NoErrors
    }

    fn get_row_error_index(&self, row: usize, message: &[u8]) -> ErrorIndex {
        self.hamming
            .get_error_index(message, row * self.column_count)
    }

    fn get_row_errors(&self, message: &[u8]) -> Vec<bool> {
        (0..self.row_count)
            .map(|row| !self.is_row_correct(row, message))
            .collect()
    }

    fn get_column_errors(&self, message: &[u8]) -> Vec<bool> {
        (0..self.column_count)
            .map(|column| !self.is_column_correct(column, message))
            .collect()
    }

    /// Flips each row/column-pair that makes a 2-bit-error row correct. Ports
    /// `correctMultipleRowTwoBitErrorsNotShadowing()`.
    fn correct_multiple_row_two_bit_errors_not_shadowing(
        &self,
        columns: &mut [bool],
        rows: &mut [bool],
        message: &mut [u8],
    ) {
        for row in 0..self.row_count {
            if !rows[row] {
                continue;
            }
            'pairs: for column1 in 0..self.column_count {
                if !columns[column1] {
                    continue;
                }
                for column2 in column1 + 1..self.column_count {
                    if !columns[column2] {
                        continue;
                    }
                    let index1 = self.get_index(column1, row);
                    let index2 = self.get_index(column2, row);
                    message[index1] ^= 1;
                    message[index2] ^= 1;

                    if self.is_row_correct(row, message) {
                        columns[column1] = false;
                        columns[column2] = false;
                        rows[row] = false;
                        break 'pairs;
                    }

                    message[index1] ^= 1;
                    message[index2] ^= 1;
                }
            }
        }
    }

    /// Fixes a row's single bit error from its Hamming index. Ports `correct1BitErrors()`.
    fn correct_1_bit_errors(&self, row: usize, message: &mut [u8], rows: &mut [bool]) {
        if let ErrorIndex::At(index) = self.get_row_error_index(row, message) {
            message[index] ^= 1;
            if self.is_row_correct(row, message) {
                rows[row] = false;
                return;
            }
            message[index] ^= 1;
        }
    }

    /// Flips the row's bits under every column error, plus the Hamming fix if
    /// still needed, and keeps them if row and columns then check. Ports `correctMultiBitErrors()`.
    fn correct_multi_bit_errors(
        &self,
        row: usize,
        message: &mut [u8],
        columns: &mut [bool],
        rows: &mut [bool],
    ) {
        let mut test_flips: Vec<usize> = Vec::new();

        for column in 0..self.column_count {
            if columns[column] {
                let to_flip = self.get_index(column, row);
                message[to_flip] ^= 1;
                test_flips.push(to_flip);
            }
        }

        if !self.is_row_correct(row, message) {
            if let ErrorIndex::At(index) = self.get_row_error_index(row, message) {
                message[index] ^= 1;
                match test_flips.iter().position(|&f| f == index) {
                    Some(p) => {
                        test_flips.remove(p);
                    }
                    None => test_flips.push(index),
                }
            }
        }

        if self.is_row_correct(row, message)
            && test_flips
                .iter()
                .all(|&f| self.is_column_correct(self.get_column(f), message))
        {
            rows[row] = false;
            for &f in &test_flips {
                columns[self.get_column(f)] = false;
            }
            return;
        }

        for &f in &test_flips {
            message[f] ^= 1;
        }
    }
}

/// Most bit flips a BPTC(68,36) decode may make (d = 6; random blocks need 5+).
pub const MAX_CORRECTED_68_36: u32 = 4;
/// Most bit flips a BPTC(128,77) decode may make (d = 8; random blocks need 9+).
pub const MAX_CORRECTED_128_77: u32 = 6;

/// Deinterleaves a short LC block (4 CACH payloads, 68 bits). Ports `BPTC_68_36.deinterleave()`.
pub fn deinterleave_68_36(interleaved: &[u8]) -> [u8; 68] {
    let mut deinterleaved = [0u8; 68];
    for index in 0..67 {
        deinterleaved[index * 17 % 67] = interleaved[index];
    }
    deinterleaved[67] = interleaved[67];
    deinterleaved
}

/// Decodes a short LC block: 36 bits (28 SLC + CRC-8) and the corrected bit
/// count, `None` if uncorrectable. Check `crc::crc8(&bits) == 0` too (SDRTrunk's
/// `SLCAssembler` overwrites that check with the BPTC result). Ports `BPTC_68_36.extract()`.
pub fn decode_68_36(interleaved: &[u8]) -> Option<([u8; 36], u32)> {
    let mut deinterleaved = deinterleave_68_36(interleaved);
    let received = deinterleaved;
    if !BPTCBase::new(Hamming17, 17, 4, false).correct(&mut deinterleaved) {
        return None;
    }

    let corrected = bit_distance(&received, &deinterleaved);
    if corrected > MAX_CORRECTED_68_36 {
        return None;
    }

    let mut extracted = [0u8; 36];
    for row in 0..3 {
        extracted[row * 12..row * 12 + 12].copy_from_slice(&deinterleaved[row * 17..row * 17 + 12]);
    }
    Some((extracted, corrected))
}

/// Deinterleaves an embedded LC block (4 x 32-bit fragments). Ports `BPTC_128_77.deinterleave()`.
pub fn deinterleave_128_77(interleaved: &[u8]) -> [u8; 128] {
    let mut deinterleaved = [0u8; 128];
    deinterleaved[127] = interleaved[127];
    for i in 0..127 {
        deinterleaved[i] = interleaved[(i * 8) % 127];
    }
    deinterleaved
}

/// Decodes an embedded LC block: 77 bits (72 LC + 5-bit checksum) and the
/// corrected bit count, `None` if uncorrectable. Check `crc::checksum_5(&bits) == 0`
/// too. Ports `BPTC_128_77.extract()`.
pub fn decode_128_77(interleaved: &[u8]) -> Option<([u8; 77], u32)> {
    let mut deinterleaved = deinterleave_128_77(interleaved);
    let received = deinterleaved;
    if !BPTCBase::new(Hamming16, 16, 8, true).correct(&mut deinterleaved) {
        return None;
    }

    let corrected = bit_distance(&received, &deinterleaved);
    if corrected > MAX_CORRECTED_128_77 {
        return None;
    }

    // Rows 0 and 1 carry 11 LC bits, rows 2..7 carry 10 LC bits and one checksum bit.
    let mut extracted = [0u8; 77];
    let mut pointer = 0;
    for row in 0..2 {
        extracted[pointer..pointer + 11].copy_from_slice(&deinterleaved[row * 16..row * 16 + 11]);
        pointer += 11;
    }
    for row in 2..7 {
        extracted[pointer..pointer + 10].copy_from_slice(&deinterleaved[row * 16..row * 16 + 10]);
        pointer += 10;
        extracted[70 + row] = deinterleaved[row * 16 + 10];
    }
    Some((extracted, corrected))
}

#[cfg(test)]
#[path = "bptc_tests.rs"]
mod tests;
