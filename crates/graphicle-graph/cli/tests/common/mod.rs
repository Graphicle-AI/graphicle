//! Helpers the engine program's tests share: a Word document built in the
//! test, so no binary file is checked in.

/// CRC-32 (IEEE), as a zip entry records it.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A zip archive of `files`, stored without compression.
pub fn zip(files: &[(&str, &str)]) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, text) in files {
        let (name, data) = (name.as_bytes(), text.as_bytes());
        let (crc, size, offset) = (crc32(data), data.len() as u32, out.len() as u32);
        // version 20, no flags, stored, 1980-01-01 00:00
        let fields = |head: &mut Vec<u8>| {
            for half in [20u16, 0, 0, 0, 0x21] {
                head.extend_from_slice(&half.to_le_bytes());
            }
            for word in [crc, size, size] {
                head.extend_from_slice(&word.to_le_bytes());
            }
            head.extend_from_slice(&(name.len() as u16).to_le_bytes());
            head.extend_from_slice(&0u16.to_le_bytes());
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        fields(&mut out);
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        fields(&mut central);
        // comment length, disk, internal and external attributes, offset
        central.extend_from_slice(&[0; 10]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let (start, length) = (out.len() as u32, central.len() as u32);
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for count in [files.len() as u16; 2] {
        out.extend_from_slice(&count.to_le_bytes());
    }
    for word in [length, start] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&[0; 2]);
    out
}

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// A Word document of `paragraphs`, each with an optional style
/// (`Heading1`, `Heading2`) and its text.
pub fn word_document(paragraphs: &[(Option<&str>, &str)]) -> Vec<u8> {
    let paragraph = |style: Option<&str>, text: &str| {
        let style = style.map_or(String::new(), |s| {
            format!("<w:pPr><w:pStyle w:val=\"{s}\"/></w:pPr>")
        });
        format!("<w:p>{style}<w:r><w:t>{text}</w:t></w:r></w:p>")
    };
    let body = paragraphs
        .iter()
        .map(|(style, text)| paragraph(*style, text))
        .collect::<String>();
    let document = format!("<w:document xmlns:w=\"{W}\"><w:body>{body}</w:body></w:document>");
    let styles = format!(
        "<w:styles xmlns:w=\"{W}\">\
         <w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/></w:style>\
         </w:styles>"
    );
    zip(&[
        ("word/document.xml", &document),
        ("word/styles.xml", &styles),
    ])
}
