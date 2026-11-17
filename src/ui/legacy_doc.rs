use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const MAX_TEXT_UNITS: usize = 4_000_000;
const MAX_CLX: usize = 16 * 1024 * 1024;

pub(super) fn main_text(path: &Path) -> Result<String> {
    let mut file = cfb::open(path).context("无法读取 Word 97–2003 文档容器")?;
    let mut header = Vec::new();
    file.open_stream("/WordDocument")?
        .take(4096)
        .read_to_end(&mut header)?;
    let u16_at = |at| le16(&header, at);
    ensure!(
        u16_at(0)? == 0xa5ec && u16_at(2)? >= 0xc1,
        "此 DOC 版本需要 LibreOffice"
    );
    let flags = u16_at(10)?;
    ensure!(flags & 0x8100 == 0, "加密的 DOC 无法直接预览");
    let words = u16_at(32)? as usize;
    let long_count_at = 34 + words * 2;
    let longs = u16_at(long_count_at)? as usize;
    ensure!(longs >= 4, "DOC 缺少正文长度");
    let long_start = long_count_at + 2;
    let text_units = le32(&header, long_start + 12)? as usize;
    ensure!(text_units <= MAX_TEXT_UNITS, "DOC 正文过大");
    if text_units == 0 {
        return Ok(String::new());
    }
    let pairs_at = long_start + longs * 4;
    // fcClx/lcbClx are the 34th pair in FibRgFcLcb97.
    ensure!(u16_at(pairs_at)? > 33, "DOC 缺少正文分片表");
    let clx_pair = pairs_at + 2 + 33 * 8;
    let clx_offset = u64::from(le32(&header, clx_pair)?);
    let clx_size = le32(&header, clx_pair + 4)? as usize;
    ensure!((5..=MAX_CLX).contains(&clx_size), "DOC 正文分片表大小无效");
    let table_stream = if flags & 0x0200 == 0 {
        "/0Table"
    } else {
        "/1Table"
    };
    let mut clx = vec![0; clx_size];
    {
        let mut table = file.open_stream(table_stream)?;
        table.seek(SeekFrom::Start(clx_offset))?;
        table.read_exact(&mut clx)?;
    }
    let mut stream = file.open_stream("/WordDocument")?;
    extract(&clx, text_units, &mut stream)
}

fn le16(buffer: &[u8], at: usize) -> Result<u16> {
    let value = buffer.get(at..at + 2).context("DOC 数据不完整")?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn le32(buffer: &[u8], at: usize) -> Result<u32> {
    let value = buffer.get(at..at + 4).context("DOC 数据不完整")?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn extract(clx: &[u8], text_units: usize, stream: &mut (impl Read + Seek)) -> Result<String> {
    let mut at = 0;
    while clx.get(at) == Some(&1) {
        at += 3 + le16(clx, at + 1)? as usize;
    }
    ensure!(clx.get(at) == Some(&2), "DOC 缺少正文分片");
    let length = le32(clx, at + 1)? as usize;
    ensure!(
        length >= 16 && (length - 4).is_multiple_of(12),
        "DOC 正文分片表格式无效"
    );
    let plc = clx
        .get(at + 5..)
        .and_then(|tail| tail.get(..length))
        .context("DOC 正文分片不完整")?;
    let pieces = (length - 4) / 12;
    let descriptor_start = (pieces + 1) * 4;
    ensure!(le32(plc, 0)? == 0, "DOC 正文起点无效");
    let mut result = Vec::with_capacity(text_units);
    let mut previous = 0;
    let mut bytes = Vec::new();
    for index in 0..pieces {
        if previous >= text_units {
            break;
        }
        let end = le32(plc, (index + 1) * 4)? as usize;
        ensure!(end > previous, "DOC 正文分片顺序无效");
        let count = end.min(text_units) - previous;
        let fc = le32(plc, descriptor_start + index * 8 + 2)?;
        let compressed = fc & 0x4000_0000 != 0;
        let offset = u64::from(fc & 0x3fff_ffff);
        let offset = if compressed { offset / 2 } else { offset };
        bytes.resize(count * if compressed { 1 } else { 2 }, 0);
        stream.seek(SeekFrom::Start(offset))?;
        stream.read_exact(&mut bytes)?;
        if compressed {
            let text = encoding_rs::WINDOWS_1252
                .decode_without_bom_handling(&bytes)
                .0;
            result.extend(text.encode_utf16());
        } else {
            result.extend(
                bytes
                    .chunks_exact(2)
                    .map(|unit| u16::from_le_bytes([unit[0], unit[1]])),
            );
        }
        previous = end;
    }
    ensure!(previous >= text_units, "DOC 正文分片缺失");
    Ok(visible_text(&String::from_utf16_lossy(&result)))
}

fn visible_text(text: &str) -> String {
    let mut fields = Vec::new();
    let mut hidden = 0;
    let mut visible = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\u{13}' => {
                fields.push(true);
                hidden += 1;
            }
            '\u{14}' => {
                if let Some(suppressed) = fields.last_mut()
                    && *suppressed
                {
                    *suppressed = false;
                    hidden -= 1;
                }
            }
            '\u{15}' => {
                if fields.pop() == Some(true) {
                    hidden -= 1;
                }
            }
            _ if hidden != 0 => {}
            '\r' | '\u{07}' => visible.push('\n'),
            '\u{0b}' => visible.push('\n'),
            '\u{0c}' | '\n' | '\t' => visible.push(ch),
            ch if !ch.is_control() => visible.push(ch),
            _ => {}
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::extract;
    use std::io::Cursor;

    #[test]
    fn reads_compressed_and_utf16_pieces_in_order() {
        let mut word = b"abc".to_vec();
        word.push(0);
        for unit in "你好\r".encode_utf16() {
            word.extend(unit.to_le_bytes());
        }
        let mut clx = vec![2];
        clx.extend(28u32.to_le_bytes());
        for cp in [0u32, 3, 6] {
            clx.extend(cp.to_le_bytes());
        }
        for offset in [0x4000_0000u32, 4u32] {
            clx.extend([0, 0]);
            clx.extend(offset.to_le_bytes());
            clx.extend([0, 0]);
        }
        assert_eq!(
            extract(&clx, 6, &mut Cursor::new(word)).unwrap(),
            "abc你好\n"
        );
    }
}
