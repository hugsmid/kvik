//! The little XML the client reads (a listing and an error) and the fake
//! writes. Not a general parser: elements are found by name, without
//! attributes or namespaces mattering.

/// The text of every element `<name>…</name>`, unescaped, in order.
pub(crate) fn elements(doc: &str, name: &str) -> Result<Vec<String>, String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let j = after
            .find(&close)
            .ok_or_else(|| format!("unclosed <{name}>"))?;
        out.push(unescape(&after[..j])?);
        rest = &after[j + close.len()..];
    }
    Ok(out)
}

/// The text of the first element `<name>`, if any.
pub(crate) fn element(doc: &str, name: &str) -> Result<Option<String>, String> {
    Ok(elements(doc, name)?.into_iter().next())
}

/// Replaces the five predefined entities and numeric character references.
pub(crate) fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let j = after.find(';').ok_or("unterminated entity")?;
        let entity = &after[..j];
        let c = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(h) = entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = entity.strip_prefix('#') {
                    d.parse().ok()
                } else {
                    None
                };
                code.and_then(char::from_u32)
                    .ok_or_else(|| format!("unknown entity &{entity};"))?
            }
        };
        out.push(c);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Escapes text for an element's content. Control characters, which XML 1.0
/// cannot carry even as references, are written as references all the same,
/// which this module reads back.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 => out.push_str(&format!("&#x{:X};", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn reads_a_listing() {
        let doc = "<?xml version=\"1.0\"?><ListBucketResult xmlns=\"x\"><Name>b</Name>\
                   <IsTruncated>true</IsTruncated><Contents><Key>a&amp;b</Key><Size>3</Size>\
                   </Contents><Contents><Key>c&#x3C;d&#62;</Key></Contents></ListBucketResult>";
        assert_eq!(elements(doc, "Key").unwrap(), vec!["a&b", "c<d>"]);
        assert_eq!(
            element(doc, "IsTruncated").unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(element(doc, "Missing").unwrap(), None);
    }

    #[test]
    fn refuses_broken_input() {
        assert!(elements("<Key>a", "Key").is_err());
        assert!(unescape("a&b").is_err());
        assert!(unescape("&nope;").is_err());
        assert!(unescape("&#xD800;").is_err());
    }

    proptest! {
        #[test]
        fn escape_round_trips(s in "\\PC*") {
            prop_assert_eq!(unescape(&escape(&s)).unwrap(), s.clone());
            let doc = format!("<Key>{}</Key>", escape(&s));
            prop_assert_eq!(elements(&doc, "Key").unwrap(), vec![s]);
        }
    }
}
