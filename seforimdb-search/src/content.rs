//! Port of `SeforimDbLineContentCodec.cs`.
//!
//! Turns a raw `content` value read from the seforim database into text.
//! From schema 6 on, line content is zstd-compressed with a PREBUILT
//! DICTIONARY that ships inside the database (`zstd_dict`). Older databases
//! store plain text. One codec covers all of them, dispatching on the
//! RUNTIME TYPE of the value:
//!
//! - NULL → None
//! - text → itself (or a base64-encoded frame, recovered)
//! - blob → decompressed with the dictionary the frame names
//!
//! Not thread-safe: the decompressor holds a reusable buffer, so each
//! connection gets its own codec.

use std::collections::HashMap;

use base64::Engine;
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::schema::table_exists;

/// Upper bound on a decompressed line, as a guard against a corrupt frame
/// header asking for an absurd buffer. The largest real line is ~1.3 MB.
const MAXIMUM_LINE_BYTES: u64 = 16 * 1024 * 1024;

/// The codec for one database.
pub enum LineContentCodec {
    /// Every value passes through untouched.
    Plain,
    /// One decompressor per dictionary, keyed by the id zstd embeds in both
    /// the dictionary and every frame built from it.
    Zstd {
        decompressors: HashMap<u32, zstd::bulk::Decompressor<'static>>,
    },
}

impl LineContentCodec {
    /// Builds the codec for an open connection: loads every dictionary from
    /// `zstd_dict`, or `Plain` when the table is absent.
    pub fn create(conn: &Connection) -> Result<LineContentCodec> {
        if !table_exists(conn, "zstd_dict")? {
            return Ok(LineContentCodec::Plain);
        }
        let mut stmt = conn.prepare("SELECT dict FROM zstd_dict")?;
        let mut rows = stmt.query([])?;
        let mut decompressors = HashMap::new();
        while let Some(row) = rows.next()? {
            let dict: Option<Vec<u8>> = row.get(0)?;
            let Some(dict) = dict else { continue };
            // The id the frames reference is the one zstd stamped into the
            // dictionary, NOT the table's `id` column.
            let id = read_dictionary_id(&dict)?;
            let decompressor = zstd::bulk::Decompressor::with_dictionary(&dict)
                .map_err(|e| Error::ContentCodec(format!("loading zstd dictionary: {e}")))?;
            decompressors.insert(id, decompressor);
        }
        // A `zstd_dict` table with no usable rows is a broken database, not a
        // plain one: the content column will still hold frames.
        if decompressors.is_empty() {
            return Err(Error::ContentCodec(
                "The database has a zstd_dict table but no dictionaries in it, so its \
                 compressed line content cannot be decoded."
                    .into(),
            ));
        }
        Ok(LineContentCodec::Zstd { decompressors })
    }

    /// True when this database compresses line content.
    pub fn is_compressed(&self) -> bool {
        matches!(self, LineContentCodec::Zstd { .. })
    }

    /// The text of a `content` value, or None when the value is NULL.
    pub fn decode(&mut self, value: ValueRef<'_>) -> Result<Option<String>> {
        match value {
            ValueRef::Null => Ok(None),
            ValueRef::Text(bytes) => {
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| Error::ContentCodec("line text is not valid UTF-8".into()))?;
                // Some rows in the shipped library hold the frame as BASE64
                // TEXT rather than as a blob. Recover those; anything else is
                // the text it claims to be.
                if self.is_compressed() {
                    if let Some(recovered) = self.try_decode_base64_frame(text) {
                        return Ok(Some(recovered));
                    }
                }
                Ok(Some(text.to_string()))
            }
            ValueRef::Blob(frame) => {
                if !self.is_compressed() {
                    return Err(Error::ContentCodec(
                        "A line holds compressed content but the database has no zstd_dict \
                         table, so no dictionary is available to decode it."
                            .into(),
                    ));
                }
                self.decode_frame(frame).map(Some)
            }
            ValueRef::Integer(_) | ValueRef::Real(_) => Err(Error::ContentCodec(
                "A line's content column holds a number, which is neither text nor a \
                 compressed frame."
                    .into(),
            )),
        }
    }

    /// `decode`, mapping NULL to empty. For base line content, where a missing
    /// row and an empty line read the same way.
    pub fn decode_or_empty(&mut self, value: ValueRef<'_>) -> Result<String> {
        Ok(self.decode(value)?.unwrap_or_default())
    }

    fn decode_frame(&mut self, frame: &[u8]) -> Result<String> {
        let LineContentCodec::Zstd { decompressors } = self else {
            return Err(Error::ContentCodec(
                "A line holds compressed content but no dictionary is loaded.".into(),
            ));
        };
        let dictionary_id = read_frame_dictionary_id(frame)?;
        let decompressor = decompressors.get_mut(&dictionary_id).ok_or_else(|| {
            Error::ContentCodec(format!(
                "A line was compressed with dictionary {dictionary_id}, which is not present in \
                 the database's zstd_dict table."
            ))
        })?;

        // The frame header's content size is a CAPACITY HINT; the number of
        // bytes actually written is what counts.
        let declared = zstd_safe::get_frame_content_size(frame)
            .map_err(|e| {
                Error::ContentCodec(format!(
                    "A line's compressed content could not be read as a zstd frame: {e}"
                ))
            })?
            .ok_or_else(|| {
                Error::ContentCodec(
                    "A line's compressed content does not declare its size.".into(),
                )
            })?;
        if declared == 0 {
            return Ok(String::new());
        }
        if declared > MAXIMUM_LINE_BYTES {
            return Err(Error::ContentCodec(format!(
                "A line declares a decompressed size of {declared} bytes, beyond the \
                 {MAXIMUM_LINE_BYTES}-byte limit for a single line."
            )));
        }
        let bytes = decompressor
            .decompress(frame, declared as usize)
            .map_err(|e| {
                Error::ContentCodec(format!(
                    "A line's compressed content could not be decompressed: {e}"
                ))
            })?;
        String::from_utf8(bytes)
            .map_err(|_| Error::ContentCodec("decompressed line is not valid UTF-8".into()))
    }

    /// Recovers a frame stored as base64 TEXT. The test is not "looks like
    /// base64": the string must decode to bytes starting with the zstd frame
    /// magic AND decompress with one of this database's dictionaries.
    fn try_decode_base64_frame(&mut self, text: &str) -> Option<String> {
        // Base64 of the frame magic 28 B5 2F FD always begins "KLUv".
        if text.len() < 8 || !text.starts_with("KLUv") {
            return None;
        }
        let frame = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
        self.decode_frame(&frame).ok()
    }
}

/// The dictionary id stamped in a dictionary's own header: magic
/// 0xEC30A437 followed by the id, both little-endian.
fn read_dictionary_id(dict: &[u8]) -> Result<u32> {
    const DICTIONARY_MAGIC: u32 = 0xEC30_A437;
    if dict.len() < 8 {
        return Err(Error::ContentCodec(
            "A row in zstd_dict is too short to be a zstd dictionary.".into(),
        ));
    }
    let magic = u32::from_le_bytes([dict[0], dict[1], dict[2], dict[3]]);
    if magic != DICTIONARY_MAGIC {
        return Err(Error::ContentCodec(
            "A row in zstd_dict does not start with the zstd dictionary magic.".into(),
        ));
    }
    Ok(u32::from_le_bytes([dict[4], dict[5], dict[6], dict[7]]))
}

/// The dictionary id a frame was built with, read from the frame header.
/// Layout: 4-byte magic 0xFD2FB528, then the Frame Header Descriptor, whose
/// low two bits say how wide the dictionary id field is (0 = absent, 1, 2,
/// 3 = four bytes). A Window Descriptor byte sits between them UNLESS the
/// Single Segment flag (bit 5) is set.
fn read_frame_dictionary_id(frame: &[u8]) -> Result<u32> {
    const FRAME_MAGIC: u32 = 0xFD2F_B528;
    if frame.len() < 5 {
        return Err(Error::ContentCodec(
            "A line's compressed content is too short to be a zstd frame.".into(),
        ));
    }
    let magic = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
    if magic != FRAME_MAGIC {
        return Err(Error::ContentCodec(
            "A line's content is a blob but does not start with the zstd frame magic.".into(),
        ));
    }
    let descriptor = frame[4];
    let mut width = (descriptor & 0x03) as usize;
    if width == 3 {
        width = 4;
    }
    if width == 0 {
        return Err(Error::ContentCodec(
            "A line's frame carries no dictionary id, so the dictionary that built it cannot \
             be identified."
                .into(),
        ));
    }
    let single_segment = descriptor & 0x20 != 0;
    let offset = if single_segment { 5 } else { 6 };
    if frame.len() < offset + width {
        return Err(Error::ContentCodec(
            "A line's frame header is truncated before its dictionary id.".into(),
        ));
    }
    let mut id = 0u32;
    for i in 0..width {
        id |= (frame[offset + i] as u32) << (8 * i);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_id_parsing() {
        let mut dict = vec![0x37, 0xA4, 0x30, 0xEC, 0x00, 0x80, 0x00, 0x00];
        dict.extend_from_slice(&[0u8; 16]);
        assert_eq!(read_dictionary_id(&dict).unwrap(), 32768);
        assert!(read_dictionary_id(&[1, 2, 3, 4, 5, 6, 7, 8]).is_err());
    }

    #[test]
    fn frame_dictionary_id_parsing() {
        // magic, descriptor: single segment (0x20) + 2-byte dict id (0x02), id 0x8000
        let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x22, 0x00, 0x80, 0x00];
        assert_eq!(read_frame_dictionary_id(&frame).unwrap(), 32768);
        // not single segment: window descriptor byte in between
        let frame2 = [0x28, 0xB5, 0x2F, 0xFD, 0x02, 0x00, 0x00, 0x80, 0x00];
        assert_eq!(read_frame_dictionary_id(&frame2).unwrap(), 32768);
        // no dictionary id
        let frame3 = [0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x00];
        assert!(read_frame_dictionary_id(&frame3).is_err());
    }

    #[test]
    fn roundtrip_with_dictionary() {
        // Train a tiny dictionary and compress a line with it, as the library does.
        let samples: Vec<Vec<u8>> = (0..64)
            .map(|i| format!("בראשית ברא אלהים את השמים ואת הארץ {i}").into_bytes())
            .collect();
        let dict = zstd::dict::from_samples(&samples, 1024).unwrap();
        let id = read_dictionary_id(&dict).unwrap();
        let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        let frame = compressor.compress("ויאמר אלהים יהי אור".as_bytes()).unwrap();
        assert_eq!(read_frame_dictionary_id(&frame).unwrap(), id);

        let mut decompressors = HashMap::new();
        decompressors.insert(id, zstd::bulk::Decompressor::with_dictionary(&dict).unwrap());
        let mut codec = LineContentCodec::Zstd { decompressors };
        assert_eq!(
            codec.decode(ValueRef::Blob(&frame)).unwrap().as_deref(),
            Some("ויאמר אלהים יהי אור")
        );
        // base64 text form of the same frame
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame);
        assert!(b64.starts_with("KLUv"));
        assert_eq!(
            codec.decode(ValueRef::Text(b64.as_bytes())).unwrap().as_deref(),
            Some("ויאמר אלהים יהי אור")
        );
        // plain text in a compressed DB passes through
        assert_eq!(
            codec.decode(ValueRef::Text("שלום".as_bytes())).unwrap().as_deref(),
            Some("שלום")
        );
        assert_eq!(codec.decode(ValueRef::Null).unwrap(), None);
    }
}
