//! Zstandard frames, the compressed encoding's container: one independent,
//! checksummed frame for the header and one for each durable batch,
//! concatenated. (dsh: `session-persistence-jsonl/src/zstd.ts`.)
//!
//! The backend owns the container, so it can append a batch as a frame and
//! recover from a crash mid-append without the seam knowing about
//! compression. These helpers are public for tools and tests that inspect
//! a stored log.

use std::io;
use std::ops::Range;

use zstd::stream::raw::{Decoder, InBuffer, Operation, OutBuffer};

/// Every frame starts with these four bytes, little-endian `0xFD2FB528`.
const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// zstd's default level, which dsh gets from Node's API (no level knob).
const LEVEL: i32 = 3;

/// Where a scan found frames. (dsh: `ZstdFrameScan`.)
#[derive(Debug, PartialEq, Eq)]
pub struct FrameScan {
    /// The structurally complete frames, in file order.
    pub frames: Vec<Range<usize>>,
    /// Where an incomplete last frame starts, when the bytes end inside it.
    pub torn_start: Option<usize>,
}

/// Finds the complete frames in `buffer` without decompressing them. Bytes
/// that end inside the last frame give its start, for repair; a structure
/// that can't be a frame is an error. With `max_frames`, stops after that
/// many complete frames, as a header-only read does. (dsh:
/// `scanZstdFrames`.)
pub fn scan_frames(buffer: &[u8], max_frames: Option<usize>) -> Result<FrameScan, String> {
    let mut frames = Vec::new();
    let mut offset = 0;
    let torn = |frames, start| {
        Ok(FrameScan {
            frames,
            torn_start: Some(start),
        })
    };
    while offset < buffer.len() {
        let start = offset;
        if buffer.len() - offset < 4 {
            return torn(frames, start);
        }
        if buffer[offset..offset + 4] != MAGIC {
            return Err(format!(
                "corrupt Zstandard session log: invalid frame magic at byte {offset}"
            ));
        }
        offset += 4;

        if offset == buffer.len() {
            return torn(frames, start);
        }
        let descriptor = buffer[offset];
        offset += 1;
        if descriptor & 0x18 != 0 {
            return Err(format!(
                "corrupt Zstandard session log: reserved frame-header bit at byte {}",
                offset - 1
            ));
        }
        let content_size_flag = descriptor >> 6;
        let single_segment = descriptor & 0x20 != 0;
        let checksum = descriptor & 0x04 != 0;
        let dictionary_bytes = match descriptor & 0x03 {
            3 => 4,
            flag => usize::from(flag),
        };
        let content_size_bytes = match content_size_flag {
            0 => usize::from(single_segment),
            flag => 1 << flag,
        };
        let remaining_header = usize::from(!single_segment) + dictionary_bytes + content_size_bytes;
        if buffer.len() - offset < remaining_header {
            return torn(frames, start);
        }
        offset += remaining_header;

        loop {
            if buffer.len() - offset < 3 {
                return torn(frames, start);
            }
            let header = u32::from(buffer[offset])
                | u32::from(buffer[offset + 1]) << 8
                | u32::from(buffer[offset + 2]) << 16;
            offset += 3;
            let last = header & 1 != 0;
            let block_type = (header >> 1) & 0x03;
            let block_size = (header >> 3) as usize;
            if block_type == 0x03 {
                return Err(format!(
                    "corrupt Zstandard session log: reserved block type at byte {}",
                    offset - 3
                ));
            }
            // An RLE block's payload is the one byte it repeats.
            let payload = if block_type == 0x01 { 1 } else { block_size };
            if buffer.len() - offset < payload {
                return torn(frames, start);
            }
            offset += payload;
            if last {
                break;
            }
        }

        if checksum {
            if buffer.len() - offset < 4 {
                return torn(frames, start);
            }
            offset += 4;
        }
        frames.push(start..offset);
        if max_frames == Some(frames.len()) {
            break;
        }
    }
    Ok(FrameScan {
        frames,
        torn_start: None,
    })
}

/// One independently decodable frame holding `input`, with its checksum.
/// (dsh: `compressZstdFrame`.)
pub fn compress_frame(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), LEVEL)?;
    encoder.include_checksum(true)?;
    io::Write::write_all(&mut encoder, input)?;
    encoder.finish()
}

/// The plaintext of one complete frame, its checksum checked. (dsh:
/// `decompressZstdFrame` and the frame decoder's `decode`.)
pub fn decompress_frame(frame: &[u8]) -> io::Result<Vec<u8>> {
    zstd::stream::decode_all(frame)
}

/// The plaintext an incomplete last frame gives: every block that arrived
/// whole. Decoding stops at the first error or at the end of the bytes,
/// keeping what came before; the frame's checksum can't be checked, since
/// it never arrived. The caller has established that the frame is torn.
/// (dsh: `decompressZstdPrefix`.)
pub fn decompress_prefix(input: &[u8]) -> Vec<u8> {
    let mut plaintext = Vec::new();
    let Ok(mut decoder) = Decoder::new() else {
        return plaintext;
    };
    let mut source = InBuffer::around(input);
    let mut chunk = vec![0; zstd::zstd_safe::DCtx::out_size()];
    loop {
        let consumed = source.pos();
        let mut sink = OutBuffer::around(&mut chunk[..]);
        let result = decoder.run(&mut source, &mut sink);
        let produced = sink.pos();
        plaintext.extend_from_slice(&chunk[..produced]);
        let stalled = produced == 0 && source.pos() == consumed;
        if result.is_err() || stalled {
            return plaintext;
        }
    }
}

#[cfg(test)]
// A list holding one frame's range is what these tests mean.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn scan(buffer: &[u8]) -> Result<FrameScan, String> {
        scan_frames(buffer, None)
    }

    fn complete(frames: Vec<Range<usize>>) -> Result<FrameScan, String> {
        Ok(FrameScan {
            frames,
            torn_start: None,
        })
    }

    fn torn_at_zero() -> Result<FrameScan, String> {
        Ok(FrameScan {
            frames: Vec::new(),
            torn_start: Some(0),
        })
    }

    fn with(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// A frame with `descriptor`, an empty last raw block and, if the
    /// descriptor says so, a checksum. (zstd.spec.ts: `emptyStructuralFrame`.)
    fn empty_structural_frame(descriptor: u8) -> Vec<u8> {
        let content_size_flag = descriptor >> 6;
        let single_segment = descriptor & 0x20 != 0;
        let dictionary = [0, 1, 2, 4][usize::from(descriptor & 0x03)];
        let content_size = match content_size_flag {
            0 => usize::from(single_segment),
            flag => 1 << flag,
        };
        let header = vec![0; usize::from(!single_segment) + dictionary + content_size];
        let checksum = if descriptor & 0x04 == 0 {
            vec![]
        } else {
            vec![0; 4]
        };
        with(&[&MAGIC, &[descriptor], &header, &[1, 0, 0], &checksum])
    }

    /// Deterministic text that barely compresses, so a frame spans several
    /// blocks. (zstd.spec.ts: `deterministicNoise`.)
    fn noise(length: usize) -> String {
        let mut state: u32 = 0x1234_5678;
        (0..length)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                char::from(33 + (state % 90) as u8)
            })
            .collect()
    }

    // "scans concatenated checksummed frames and honors a frame limit"
    #[test]
    fn concatenated_checksummed_frames_scan_and_decode() {
        let first = compress_frame(b"header\n").unwrap();
        let second = compress_frame(b"event\n").unwrap();
        let stream = with(&[&first, &second]);
        assert_eq!(scan(b""), complete(Vec::new()));
        assert_eq!(
            scan(&stream),
            complete(vec![0..first.len(), first.len()..stream.len()])
        );
        assert_eq!(
            scan_frames(&stream, Some(1)),
            complete(vec![0..first.len()])
        );
        // The checksum flag is set on every frame.
        assert_eq!(first[4] & 0x04, 0x04);
        assert_eq!(second[4] & 0x04, 0x04);
        assert_eq!(decompress_frame(&first).unwrap(), b"header\n");
        assert_eq!(decompress_frame(&second).unwrap(), b"event\n");
    }

    // "distinguishes incomplete frame regions from invalid complete structure"
    #[test]
    fn incomplete_regions_are_torn_and_invalid_structure_is_refused() {
        assert_eq!(scan(&MAGIC[..2]), torn_at_zero());
        assert_eq!(scan(&MAGIC), torn_at_zero());
        assert_eq!(
            scan(&[0; 4]),
            Err("corrupt Zstandard session log: invalid frame magic at byte 0".into())
        );
        assert_eq!(
            scan(&with(&[&MAGIC, &[0x08]])),
            Err("corrupt Zstandard session log: reserved frame-header bit at byte 4".into())
        );
        // No window descriptor after a multi-segment descriptor.
        assert_eq!(scan(&with(&[&MAGIC, &[0x00]])), torn_at_zero());
        // Two bytes of a three-byte block header.
        assert_eq!(
            scan(&with(&[&MAGIC, &[0x20, 0x00, 0x01, 0x00]])),
            torn_at_zero()
        );
        // A raw block of five bytes with two present.
        let raw_five = [(5 << 3) | 1, 0, 0];
        assert_eq!(
            scan(&with(&[&MAGIC, &[0x20, 0x00], &raw_five, &[0x01, 0x02]])),
            torn_at_zero()
        );
        assert_eq!(
            scan(&with(&[&MAGIC, &[0x20, 0x00, 0x07, 0x00, 0x00]])),
            Err("corrupt Zstandard session log: reserved block type at byte 6".into())
        );
        // A torn frame after a complete one starts where it starts.
        let frame = compress_frame(b"event\n").unwrap();
        assert_eq!(
            scan(&with(&[&frame, &MAGIC[..3]])),
            Ok(FrameScan {
                frames: vec![0..frame.len()],
                torn_start: Some(frame.len()),
            })
        );
    }

    // "covers standard header variants, RLE blocks, multiple blocks, and checksums"
    #[test]
    fn header_variants_rle_and_multiple_blocks_scan() {
        for descriptor in [0x00, 0x21, 0x42, 0x83, 0xE3] {
            let frame = empty_structural_frame(descriptor);
            assert_eq!(
                scan(&frame),
                complete(vec![0..frame.len()]),
                "{descriptor:#x}"
            );
        }
        let rle = with(&[
            &MAGIC,
            &[0x20, 0x01],
            &[(1 << 3) | (1 << 1) | 1, 0, 0],
            &[0x41],
        ]);
        assert_eq!(scan(&rle), complete(vec![0..rle.len()]));
        let two_blocks = with(&[&MAGIC, &[0x20, 0x00], &[0, 0, 0], &[1, 0, 0]]);
        assert_eq!(scan(&two_blocks), complete(vec![0..two_blocks.len()]));
        let checksummed = empty_structural_frame(0x24);
        assert_eq!(scan(&checksummed[..checksummed.len() - 1]), torn_at_zero());
        assert_eq!(scan(&checksummed), complete(vec![0..checksummed.len()]));
    }

    #[test]
    fn a_complete_frame_with_a_wrong_checksum_is_refused() {
        let mut frame = compress_frame(b"{\"type\":\"turn/start\"}\n").unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert_eq!(scan(&frame), complete(vec![0..frame.len()]));
        assert!(decompress_frame(&frame).is_err());
    }

    // zstd.compat.spec.ts: "missing checksum byte" is torn.
    #[test]
    fn a_frame_missing_its_last_checksum_byte_is_torn_and_decodes_whole() {
        let frame = compress_frame(b"one\ntwo\n").unwrap();
        let cut = &frame[..frame.len() - 1];
        assert_eq!(scan(cut), torn_at_zero());
        assert_eq!(decompress_prefix(cut), b"one\ntwo\n");
    }

    /// A batch big enough to span blocks, cut inside a later block, gives
    /// back the blocks before the cut: the records already flushed.
    #[test]
    fn a_prefix_decode_keeps_the_blocks_that_arrived_whole() {
        let records = format!("first\nsecond\n{}\n", noise(300_000));
        let frame = compress_frame(records.as_bytes()).unwrap();
        // Cut in the middle: past the first block, before the end.
        let cut = &frame[..frame.len() / 2];
        assert_eq!(scan(cut), torn_at_zero());
        let plaintext = decompress_prefix(cut);
        assert!(!plaintext.is_empty());
        assert!(plaintext.len() < records.len());
        assert!(records.as_bytes().starts_with(&plaintext));
        assert!(plaintext.starts_with(b"first\nsecond\n"));
    }

    #[test]
    fn a_frame_cut_inside_its_header_gives_no_plaintext() {
        let frame = compress_frame(b"one\n").unwrap();
        for end in [2, 4, 5] {
            assert!(decompress_prefix(&frame[..end]).is_empty(), "cut at {end}");
        }
        assert!(decompress_prefix(&[0; 8]).is_empty());
    }
}
