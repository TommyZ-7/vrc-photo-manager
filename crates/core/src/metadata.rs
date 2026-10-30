use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Metadata that VRChat writes into the XMP packet of a photo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XmpMetadata {
    pub world_name: Option<String>,
    pub world_id: Option<String>,
    pub author: Option<String>,
}

impl XmpMetadata {
    pub fn is_empty(&self) -> bool {
        self.world_name.is_none() && self.world_id.is_none() && self.author.is_none()
    }
}

/// Reads the XMP packet from a PNG file.
///
/// VRChat stores XMP in an iTXt chunk named `XML:com.adobe.xmp`. Some tools use
/// tEXt/zTXt instead, so all three are accepted. Non-PNG files and malformed
/// PNGs return `None` rather than failing photo indexing.
pub fn read_xmp(path: &Path) -> Result<Option<XmpMetadata>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let decoder = png::Decoder::new(BufReader::new(file));
    let reader = match decoder.read_info() {
        Ok(reader) => reader,
        Err(_) => return Ok(None),
    };
    let info = reader.info();

    let mut packet = None;
    for chunk in &info.utf8_text {
        if is_xmp_keyword(&chunk.keyword)
            && let Ok(text) = chunk.get_text()
        {
            packet = Some(text);
            break;
        }
    }
    if packet.is_none() {
        for chunk in &info.compressed_latin1_text {
            if is_xmp_keyword(&chunk.keyword)
                && let Ok(text) = chunk.get_text()
            {
                packet = Some(text);
                break;
            }
        }
    }
    if packet.is_none() {
        for chunk in &info.uncompressed_latin1_text {
            if is_xmp_keyword(&chunk.keyword) {
                packet = Some(chunk.text.clone());
                break;
            }
        }
    }

    Ok(packet.and_then(|packet| parse_xmp(&packet)))
}

fn is_xmp_keyword(keyword: &str) -> bool {
    keyword.eq_ignore_ascii_case("XML:com.adobe.xmp") || keyword.eq_ignore_ascii_case("xmp")
}

/// Extracts the fields VRChat stores as RDF attributes.
pub fn parse_xmp(xml: &str) -> Option<XmpMetadata> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut metadata = XmpMetadata::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                for attribute in element.attributes().flatten() {
                    let key = attribute.key.local_name();
                    let key = Some(key.as_ref());
                    let value = attribute
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .ok()?
                        .into_owned();
                    match key {
                        Some("WorldDisplayName") => metadata.world_name = Some(value),
                        Some("WorldId") => metadata.world_id = Some(value),
                        Some("Author") => metadata.author = Some(value),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    if metadata.is_empty() {
        None
    } else {
        Some(metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XMP: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description xmlns:vrc="https://vrchat.com/xmp/1.0/"
      vrc:WorldDisplayName="Midnight Rooftop"
      vrc:WorldId="wrld_test"
      Author="TommyZ" />
  </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

    #[test]
    fn parses_vrchat_xmp_attributes() {
        let metadata = parse_xmp(XMP).expect("metadata");
        assert_eq!(metadata.world_name.as_deref(), Some("Midnight Rooftop"));
        assert_eq!(metadata.world_id.as_deref(), Some("wrld_test"));
        assert_eq!(metadata.author.as_deref(), Some("TommyZ"));
    }

    #[test]
    fn reads_itxt_packet_from_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xmp.png");
        {
            let file = File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .add_itxt_chunk("XML:com.adobe.xmp".to_string(), XMP.to_string())
                .unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        }
        let metadata = read_xmp(&path).unwrap().expect("xmp");
        assert_eq!(metadata.world_name.as_deref(), Some("Midnight Rooftop"));
        assert_eq!(metadata.author.as_deref(), Some("TommyZ"));
    }
}
