//! Minimal GPT parser: find the APFS container partition's byte range.
//!
//! Only the fields needed to locate the APFS container are read. Offsets
//! are validated against the project plan's empirically-locked GPT layout.

use apfs_core::block_device::{BlockDevice, BlockError};

/// APFS container GPT partition type GUID, canonical string form.
pub const APFS_TYPE_GUID: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";

const SECTOR: u64 = 512;

#[derive(Debug)]
pub enum GptError {
    Block(BlockError),
    NoProtectiveMbr,
    NoGptHeader,
    NoApfsPartition,
    Malformed(&'static str),
}

impl From<BlockError> for GptError {
    fn from(e: BlockError) -> Self {
        GptError::Block(e)
    }
}

/// Byte range of the APFS container within the whole disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerRange {
    pub offset: u64,
    pub length: u64,
}

fn u32_le(b: &[u8], at: usize) -> Result<u32, GptError> {
    let s: [u8; 4] = b
        .get(at..at + 4)
        .ok_or(GptError::Malformed("u32"))?
        .try_into()
        .map_err(|_| GptError::Malformed("u32"))?;
    Ok(u32::from_le_bytes(s))
}

fn u64_le(b: &[u8], at: usize) -> Result<u64, GptError> {
    let s: [u8; 8] = b
        .get(at..at + 8)
        .ok_or(GptError::Malformed("u64"))?
        .try_into()
        .map_err(|_| GptError::Malformed("u64"))?;
    Ok(u64::from_le_bytes(s))
}

/// Format the 16-byte GPT partition type field as the canonical GUID
/// string. GPT stores Data1/2/3 little-endian and Data4 big-endian.
#[allow(clippy::indexing_slicing)] // fixed [u8;16] — all indices < 16, always in bounds
fn guid_string(g: &[u8]) -> Result<String, GptError> {
    let g: [u8; 16] = g
        .get(..16)
        .ok_or(GptError::Malformed("guid"))?
        .try_into()
        .map_err(|_| GptError::Malformed("guid"))?;
    let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let d2 = u16::from_le_bytes([g[4], g[5]]);
    let d3 = u16::from_le_bytes([g[6], g[7]]);
    Ok(format!(
        "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    ))
}

/// Locate the first APFS-type partition and return its byte range.
pub fn find_apfs_container<D: BlockDevice>(dev: &mut D) -> Result<ContainerRange, GptError> {
    // Protective MBR signature.
    let mut mbr = [0u8; 512];
    dev.read_at(0, &mut mbr)?;
    if mbr.get(510..512) != Some(&[0x55, 0xAA]) {
        return Err(GptError::NoProtectiveMbr);
    }

    // GPT header at LBA1.
    let mut hdr = [0u8; 512];
    dev.read_at(SECTOR, &mut hdr)?;
    if hdr.get(0..8) != Some(b"EFI PART") {
        return Err(GptError::NoGptHeader);
    }
    let part_lba = u64_le(&hdr, 72)?;
    let num_parts = u32_le(&hdr, 80)?;
    let part_size = u32_le(&hdr, 84)? as u64;
    if part_size < 128 || num_parts == 0 || num_parts > 65_536 {
        return Err(GptError::Malformed("gpt header fields"));
    }

    let table_base = part_lba
        .checked_mul(SECTOR)
        .ok_or(GptError::Malformed("table base overflow"))?;
    for i in 0..num_parts as u64 {
        let mut ent = vec![0u8; part_size as usize];
        let ent_off = table_base
            .checked_add(i * part_size)
            .ok_or(GptError::Malformed("entry offset overflow"))?;
        dev.read_at(ent_off, &mut ent)?;
        if guid_string(&ent)?.eq_ignore_ascii_case(APFS_TYPE_GUID) {
            let first_lba = u64_le(&ent, 32)?;
            let last_lba = u64_le(&ent, 40)?;
            if last_lba < first_lba {
                return Err(GptError::Malformed("last_lba < first_lba"));
            }
            let offset = first_lba
                .checked_mul(SECTOR)
                .ok_or(GptError::Malformed("offset overflow"))?;
            let length = (last_lba - first_lba + 1)
                .checked_mul(SECTOR)
                .ok_or(GptError::Malformed("length overflow"))?;
            return Ok(ContainerRange { offset, length });
        }
    }
    Err(GptError::NoApfsPartition)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    struct MemDisk {
        data: Vec<u8>,
    }
    impl BlockDevice for MemDisk {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let o = offset as usize;
            let end = o + buf.len();
            let src = self.data.get(o..end).ok_or(BlockError::OutOfRange {
                offset,
                len: buf.len() as u64,
                size: self.data.len() as u64,
            })?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    fn apfs_type_bytes() -> [u8; 16] {
        // 7C3457EF-0000-11AA-AA11-00306543ECAC in GPT mixed-endian form.
        [
            0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43,
            0xEC, 0xAC,
        ]
    }

    fn build_disk(first_lba: u64, last_lba: u64) -> MemDisk {
        let total = (last_lba as usize + 16) * 512;
        let mut d = vec![0u8; total];
        d[510] = 0x55;
        d[511] = 0xAA;
        d[512..520].copy_from_slice(b"EFI PART");
        d[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes()); // partLBA=2
        d[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes()); // numParts
        d[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes()); // partSize
        let ent = 2 * 512;
        d[ent..ent + 16].copy_from_slice(&apfs_type_bytes());
        d[ent + 32..ent + 40].copy_from_slice(&first_lba.to_le_bytes());
        d[ent + 40..ent + 48].copy_from_slice(&last_lba.to_le_bytes());
        let cont = first_lba as usize * 512;
        d[cont + 32..cont + 36].copy_from_slice(b"NXSB");
        MemDisk { data: d }
    }

    #[test]
    fn finds_apfs_partition_range() {
        let mut disk = build_disk(40, 200);
        let r = find_apfs_container(&mut disk).unwrap();
        assert_eq!(r.offset, 40 * 512);
        assert_eq!(r.length, (200 - 40 + 1) * 512);
        // Sanity: NXSB magic sits at container offset + 32.
        let mut m = [0u8; 4];
        disk.read_at(r.offset + 32, &mut m).unwrap();
        assert_eq!(&m, b"NXSB");
    }

    #[test]
    fn rejects_missing_protective_mbr() {
        let mut disk = build_disk(40, 200);
        disk.data[510] = 0;
        assert!(matches!(
            find_apfs_container(&mut disk),
            Err(GptError::NoProtectiveMbr)
        ));
    }

    #[test]
    fn errors_when_no_apfs_partition() {
        let mut disk = build_disk(40, 200);
        let ent = 2 * 512;
        disk.data[ent] ^= 0xFF; // corrupt the type GUID
        assert!(matches!(
            find_apfs_container(&mut disk),
            Err(GptError::NoApfsPartition)
        ));
    }
}
