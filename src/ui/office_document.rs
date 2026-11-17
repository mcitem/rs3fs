use super::file_viewer::OfficeSource;
use anyhow::{Context, Result, ensure};
use gpui_kit::{Image, ImageFormat};
use roxmltree::{Document, Node, ParsingOptions};
use std::{collections::HashMap, fs::File, io::Read, sync::Arc};
use zip::ZipArchive;

const XML_LIMIT: u64 = 16 * 1024 * 1024;
const IMAGE_LIMIT: u64 = 8 * 1024 * 1024;
const MEDIA_BUDGET: u64 = 24 * 1024 * 1024;

type Package = ZipArchive<File>;
type Relationships = HashMap<String, String>;

pub(super) enum OfficeDocument {
    Word(Vec<WordBlock>),
    Slides(Presentation, Slide),
}

pub(super) enum WordBlock {
    Paragraph(Paragraph),
    TableRow(Vec<String>),
    Image {
        image: Arc<Image>,
        width: f32,
        ratio: f32,
    },
    Break,
}

pub(super) struct Paragraph {
    pub text: String,
    pub emphasis: Vec<(std::ops::Range<usize>, bool, bool)>,
    pub size: f32,
    pub alignment: &'static str,
    pub bullet: bool,
    pub heading: bool,
}

pub(super) struct Presentation {
    source: OfficeSource,
    slides: Vec<String>,
    pub width: f32,
    pub height: f32,
}

pub(super) struct Slide {
    pub background: u32,
    pub shapes: Vec<SlideShape>,
}

pub(super) struct SlideShape {
    pub geometry: [f32; 4],
    pub fill: Option<u32>,
    pub text_color: u32,
    pub font_size: f32,
    pub text: String,
    pub image: Option<Arc<Image>>,
}

impl OfficeDocument {
    pub(super) fn legacy_word(path: &std::path::Path) -> Result<Self> {
        let text = super::legacy_doc::main_text(path)?;
        ensure!(
            text.bytes()
                .filter(|byte| matches!(byte, b'\n' | 12))
                .count()
                <= 100_000,
            "DOC 段落过多"
        );
        let mut blocks = Vec::new();
        for (page, text) in text.split('\u{0c}').enumerate() {
            if page != 0 {
                blocks.push(WordBlock::Break);
            }
            blocks.extend(text.split_terminator('\n').map(|line| {
                WordBlock::Paragraph(Paragraph {
                    text: line.to_owned(),
                    emphasis: Vec::new(),
                    size: 16.,
                    alignment: "left",
                    bullet: false,
                    heading: false,
                })
            }));
        }
        Ok(Self::Word(blocks))
    }

    pub(super) fn open(source: OfficeSource, extension: &str) -> Result<Self> {
        let mut package = ZipArchive::new(File::open(&source.path)?)?;
        if extension == ".docx" {
            return Ok(Self::Word(read_word(&mut package)?));
        }
        let xml = xml_part(&mut package, "ppt/presentation.xml")?;
        let doc = parse(&xml)?;
        let relationships = relationships(&mut package, "ppt/presentation.xml");
        let slide_list = doc
            .descendants()
            .find(|node| node.has_tag_name("sldIdLst"))
            .context("演示文稿没有幻灯片")?;
        let slides = children(slide_list, "sldId")
            .map(|slide| {
                let id = relationship_attr(slide, "id").context("幻灯片索引无效")?;
                relationships.get(id).cloned().context("找不到幻灯片内容")
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(!slides.is_empty(), "演示文稿没有幻灯片");
        let size = child(doc.root_element(), "sldSz");
        let dimension = |attr, fallback| {
            size.and_then(|node| positive(node.attribute(attr)))
                .unwrap_or(fallback)
        };
        let presentation = Presentation {
            source,
            slides,
            width: dimension("cx", 9_144_000.),
            height: dimension("cy", 6_858_000.),
        };
        let first = read_slide(&mut package, &presentation.slides[0])?;
        Ok(Self::Slides(presentation, first))
    }
}

impl Presentation {
    pub(super) fn len(&self) -> usize {
        self.slides.len()
    }

    pub(super) fn slide(&self, index: usize) -> Result<Slide> {
        let mut package = ZipArchive::new(File::open(&self.source.path)?)?;
        read_slide(&mut package, &self.slides[index])
    }
}

fn read_part(package: &mut Package, path: &str, limit: u64) -> Result<Vec<u8>> {
    let part = package
        .by_name(path)
        .with_context(|| format!("缺少文档内容 {path}"))?;
    ensure!(part.size() <= limit, "文档内容超出预览大小上限");
    let mut bytes = Vec::with_capacity(part.size() as usize);
    part.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "文档内容超出预览大小上限");
    Ok(bytes)
}

fn xml_part(package: &mut Package, path: &str) -> Result<String> {
    let bytes = read_part(package, path, XML_LIMIT)?;
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little_endian = bytes[0] == 0xff;
        ensure!(bytes.len().is_multiple_of(2), "无效的 UTF-16 文档内容");
        let units: Vec<_> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| {
                let pair = [pair[0], pair[1]];
                if little_endian {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            })
            .collect();
        Ok(String::from_utf16(&units)?)
    } else {
        Ok(String::from_utf8(bytes)?)
    }
}

fn parse(xml: &str) -> Result<Document<'_>> {
    Ok(Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 500_000,
        },
    )?)
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    children(node, name).next()
}

fn children<'a, 'input>(
    node: Node<'a, 'input>,
    name: &str,
) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children()
        .filter(move |child| child.has_tag_name(name))
}

fn attribute<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|attr| attr.name() == name)
        .map(|attr| attr.value())
}

fn relationship_attr<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|attr| {
            attr.name() == name
                && attr
                    .namespace()
                    .is_some_and(|ns| ns.ends_with("/relationships"))
        })
        .map(|attr| attr.value())
}

fn positive(value: Option<&str>) -> Option<f32> {
    value?
        .parse::<f32>()
        .ok()
        .filter(|size| size.is_finite() && *size > 0.)
}

fn relationships(package: &mut Package, part: &str) -> Relationships {
    let (folder, name) = part.rsplit_once('/').unwrap_or(("", part));
    let rels = format!("{folder}/_rels/{name}.rels");
    let Ok(xml) = xml_part(package, &rels) else {
        return HashMap::new();
    };
    let Ok(doc) = parse(&xml) else {
        return HashMap::new();
    };
    children(doc.root_element(), "Relationship")
        .filter_map(|node| {
            if node
                .attribute("TargetMode")
                .is_some_and(|mode| mode.eq_ignore_ascii_case("External"))
            {
                return None;
            }
            let target = node.attribute("Target")?;
            if target.contains([':', '\\', '#', '?']) {
                return None;
            }
            let mut parts: Vec<_> = if target.starts_with('/') {
                Vec::new()
            } else {
                folder.split('/').collect()
            };
            for part in target.split('/') {
                match part {
                    "" | "." => {}
                    ".." => {
                        parts.pop()?;
                    }
                    part => parts.push(part),
                }
            }
            Some((node.attribute("Id")?.to_owned(), parts.join("/")))
        })
        .collect()
}

fn load_image(package: &mut Package, path: &str, budget: &mut u64) -> Option<Arc<Image>> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    let format = match extension.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "bmp" => ImageFormat::Bmp,
        _ => return None,
    };
    let bytes = read_part(package, path, IMAGE_LIMIT.min(*budget)).ok()?;
    // Inspect dimensions before handing compressed media to the UI decoder.
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    let pixels = u64::from(width) * u64::from(height);
    let cost = (bytes.len() as u64).max(pixels.saturating_mul(4));
    if cost > *budget {
        return None;
    }
    *budget -= cost;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

fn is_enabled(node: Option<Node<'_, '_>>) -> bool {
    node.is_some_and(|node| !matches!(attribute(node, "val"), Some("0" | "false" | "off")))
}

fn word_paragraph(node: Node<'_, '_>) -> Paragraph {
    let props = child(node, "pPr");
    let style = props
        .and_then(|p| child(p, "pStyle"))
        .and_then(|s| attribute(s, "val"))
        .unwrap_or("");
    let heading =
        style.to_ascii_lowercase().starts_with("heading") || style.eq_ignore_ascii_case("title");
    let mut text = String::new();
    let mut emphasis = Vec::new();
    let mut size: f32 = if heading { 23. } else { 16. };
    for run in node.descendants().filter(|n| n.has_tag_name("r")) {
        let start = text.len();
        for content in run.children() {
            match content.tag_name().name() {
                "t" => text.push_str(content.text().unwrap_or("")),
                "tab" => text.push_str("    "),
                "br" if attribute(content, "type") != Some("page") => text.push('\n'),
                _ => {}
            }
        }
        let props = child(run, "rPr");
        if let Some(props) = props {
            let bold = is_enabled(child(props, "b"));
            let italic = is_enabled(child(props, "i"));
            if (bold || italic) && text.len() > start {
                emphasis.push((start..text.len(), bold, italic));
            }
            if start == 0
                && let Some(points) = child(props, "sz").and_then(|n| positive(attribute(n, "val")))
            {
                size = (points * 2. / 3.).clamp(9., 80.);
            }
        }
    }
    let alignment = match props
        .and_then(|p| child(p, "jc"))
        .and_then(|n| attribute(n, "val"))
    {
        Some("center") => "center",
        Some("right" | "end") => "right",
        _ => "left",
    };
    Paragraph {
        text,
        emphasis,
        size,
        alignment,
        heading,
        bullet: props.is_some_and(|p| child(p, "numPr").is_some()),
    }
}

fn read_word(package: &mut Package) -> Result<Vec<WordBlock>> {
    let path = "word/document.xml";
    let xml = xml_part(package, path)?;
    let doc = parse(&xml)?;
    let rels = relationships(package, path);
    let body = child(doc.root_element(), "body").context("Word 文档缺少正文")?;
    let mut blocks = Vec::new();
    let mut budget = MEDIA_BUDGET;
    for node in body.children().filter(Node::is_element) {
        match node.tag_name().name() {
            "p" => {
                let paragraph = word_paragraph(node);
                let mut images = node
                    .descendants()
                    .filter(|n| n.has_tag_name("drawing"))
                    .filter_map(|drawing| {
                        let blip = drawing
                            .descendants()
                            .find(|part| part.has_tag_name("blip"))?;
                        let path = rels.get(relationship_attr(blip, "embed")?)?;
                        let extent = drawing
                            .descendants()
                            .find(|part| part.has_tag_name("extent"));
                        let width = extent
                            .and_then(|part| positive(part.attribute("cx")))
                            .map_or(384., |emu| (emu / 9525.).clamp(24., 1024.));
                        let ratio = extent
                            .and_then(|part| {
                                Some(
                                    positive(part.attribute("cx"))?
                                        / positive(part.attribute("cy"))?,
                                )
                            })
                            .unwrap_or(4. / 3.)
                            .clamp(0.1, 10.);
                        let image = load_image(package, path, &mut budget)?;
                        Some(WordBlock::Image {
                            image,
                            width: width.min(560. * ratio),
                            ratio,
                        })
                    })
                    .peekable();
                if !paragraph.text.is_empty() || images.peek().is_none() {
                    blocks.push(WordBlock::Paragraph(paragraph));
                }
                blocks.extend(images);
                if node
                    .descendants()
                    .any(|n| n.has_tag_name("br") && attribute(n, "type") == Some("page"))
                {
                    blocks.push(WordBlock::Break);
                }
            }
            "tbl" => {
                blocks.extend(children(node, "tr").map(|row| {
                    WordBlock::TableRow(
                        children(row, "tc")
                            .map(|cell| {
                                children(cell, "p")
                                    .map(|p| word_paragraph(p).text)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .collect(),
                    )
                }));
            }
            _ => {}
        }
    }
    Ok(blocks)
}

fn geometry(shape: Node<'_, '_>) -> Option<[f32; 4]> {
    let transform = child(shape, "spPr")
        .and_then(|props| child(props, "xfrm"))
        .or_else(|| child(shape, "xfrm"))?;
    let offset = child(transform, "off")?;
    let size = child(transform, "ext")?;
    let pos = |attr| {
        offset
            .attribute(attr)?
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
    };
    Some([
        pos("x")?,
        pos("y")?,
        positive(size.attribute("cx"))?,
        positive(size.attribute("cy"))?,
    ])
}

fn placeholder<'a>(shape: Node<'a, '_>) -> Option<(Option<&'a str>, Option<&'a str>)> {
    let node = shape.descendants().find(|node| node.has_tag_name("ph"))?;
    Some((node.attribute("idx"), node.attribute("type")))
}

fn color(node: Node<'_, '_>) -> Option<u32> {
    let value = child(child(node, "solidFill")?, "srgbClr")?.attribute("val")?;
    (value.len() == 6)
        .then(|| u32::from_str_radix(value, 16).ok())
        .flatten()
}

fn read_slide(package: &mut Package, path: &str) -> Result<Slide> {
    let xml = xml_part(package, path)?;
    let doc = parse(&xml)?;
    let rels = relationships(package, path);
    let layout_path = rels
        .values()
        .find(|path| path.starts_with("ppt/slideLayouts/") && path.ends_with(".xml"));
    let layout_xml = layout_path.and_then(|path| xml_part(package, path).ok());
    let layout_doc = layout_xml.as_deref().and_then(|xml| parse(xml).ok());
    let master_path = layout_path.and_then(|path| {
        relationships(package, path)
            .into_values()
            .find(|path| path.starts_with("ppt/slideMasters/") && path.ends_with(".xml"))
    });
    let master_xml = master_path.and_then(|path| xml_part(package, &path).ok());
    let master_doc = master_xml.as_deref().and_then(|xml| parse(xml).ok());
    let content = child(doc.root_element(), "cSld").context("幻灯片缺少内容")?;
    let tree = child(content, "spTree").context("幻灯片缺少布局")?;
    let background = child(content, "bg")
        .and_then(|bg| child(bg, "bgPr"))
        .and_then(color)
        .unwrap_or(0xffffff);
    let mut budget = MEDIA_BUDGET / 2;
    let mut shapes = Vec::new();
    for shape in tree.children().filter(Node::is_element) {
        if !matches!(shape.tag_name().name(), "sp" | "pic" | "graphicFrame") {
            continue;
        }
        let ph = placeholder(shape);
        let inherited = |doc: &Document| -> Option<[f32; 4]> {
            let (index, kind) = ph?;
            doc.descendants()
                .filter(|n| n.has_tag_name("sp"))
                .find(|&node| {
                    placeholder(node).is_some_and(|(candidate_index, candidate_kind)| {
                        (index.is_some() && index == candidate_index)
                            || (index.is_none() && kind.is_some() && kind == candidate_kind)
                    })
                })
                .and_then(geometry)
        };
        let is_title = ph.is_some_and(|(_, kind)| matches!(kind, Some("title" | "ctrTitle")));
        let rect = geometry(shape)
            .or_else(|| layout_doc.as_ref().and_then(inherited))
            .or_else(|| master_doc.as_ref().and_then(inherited));
        let paragraphs = shape
            .descendants()
            .filter(|n| n.has_tag_name("p"))
            .map(|paragraph| {
                paragraph
                    .descendants()
                    .filter(|n| n.has_tag_name("t"))
                    .filter_map(|n| n.text())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let text = paragraphs.join("\n");
        let image = shape
            .descendants()
            .find(|n| n.has_tag_name("blip"))
            .and_then(|n| rels.get(relationship_attr(n, "embed")?))
            .and_then(|path| load_image(package, path, &mut budget));
        let fill = child(shape, "spPr").and_then(color);
        if text.is_empty() && image.is_none() && fill.is_none() {
            continue;
        }
        let direct = shape.descendants().find(|n| n.has_tag_name("rPr"));
        let font_size = direct
            .and_then(|n| positive(n.attribute("sz")))
            .map(|size| (size / 100.).clamp(6., 200.))
            .unwrap_or(if is_title { 32. } else { 18. });
        let text_color = direct.and_then(color).unwrap_or(0x202020);
        let geometry = rect.unwrap_or_else(|| {
            if is_title {
                [500_000., 350_000., 8_000_000., 800_000.]
            } else {
                [
                    650_000.,
                    1_350_000. + shapes.len() as f32 * 600_000.,
                    7_800_000.,
                    1_500_000.,
                ]
            }
        });
        shapes.push(SlideShape {
            geometry,
            fill,
            text_color,
            font_size,
            text,
            image,
        });
    }
    Ok(Slide { background, shapes })
}
