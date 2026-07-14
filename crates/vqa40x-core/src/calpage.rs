//! The 512-byte factory calibration page served through registers 0x0D/0x19.
//!
//! The default page is a REAL one, extracted from a USB capture of the
//! official app reading a QA402 at connect (`tools/extract_calpage.py`).
//! (The page holds only per-range gain trims; no serial number or other
//! identifier is embedded in it.)
//!
//! Full layout, reverse-engineered from that page (host byte order — the
//! wire carries each 4-byte word reversed):
//!
//! ```text
//! 0x000  u32 = 0          version/flags
//! 0x004  u32 = 76         payload length in 16-bit words (0x010..0x0A8)
//! 0x008  u32 = 50         unknown constant (schema id?)
//! 0x00C  u16 = 0x0023     block marker, then u16 = 0xDEAD sentinel
//! 0x010  u32 = 8          ADC range count
//! 0x014  u32 = 4          DAC range count
//! 0x018  8 × { i16 level dBV; f32 trim dB } L then R   ADC records
//! 0x078  4 × { i16 level dBV; f32 trim dB } L then R   DAC records
//! 0x0A8  u16 = 0x0011     end marker, then u16 = 0xDEAD sentinel
//! 0x0AC  zeros
//! 0x1FE  u16 big-endian   CRC-16/BUYPASS (poly 0x8005, init 0, unreflected)
//!                          over bytes [0x000..0x1FE)
//! ```
//!
//! The CRC makes the page self-validating — a page failing it is rejected by
//! the official app as invalid calibration data. [`generate_page`] builds a
//! fully valid page from scratch with randomized-but-realistic trims.
//!
//! The audio engine reads its ADC/DAC trims from the page actually served,
//! so a host that reads the page and calibrates its math measures a loopback
//! of exactly `loopback_gain_db` (0 dB for an ideal cable) — the same closure
//! as real hardware, with any page.

/// Real factory calibration page of a QA402 (from the connect capture).
pub const REAL_QA402_PAGE: &[u8; 512] = include_bytes!("../assets/calpage-qa402.bin");

/// ADC record byte offsets, indexed by input-range register value (0..=7).
pub const ADC_OFFSETS: [usize; 8] = [24, 36, 48, 60, 72, 84, 96, 108];
/// DAC record byte offsets, indexed by output-range register value (0..=3).
pub const DAC_OFFSETS: [usize; 4] = [120, 132, 144, 156];

pub const ADC_LEVELS_DBV: [i16; 8] = [0, 6, 12, 18, 24, 30, 36, 42];
pub const DAC_LEVELS_DBV: [i16; 4] = [-12, -2, 8, 18];

/// Nominal trims used when a record does not decode (custom/corrupt page).
pub const NOMINAL_ADC_DB: f32 = 8.75;
pub const NOMINAL_DAC_DB: f32 = -0.30;

/// The f32 dB trim of the record at `offset` (+6 for the right channel),
/// `None` when out of range or implausible (same validity rule as the hosts).
pub fn trim_db(page: &[u8], offset: usize) -> Option<f32> {
    let p = offset + 2;
    if p + 4 > page.len() {
        return None;
    }
    let v = f32::from_le_bytes([page[p], page[p + 1], page[p + 2], page[p + 3]]);
    (v.is_finite() && v.abs() < 20.0).then_some(v)
}

/// ADC trim (dB) for an input-range index and channel (0 = left, 1 = right).
pub fn adc_trim_db(page: &[u8], range: usize, channel: usize) -> f32 {
    ADC_OFFSETS
        .get(range)
        .and_then(|o| trim_db(page, o + 6 * channel))
        .unwrap_or(NOMINAL_ADC_DB)
}

/// DAC trim (dB) for an output-range index and channel (0 = left, 1 = right).
pub fn dac_trim_db(page: &[u8], range: usize, channel: usize) -> f32 {
    DAC_OFFSETS
        .get(range)
        .and_then(|o| trim_db(page, o + 6 * channel))
        .unwrap_or(NOMINAL_DAC_DB)
}

/// CRC-16/BUYPASS (aka CRC-16/UMTS): poly 0x8005, init 0x0000, unreflected,
/// no final xor. The page stores it big-endian in its last two bytes.
pub fn crc16_buypass(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// True if the last two bytes hold the correct CRC of the rest of the page.
pub fn page_crc_ok(page: &[u8; 512]) -> bool {
    crc16_buypass(&page[..510]) == u16::from_be_bytes([page[510], page[511]])
}

/// Generate a fully valid calibration page from scratch, with realistic
/// randomized trims (a plausible "different unit"). The framing, counts,
/// markers and CRC follow the reverse-engineered layout above; the trim
/// statistics mirror the real page: ADC ≈ +8.75 dB with a +0.02 dB/range
/// slope and a ≈ −0.17 dB step on the attenuated ranges (≥ 24 dBV), DAC
/// ≈ −0.3 dB per range with the right channel ≈ 0.055 dB below the left.
pub fn generate_page(seed: u64) -> [u8; 512] {
    // splitmix64 → uniform in [-1, 1).
    let mut state = seed;
    let mut unit = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f32 / (1u64 << 52) as f32 - 1.0
    };

    let mut page = [0u8; 512];
    page[0x04..0x08].copy_from_slice(&76u32.to_le_bytes()); // payload u16 words
    page[0x08..0x0C].copy_from_slice(&50u32.to_le_bytes());
    page[0x0C..0x0E].copy_from_slice(&0x0023u16.to_le_bytes());
    page[0x0E..0x10].copy_from_slice(&0xDEADu16.to_le_bytes());
    page[0x10..0x14].copy_from_slice(&8u32.to_le_bytes()); // ADC ranges
    page[0x14..0x18].copy_from_slice(&4u32.to_le_bytes()); // DAC ranges

    let record = |offset: usize, level: i16, trim: f32, page: &mut [u8; 512]| {
        page[offset..offset + 2].copy_from_slice(&level.to_le_bytes());
        page[offset + 2..offset + 6].copy_from_slice(&trim.to_le_bytes());
    };

    let adc_base = 8.75 + 0.04 * unit();
    let adc_slope = 0.020 + 0.003 * unit();
    let atten_step = -0.167 + 0.02 * unit();
    for (i, &off) in ADC_OFFSETS.iter().enumerate() {
        let l = adc_base
            + adc_slope * i as f32
            + if i >= 4 { atten_step } else { 0.0 }
            + 0.003 * unit();
        let r = l + 0.002 * unit();
        record(off, ADC_LEVELS_DBV[i], l, &mut page);
        record(off + 6, ADC_LEVELS_DBV[i], r, &mut page);
    }

    let dac_base = -0.30 + 0.06 * unit();
    let rl_offset = -0.055 + 0.004 * unit();
    for (i, &off) in DAC_OFFSETS.iter().enumerate() {
        let l = dac_base + 0.09 * unit();
        let r = l + rl_offset + 0.002 * unit();
        record(off, DAC_LEVELS_DBV[i], l, &mut page);
        record(off + 6, DAC_LEVELS_DBV[i], r, &mut page);
    }

    page[0xA8..0xAA].copy_from_slice(&0x0011u16.to_le_bytes());
    page[0xAA..0xAC].copy_from_slice(&0xDEADu16.to_le_bytes());

    let crc = crc16_buypass(&page[..510]);
    page[510..512].copy_from_slice(&crc.to_be_bytes());
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded real page must decode plausible trims at every record,
    /// with the expected magnitudes (ADC ≈ +8.7 dB, DAC ≈ −0.3 dB).
    #[test]
    fn real_page_decodes_all_records() {
        let page = &REAL_QA402_PAGE[..];
        for (i, &off) in ADC_OFFSETS.iter().enumerate() {
            for ch in 0..2 {
                let v = trim_db(page, off + 6 * ch).expect("ADC record decodes");
                assert!((8.0..9.5).contains(&v), "ADC range {i} ch {ch}: {v}");
                assert_eq!(adc_trim_db(page, i, ch), v);
            }
            // The level field is the range's dBV, i16 LE.
            let lvl = i16::from_le_bytes([page[off], page[off + 1]]);
            assert_eq!(lvl, ADC_LEVELS_DBV[i]);
        }
        for (i, &off) in DAC_OFFSETS.iter().enumerate() {
            for ch in 0..2 {
                let v = trim_db(page, off + 6 * ch).expect("DAC record decodes");
                assert!((-1.0..0.0).contains(&v), "DAC range {i} ch {ch}: {v}");
                assert_eq!(dac_trim_db(page, i, ch), v);
            }
        }
    }

    /// The reverse-engineered CRC must validate the real page — this is the
    /// proof of the layout (and guards the embedded asset against damage).
    #[test]
    fn real_page_crc_validates() {
        assert!(page_crc_ok(REAL_QA402_PAGE));
        // Any content change must break it.
        let mut tampered = *REAL_QA402_PAGE;
        tampered[30] ^= 1;
        assert!(!page_crc_ok(&tampered));
    }

    /// Generated pages carry the exact real framing (header, counts, levels,
    /// markers, valid CRC) with different, plausible trims.
    #[test]
    fn generated_page_is_valid_and_distinct() {
        let real = REAL_QA402_PAGE;
        let a = generate_page(1);
        let b = generate_page(2);
        assert!(page_crc_ok(&a));
        assert!(page_crc_ok(&b));
        assert_ne!(a, b, "different seeds, different trims");

        // Framing identical to the real page everywhere outside the trim
        // floats and the CRC.
        assert_eq!(&a[..0x18], &real[..0x18], "header");
        assert_eq!(&a[0xA8..0xAC], &real[0xA8..0xAC], "end marker");
        assert_eq!(&a[0xAC..0x1FE], &real[0xAC..0x1FE], "zero tail");
        for (i, &off) in ADC_OFFSETS.iter().enumerate() {
            assert_eq!(
                &a[off..off + 2],
                &ADC_LEVELS_DBV[i].to_le_bytes(),
                "ADC level"
            );
            for ch in 0..2 {
                let v = trim_db(&a, off + 6 * ch).expect("plausible ADC trim");
                assert!((8.0..9.5).contains(&v), "ADC trim {v}");
                assert_ne!(v, trim_db(real, off + 6 * ch).unwrap());
            }
        }
        for (i, &off) in DAC_OFFSETS.iter().enumerate() {
            assert_eq!(
                &a[off..off + 2],
                &DAC_LEVELS_DBV[i].to_le_bytes(),
                "DAC level"
            );
            for ch in 0..2 {
                let v = trim_db(&a, off + 6 * ch).expect("plausible DAC trim");
                assert!((-0.8..0.3).contains(&v), "DAC trim {v}");
            }
            // Right channel sits below the left, like the real unit.
            let (l, r) = (trim_db(&a, off).unwrap(), trim_db(&a, off + 6).unwrap());
            assert!(r < l, "R below L");
        }
    }

    /// Out-of-range or implausible records fall back to the nominal trims.
    #[test]
    fn fallback_on_bad_page() {
        let blank = [0u8; 512];
        // 0.0 dB is a valid (plausible) trim, so craft an implausible one.
        let mut bad = [0u8; 512];
        bad[26..30].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(adc_trim_db(&bad, 0, 0), NOMINAL_ADC_DB);
        assert_eq!(
            adc_trim_db(&blank[..20], 0, 0),
            NOMINAL_ADC_DB,
            "truncated page"
        );
        assert_eq!(
            dac_trim_db(&blank, 9, 0),
            NOMINAL_DAC_DB,
            "range out of bounds"
        );
    }
}
