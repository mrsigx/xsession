//! 2.3.0 `package_units`: text units of an Office Open XML package in document order — Word
//! paragraphs; Excel shared strings, then inline and formula strings per sheet.

use crate::xml::{fromstring, Element};
use crate::zip::{PkgErr, Zip};

const PACKAGE_PART_LIMIT: u64 = 64 * 1024 * 1024;
const WORD: &str = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}";
const SHEET: &str = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}";

fn part(package: &Zip, name: &str) -> Result<Element, PkgErr> {
    let info = package.getinfo(name)?;
    if info.file_size > PACKAGE_PART_LIMIT {
        return Err(PkgErr { class: "ValueError", msg: format!("{} exceeds the extraction limit", name) });
    }
    let data = package.read(info)?;
    fromstring(&data).map_err(|e| PkgErr { class: e.class, msg: e.msg })
}

/// `''.join(t.text or '' for t in elem.iter(tag))`.
fn joined_text(elem: &Element, tag: &str) -> String {
    let mut found = Vec::new();
    elem.iter_tag(tag, &mut found);
    found.iter().filter_map(|t| t.text.as_deref()).collect()
}

pub fn package_units(form: &str, data: &[u8]) -> Result<Vec<String>, PkgErr> {
    let package = Zip::open(data)?;
    let mut units = Vec::new();
    if form == "docx" {
        let document = part(&package, "word/document.xml")?;
        let mut paragraphs = Vec::new();
        document.iter_tag(&format!("{}p", WORD), &mut paragraphs);
        let t = format!("{}t", WORD);
        for p in paragraphs {
            units.push(joined_text(p, &t));
        }
    } else {
        let t = format!("{}t", SHEET);
        let names: Vec<String> = package.namelist().into_iter().map(str::to_string).collect();
        if names.iter().any(|n| n == "xl/sharedStrings.xml") {
            let shared = part(&package, "xl/sharedStrings.xml")?;
            let mut items = Vec::new();
            shared.iter_tag(&format!("{}si", SHEET), &mut items);
            for item in items {
                units.push(joined_text(item, &t));
            }
        }
        let mut sheets: Vec<&String> = names.iter().filter(|n| n.starts_with("xl/worksheets/") && n.ends_with(".xml")).collect();
        sheets.sort();
        let c = format!("{}c", SHEET);
        let v = format!("{}v", SHEET);
        for name in sheets {
            let sheet = part(&package, name)?;
            let mut cells = Vec::new();
            sheet.iter_tag(&c, &mut cells);
            for cell in cells {
                match cell.get("t") {
                    Some("inlineStr") => units.push(joined_text(cell, &t)),
                    Some("str") => {
                        if let Some(value) = cell.find(&v) {
                            units.push(value.text.clone().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    units.retain(|u| !u.is_empty());
    Ok(units)
}
